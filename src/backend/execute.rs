//! Statement execution for the Trino backend: `exec_direct`, `prepare` and
//! `execute`, plus the [`StatementBackend`] implementation that streams result
//! rows back through the shared `stackable-odbc-core` fetch path.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use stackable_odbc_core::backend::StatementBackend;
use stackable_odbc_core::errors::OdbcError;
use stackable_odbc_core::types::{
    CDataType, ColumnDescriptor, ColumnValue, ExecuteOutcome, FetchResult, SqlState,
};
use trino_rust_client::{Row, TrinoTy};

use super::info::trino_bare_type_name;
use super::{
    TrinoCancelToken, TrinoConnection, TrinoError, TrinoStatement, log_page_stats, map_trino_error,
    map_trino_error_on,
};
use crate::type_conversion::{
    TrinoTypeName, json_to_column_value, trino_ty_precision, trino_ty_scale, trino_ty_to_sql_type,
    type_name_precision, type_name_scale,
};

/// Convert rows from a Trino response page into `Vec<Vec<ColumnValue>>`.
///
/// Takes ownership of the page data to avoid cloning every `Row`. The caller
/// must extract `next_uri`, `stats`, `error`, etc. from the page before
/// passing `page.data` here.
///
/// A page the coordinator spooled cannot be decoded from the page alone — its
/// segments may live in object storage — so `into_vec` reports
/// `Error::Protocol` for one. This driver never advertises a spooling encoding,
/// so the coordinator has no reason to send one; the error is returned rather
/// than swallowed because an empty batch would reach the application as an
/// empty result set.
fn convert_rows(
    data: Option<trino_rust_client::models::QueryResultData<Row>>,
    types: &[(String, TrinoTy)],
) -> Result<Vec<Vec<ColumnValue>>, trino_rust_client::error::Error> {
    let rows = match data {
        Some(d) => d.into_vec()?,
        None => return Ok(Vec::new()),
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            row.into_json()
                .into_iter()
                .zip(types.iter())
                .map(|(val, (_, ty))| json_to_column_value(val, ty))
                .collect()
        })
        .collect())
}

/// Remove the statement terminator an application left on the end.
///
/// Trino's REST API takes one statement per request and its grammar has no
/// terminator, so a trailing `;` is a syntax error rather than a no-op:
/// `SELECT 1;` fails with `SYNTAX_ERROR` at the semicolon's own column. ODBC
/// applications and query tools routinely send one — `isql` submits the line as
/// typed, and SQL editors commonly append it — so a driver that passes it
/// through rejects statements every other client accepts.
///
/// Only the *trailing* run is removed, after trailing whitespace. That is
/// enough to be safe without parsing: a statement whose last token is a string
/// literal or quoted identifier ends with the closing quote, so a semicolon
/// inside one is never the final character and never seen here. An embedded
/// semicolon is left alone, and Trino rejects it — correctly, since it does not
/// accept multiple statements per request.
///
/// A comment *after* the terminator (`SELECT 1; -- done`) is not handled: the
/// trailing character is then the comment, not the semicolon. Recognising it
/// would mean parsing comments, which is a larger change than the case
/// justifies.
fn strip_trailing_semicolons(sql: &str) -> &str {
    let mut trimmed = sql.trim_end();
    while let Some(rest) = trimmed.strip_suffix(';') {
        trimmed = rest.trim_end();
    }
    trimmed
}

pub(super) fn exec_direct(
    conn: &TrinoConnection,
    cancel: &TrinoCancelToken,
    sql: &str,
) -> Result<TrinoStatement, TrinoError> {
    // Every path carrying application SQL funnels through here -- `execute`
    // calls this after interpolating parameters -- so this is the one place the
    // terminator has to be dropped.
    let stripped = strip_trailing_semicolons(sql);
    if stripped.len() != sql.len() {
        tracing::debug!("stripped a trailing statement terminator; Trino's grammar has none");
    }
    let sql = stripped;
    tracing::debug!(%sql, "TrinoBackend::exec_direct");

    // Submit the query to Trino.
    let submit_start = Instant::now();
    let mut page = {
        let _span = tracing::info_span!("trino.submit").entered();
        conn.runtime
            .block_on(conn.client.get::<Row>(sql.to_string()))
            .map_err(|e| map_trino_error_on(&conn.liveness, e))?
    };
    let submit_elapsed = submit_start.elapsed();

    // Publish the id before polling for metadata rather than after. A query
    // that is queued or planning emits metadata-less pages for as long as the
    // coordinator is busy, and that wait is precisely when an application
    // reaches for `SQLCancel`; recording the id only once the loop below breaks
    // would leave the whole wait uncancellable.
    cancel.state.begin_query(page.id.clone());

    log_page_stats(&page.stats, 1);

    // Poll through initial pages until we get column metadata.
    let mut page_count: u32 = 1;
    let mut empty_page_count: u32 = 0;
    let mut total_fetch_time = submit_elapsed;

    if page.columns.is_none() {
        let _span = tracing::info_span!("trino.poll_metadata").entered();
        while page.columns.is_none() {
            if let Some(error) = page.error.take() {
                return Err(map_trino_error_on(&conn.liveness, error.into()));
            }
            let next_url = page.next_uri.as_ref().ok_or_else(|| TrinoError::General {
                message: "query returned no columns and no next page".into(),
            })?;

            empty_page_count += 1;
            let fetch_start = Instant::now();
            page = conn
                .runtime
                .block_on(conn.client.get_next(next_url))
                .map_err(|e| map_trino_error_on(&conn.liveness, e))?;
            total_fetch_time += fetch_start.elapsed();
            page_count += 1;

            log_page_stats(&page.stats, page_count);
        }
        tracing::info!(empty_pages = empty_page_count, "metadata polling complete");
    }

    if let Some(error) = page.error.take() {
        return Err(map_trino_error_on(&conn.liveness, error.into()));
    }

    // Extract column metadata.
    //
    // `Column` carries the real name and Trino's native type text
    // ("varchar(50)", "decimal(10,2)"). `TrinoTy::from_column` consumes the
    // `Column` and discards both (including the varchar length, which
    // `RawTrinoTy::VarChar` drops entirely), so destructure first and derive
    // size/scale from the native type text via the same parser the catalog
    // path (`SQLColumns`) uses. `TrinoTy` remains the fallback for a type
    // string the parser does not recognise (e.g. compound types).
    let raw_columns = page.columns.take().unwrap_or_default();

    let mut trino_types: Vec<(String, TrinoTy)> = Vec::with_capacity(raw_columns.len());
    let mut columns: Vec<ColumnDescriptor> = Vec::with_capacity(raw_columns.len());

    for column in raw_columns {
        let native_name = column.ty.clone();
        let column_name = column.name.clone();
        let ty = match TrinoTy::from_column(column) {
            Ok((_, ty)) => ty,
            Err(error) => {
                tracing::warn!(
                    column = %column_name,
                    trino_type = %native_name,
                    %error,
                    "could not parse Trino type signature; describing column as unknown"
                );
                TrinoTy::Unknown
            }
        };

        let sql_type = TrinoTypeName::parse(&native_name)
            .map(|t| t.sql_type())
            .unwrap_or_else(|| trino_ty_to_sql_type(&ty));

        let precision = type_name_precision(&native_name)
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or_else(|| trino_ty_precision(&ty));
        let scale = type_name_scale(&native_name)
            .and_then(|s| i16::try_from(s).ok())
            .unwrap_or_else(|| trino_ty_scale(&ty));

        // Nullability is left at `ColumnDescriptor::new`'s
        // `SQL_NULLABLE_UNKNOWN`. Trino's REST protocol describes a result
        // column with a name and a type and nothing else, so this driver cannot
        // determine whether a column accepts NULL -- and the ODBC spec defines
        // the third value for exactly that case. Claiming `SQL_NULLABLE`
        // instead would be a guess that happens to be safe for a projection of
        // a nullable base column and wrong for a `COUNT(*)`; claiming
        // `SQL_NO_NULLS` would tell an application it may skip a NULL check it
        // needs.
        columns.push(
            ColumnDescriptor::new(column_name.clone(), sql_type)
                // Spec (SQL_DESC_TYPE_NAME / SQLColumns.TYPE_NAME): both list
                // bare examples ("CHAR", "VARCHAR", ...), not parameterised
                // declarations: "varchar(50)" is a *declaration*, and matches
                // no `SQLGetTypeInfo` row. `trino_bare_type_name` returns the
                // bare name that does (see its doc comment in
                // `backend/info.rs`); precision/scale still come from
                // `native_name`, so the declared length is not lost, only
                // moved out of the name.
                .with_type_name(trino_bare_type_name(&native_name, sql_type))
                .with_precision_scale(precision, scale),
        );
        trino_types.push((column_name, ty));
    }

    // Extract fields before consuming page.data for conversion.
    let next_uri = page.next_uri;
    let query_id = page.id;

    // Convert the first batch of rows (if any).
    let convert_start = Instant::now();
    let batch = {
        let _span = tracing::info_span!("trino.convert_batch", page = page_count).entered();
        convert_rows(page.data, &trino_types).map_err(|e| map_trino_error_on(&conn.liveness, e))?
    };
    let total_convert_time = convert_start.elapsed();
    let total_rows_fetched = batch.len() as u64;

    tracing::debug!(
        columns = columns.len(),
        batch_rows = batch.len(),
        has_next = next_uri.is_some(),
        "exec_direct: first page with columns received"
    );

    Ok(TrinoStatement {
        pending_sql: None,
        columns,
        trino_types,
        batch,
        batch_cursor: 0,
        fetch_failed: false,
        next_uri,
        query_id: Some(query_id),
        client: Some(Arc::clone(&conn.client)),
        runtime: Some(Arc::clone(&conn.runtime)),
        cancel_state: Some(Arc::clone(&cancel.state)),
        liveness: Some(conn.liveness.clone()),
        page_count,
        empty_page_count,
        total_rows_fetched,
        total_fetch_time,
        total_convert_time,
    })
}

/// Store the SQL for a later `SQLExecute`.
///
/// The cancel token is untouched: preparing runs no query on the coordinator,
/// so there is nothing yet for `SQLCancel` to name. `execute` fills the token's
/// slot when it submits.
pub(super) fn prepare(
    _conn: &TrinoConnection,
    _cancel: &TrinoCancelToken,
    sql: &str,
) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(sql, "TrinoBackend::prepare");
    Ok(TrinoStatement {
        pending_sql: Some(sql.to_string()),
        columns: Vec::new(),
        trino_types: Vec::new(),
        batch: Vec::new(),
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        liveness: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

pub(super) fn execute(
    conn: &TrinoConnection,
    cancel: &TrinoCancelToken,
    stmt: &mut TrinoStatement,
    params: &[ColumnValue],
) -> Result<ExecuteOutcome, TrinoError> {
    // Cloned rather than taken: the template is needed again to re-execute the
    // same prepared statement with different parameter values, which is the
    // main reason to prepare in the first place.
    let template = stmt
        .pending_sql
        .clone()
        .ok_or_else(|| TrinoError::General {
            message: "execute called without a prepared statement".into(),
        })?;

    // Trino has no wire-level parameter binding, so bound values are rendered
    // into the SQL as literals. See `super::params` for the escaping rules.
    let sql = super::params::interpolate_params(&template, params)?;
    tracing::debug!(sql, "TrinoBackend::execute");

    let mut result = exec_direct(conn, cancel, &sql)?;
    // Swap into the existing statement handle. The old `stmt` fields are moved
    // into `result` which is then dropped; its Drop impl will drain any
    // residual pages from the *previous* query.
    std::mem::swap(stmt, &mut result);
    // exec_direct returns a statement with no pending SQL; restore the template
    // so the handle stays re-executable.
    stmt.pending_sql = Some(template);
    // Trino has no wire-level parameter binding and therefore no output params.
    Ok(ExecuteOutcome::default())
}

/// Cancel a running Trino query via the REST API.
///
/// Called by `SQLCancel`, possibly from a thread holding no lock on the
/// connection while another thread executes on the same statement. Everything
/// this needs is therefore reached through the token: the statement itself may
/// be under `&mut` on the other thread and is not touchable from here.
pub(super) fn cancel(token: &TrinoCancelToken) -> Result<(), TrinoError> {
    // Taken, not read: a second SQLCancel for the same query has nothing left
    // to do, and Trino answers a DELETE for an already-cancelled query with an
    // error that is not worth surfacing.
    let query_id = match token.state.query_id.lock() {
        Ok(mut slot) => slot.take(),
        // A poisoned lock is an internal invariant violation rather than a
        // client failure, so it is hand-built rather than routed through
        // `map_trino_error` (see AGENTS.md), and built as an `OdbcError` so its
        // SQLSTATE and message reach `SQLGetDiagRec` unchanged -- a
        // `TrinoError::Odbc` is unwrapped, not re-mapped.
        Err(_) => {
            return Err(TrinoError::from(OdbcError::general(
                "cancel state was poisoned by an earlier panic",
                SqlState::general_error(),
            )));
        }
    };

    let Some(query_id) = query_id else {
        tracing::debug!("SQLCancel: no query ID available (query may be finished)");
        return Ok(());
    };

    tracing::debug!(query_id = %query_id, "cancelling Trino query");

    token
        .runtime
        .block_on(token.client.cancel(&query_id))
        .map_err(|e| map_trino_error_on(&token.liveness, e))?;

    // Publish the cancellation so `fetch`, `close_cursor` and `Drop` stay off
    // `next_uri`: after a server-side cancel, `get_next` fails and leaves the
    // pooled TCP socket carrying residual bytes, corrupting subsequent queries
    // that reuse it from the same reqwest pool.
    //
    // The flag is set only once the DELETE has succeeded. A failed cancel means
    // the query is still running server-side, so the result set is still
    // legitimately drainable and suppressing the drain would strand it.
    //
    // The pooled connection may still carry unread response bytes for the
    // request that was in flight when the cancel landed. reqwest evicts such a
    // socket on its idle timeout (90s).
    token.state.cancelled.store(true, Ordering::SeqCst);

    Ok(())
}

impl TrinoStatement {
    /// Whether a `SQLCancel` on another thread has already stopped this
    /// statement's query server-side.
    ///
    /// `false` for a statement built without a cancel state — the in-memory
    /// catalog results, which hold no `next_uri` and so have nothing to stop.
    fn is_cancelled(&self) -> bool {
        self.cancel_state
            .as_ref()
            .is_some_and(|state| state.is_cancelled())
    }

    /// Classify a client error, recording a lost link on the connection this
    /// statement came from.
    ///
    /// A page fetch is the most likely place a connection failure is first
    /// seen, and `SQL_ATTR_CONNECTION_DEAD` is a fact about the connection
    /// rather than about the statement — so the observation has to travel back.
    /// Falls back to the bare mapper for a statement built without a liveness
    /// handle, which is the in-memory catalog results that reach no network.
    fn map_client_error(&self, e: trino_rust_client::error::Error) -> TrinoError {
        match &self.liveness {
            Some(liveness) => map_trino_error_on(liveness, e),
            None => map_trino_error(e),
        }
    }

    /// Discard the result set after a failed page fetch and return `err`.
    ///
    /// The rows of the last successfully fetched page are still buffered when a
    /// fetch fails. Leaving them in place would let `SQLGetData` keep returning
    /// data for a row the application was told it never received, so the batch
    /// is dropped, the cursor is rewound and the statement is marked failed.
    fn abandon_result_set(&mut self, err: TrinoError) -> TrinoError {
        tracing::debug!(
            page = self.page_count,
            "abandoning Trino result set after a failed page fetch"
        );
        self.batch.clear();
        self.batch_cursor = 0;
        self.next_uri = None;
        self.fetch_failed = true;
        err
    }

    /// Turn a failed page fetch into the right outcome, distinguishing a
    /// cancellation from a genuine failure.
    ///
    /// Both discard the buffered rows, but they leave the statement in
    /// different states. A failure marks it `fetch_failed`, so a further fetch
    /// reports `24000` rather than a spurious `NoData` — the cursor position is
    /// genuinely undefined. A cancellation is not a failure: the application
    /// asked for it, the rows are simply over, and a subsequent fetch should
    /// see the same clean `NoData` it would get had the cancel landed between
    /// fetches rather than during one.
    ///
    /// Either way the drain is suppressed by `next_uri = None`: paging a
    /// cancelled query fails and leaves the pooled socket dirty.
    fn end_page_fetch(&mut self, err: TrinoError) -> TrinoError {
        if matches!(err, TrinoError::OperationCancelled { .. }) {
            tracing::debug!(
                page = self.page_count,
                "Trino reported the query as cancelled; ending the result set"
            );
            self.batch.clear();
            self.batch_cursor = 0;
            self.next_uri = None;
            return err;
        }
        self.abandon_result_set(err)
    }
}

impl StatementBackend for TrinoStatement {
    type Error = TrinoError;

    fn fetch(&mut self) -> Result<FetchResult, TrinoError> {
        // A previous page fetch failed, so the cursor position is undefined and
        // the result set cannot be resumed. Report that rather than the
        // `NoData` an exhausted `next_uri` would otherwise produce.
        if self.fetch_failed {
            return Err(OdbcError::general(
                "the result set was abandoned by an earlier fetch failure",
                SqlState::invalid_cursor_state(),
            )
            .into());
        }

        // Looping rather than recursing: Trino emits empty data pages while a
        // query is queued or planning, and a long-queued query on a busy
        // coordinator can produce arbitrarily many of them. Recursing once per
        // empty page exhausts the stack, and a stack overflow aborts the host
        // process rather than unwinding into panic_safe.
        loop {
            // A concurrent SQLCancel, or a query timeout core enforced by
            // cancelling, has already stopped this query server-side. Discard
            // what is left rather than serving rows from a result set the
            // application asked to abandon, and above all do not poll
            // `next_uri` -- see `cancel` for why that dirties the socket.
            //
            // Reported as an error, not `NoData`. `NoData` is "your result set
            // ended", which is false here: rows were discarded, and an
            // application that asked for a 30-second deadline and got an empty
            // answer at 30 seconds cannot tell a timeout from an empty table.
            // `end_page_fetch` recognises the cancelled variant and keeps it
            // off `abandon_result_set`, so the cursor is finished rather than
            // left in the undefined position `24000` describes.
            //
            // This is the half of the cancel that `map_trino_error` cannot see:
            // a cancel landing between page requests leaves no failed response
            // to classify. Core relabels it `HYT00` when its own timer fired.
            if self.is_cancelled() {
                return Err(self.end_page_fetch(super::cancelled_between_requests()));
            }

            // Try to advance within the current batch.
            if self.batch_cursor < self.batch.len() {
                self.batch_cursor += 1;
                return Ok(FetchResult::Row);
            }

            // Current batch exhausted: fetch the next page from Trino.
            let next_url = match self.next_uri.take() {
                Some(url) => url,
                None => return Ok(FetchResult::NoData), // No more pages.
            };

            let client = self.client.as_ref().ok_or_else(|| {
                TrinoError::from(OdbcError::general(
                    "no client available for streaming fetch",
                    SqlState::general_error(),
                ))
            })?;
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                TrinoError::from(OdbcError::general(
                    "no runtime available for streaming fetch",
                    SqlState::general_error(),
                ))
            })?;

            tracing::debug!(url = %next_url, "fetching next page from Trino");

            // Phase: HTTP fetch
            let fetch_start = Instant::now();
            let fetched = {
                let _span = tracing::info_span!("trino.fetch_page").entered();
                runtime.block_on(client.get_next(&next_url))
            };
            self.total_fetch_time += fetch_start.elapsed();
            self.page_count += 1;

            // Both failure paths below must abandon the result set: the rows of
            // the previous page are still in `self.batch` and would otherwise
            // remain readable through `get_data` after the error was reported.
            let mut page: trino_rust_client::QueryResult<Row> = match fetched {
                Ok(page) => page,
                Err(e) => {
                    let mapped = self.map_client_error(e);
                    return Err(self.end_page_fetch(mapped));
                }
            };

            if let Some(error) = page.error.take() {
                let mapped = self.map_client_error(error.into());
                return Err(self.end_page_fetch(mapped));
            }

            log_page_stats(&page.stats, self.page_count);

            // Extract fields before consuming page.data for conversion.
            self.next_uri = page.next_uri;

            // Phase: row conversion
            let convert_start = Instant::now();
            let converted = {
                let _span =
                    tracing::info_span!("trino.convert_batch", page = self.page_count).entered();
                convert_rows(page.data, &self.trino_types)
            };
            self.batch = match converted {
                Ok(batch) => batch,
                Err(e) => {
                    let mapped = self.map_client_error(e);
                    return Err(self.end_page_fetch(mapped));
                }
            };

            if self.batch.is_empty() {
                self.empty_page_count += 1;
            }
            self.total_convert_time += convert_start.elapsed();
            self.total_rows_fetched += self.batch.len() as u64;
            self.batch_cursor = 0;

            tracing::debug!(
                batch_rows = self.batch.len(),
                has_next = self.next_uri.is_some(),
                "next page received"
            );

            // Fall through to the top of the loop: a non-empty batch is
            // consumed there, and an empty one advances to the next page (or
            // returns NoData when next_uri is exhausted).
        }
    }

    fn get_data(
        &mut self,
        col: u16,
        _target_type: CDataType,
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, TrinoError> {
        if self.batch_cursor == 0 {
            return Err(OdbcError::general(
                "get_data called before fetch",
                SqlState::general_error(),
            )
            .into());
        }
        let col_idx = (col as usize).checked_sub(1).ok_or_else(|| {
            TrinoError::from(OdbcError::general(
                "column index must be >= 1",
                SqlState::general_error(),
            ))
        })?;
        let row = &self.batch[self.batch_cursor - 1];
        row.get(col_idx)
            .map(std::borrow::Cow::Borrowed)
            .ok_or_else(|| {
                TrinoError::from(OdbcError::general(
                    format!("column {col} out of range (have {} columns)", row.len()),
                    SqlState::general_error(),
                ))
            })
    }

    fn column_count(&self) -> i16 {
        // `SQLNumResultCols` writes through a `SQLSMALLINT *`, so the count is
        // narrowed here rather than in core: this is where the real number is
        // known. Saturating is the only option the ABI leaves -- there is no
        // `SQL_NO_TOTAL` for a column count -- and a Trino result set with more
        // than 32767 columns is beyond anything the coordinator will plan.
        i16::try_from(self.columns.len()).unwrap_or_else(|_| {
            tracing::warn!(
                columns = self.columns.len(),
                "result set has more columns than SQLSMALLINT can express; reporting i16::MAX"
            );
            i16::MAX
        })
    }

    fn describe_col(&self, col: u16) -> Result<std::borrow::Cow<'_, ColumnDescriptor>, TrinoError> {
        let idx = (col as usize).checked_sub(1).ok_or_else(|| {
            TrinoError::from(OdbcError::general(
                "column index must be >= 1",
                SqlState::general_error(),
            ))
        })?;
        // Borrowed, not cloned: `SQLColAttribute` calls this once per column
        // per attribute, and the descriptors live on the statement for as long
        // as the result set does.
        self.columns
            .get(idx)
            .map(std::borrow::Cow::Borrowed)
            .ok_or_else(|| {
                TrinoError::from(OdbcError::general(
                    format!("column {col} out of range (have {})", self.columns.len()),
                    SqlState::general_error(),
                ))
            })
    }

    fn row_count(&self) -> Option<i64> {
        // With streaming, we don't know the total row count until exhausted.
        None
    }

    fn close_cursor(&mut self) -> Result<(), TrinoError> {
        // Drain remaining Trino response pages so the underlying HTTP
        // connection is cleanly returned to reqwest's connection pool.
        // Without this, residual bytes on the socket corrupt subsequent
        // queries that reuse the pooled connection.
        //
        // Skipped entirely once the query has been cancelled server-side:
        // `get_next` then fails and leaves exactly the residual bytes this
        // drain exists to avoid.
        let mut drain_failure = None;
        if self.is_cancelled() {
            self.next_uri = None;
        } else if let (Some(client), Some(runtime)) = (&self.client, &self.runtime) {
            let mut next = self.next_uri.take();
            while let Some(url) = next {
                match runtime.block_on(client.get_next::<trino_rust_client::Row>(&url)) {
                    Ok(page) => next = page.next_uri,
                    Err(e) => {
                        // A failed drain leaves the pooled socket dirty, which
                        // surfaces later as an unrelated query failing. Core
                        // records what this returns on the statement's own
                        // diagnostic queue, so it reaches the application
                        // rather than only the log -- but the teardown below
                        // still runs first, so one dirty socket does not also
                        // strand the statement.
                        tracing::warn!(
                            error = %e,
                            "failed to drain remaining Trino pages; the pooled \
                             connection may carry residual bytes"
                        );
                        drain_failure = Some(self.map_client_error(e));
                        break;
                    }
                }
            }
        }

        self.batch.clear();
        self.batch_cursor = 0;
        self.next_uri = None;
        self.query_id = None;
        // The failed result set is gone; the handle is reusable for a new query.
        self.fetch_failed = false;

        match drain_failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Log the profiling summary and drain residual HTTP pages on drop.
///
/// The profiling summary is logged here (not in `close_cursor`) because
/// `close_cursor` may not be called for fully-consumed queries where
/// `next_uri` is already `None`. Drop always runs.
impl Drop for TrinoStatement {
    fn drop(&mut self) {
        // Log profiling summary for any query that actually executed.
        if self.page_count > 0 {
            tracing::info!(
                query_id = ?self.query_id,
                pages = self.page_count,
                empty_pages = self.empty_page_count,
                total_rows = self.total_rows_fetched,
                fetch_ms = self.total_fetch_time.as_millis() as u64,
                convert_ms = self.total_convert_time.as_millis() as u64,
                "query profiling summary"
            );
        }

        // Drain residual pages so reqwest's connection pool isn't corrupted.
        // The result is discarded rather than propagated: there is nowhere for
        // a drop to report a diagnostic, and `close_cursor` has already logged
        // the failure at `warn!`.
        if self.next_uri.is_some() {
            let _ = self.close_cursor();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_semicolon_is_removed() {
        assert_eq!(strip_trailing_semicolons("SELECT 1;"), "SELECT 1");
        assert_eq!(strip_trailing_semicolons("SELECT 1 ;  "), "SELECT 1");
        assert_eq!(strip_trailing_semicolons("SELECT 1;\n"), "SELECT 1");
        // Repeated, because one strip leaving a second terminator behind would
        // fail exactly as the original did.
        assert_eq!(strip_trailing_semicolons("SELECT 1;;"), "SELECT 1");
        assert_eq!(strip_trailing_semicolons("SELECT 1 ; ; "), "SELECT 1");
    }

    #[test]
    fn a_statement_without_one_is_untouched() {
        assert_eq!(strip_trailing_semicolons("SELECT 1"), "SELECT 1");
        assert_eq!(
            strip_trailing_semicolons("SELECT * FROM t"),
            "SELECT * FROM t"
        );
    }

    /// A semicolon inside a literal is data, not a terminator.
    ///
    /// These are safe for free rather than by special handling: a statement
    /// ending in a literal ends with the closing quote, so the trailing
    /// character is never the semicolon. The test pins that reasoning, because
    /// a future "smarter" strip that scanned for any semicolon would corrupt
    /// every one of them.
    #[test]
    fn a_semicolon_inside_a_literal_survives() {
        assert_eq!(strip_trailing_semicolons("SELECT ';'"), "SELECT ';'");
        assert_eq!(strip_trailing_semicolons("SELECT 'a;b'"), "SELECT 'a;b'");
        assert_eq!(
            strip_trailing_semicolons("SELECT * FROM t WHERE x = ';'"),
            "SELECT * FROM t WHERE x = ';'"
        );
        assert_eq!(
            strip_trailing_semicolons(r#"SELECT "weird;name" FROM t"#),
            r#"SELECT "weird;name" FROM t"#
        );
        // A literal that ends the statement *and* is followed by a terminator:
        // the terminator goes, the literal does not.
        assert_eq!(strip_trailing_semicolons("SELECT ';';"), "SELECT ';'");
    }

    /// Nothing sensible is left to send, so nothing is invented: the empty
    /// statement reaches Trino and is reported as its own syntax error rather
    /// than being turned into something the application did not write.
    #[test]
    fn a_statement_of_only_semicolons_reduces_to_empty() {
        assert_eq!(strip_trailing_semicolons(";"), "");
        assert_eq!(strip_trailing_semicolons("  ;; "), "");
        assert_eq!(strip_trailing_semicolons(""), "");
    }
}
