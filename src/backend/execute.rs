//! Statement execution for the Trino backend: `exec_direct`, `prepare` and
//! `execute`, plus the [`StatementBackend`] implementation that streams result
//! rows back through the shared `stackable-odbc-core` fetch path.

use std::sync::Arc;
use std::time::Instant;

use stackable_odbc_core::backend::StatementBackend;
use stackable_odbc_core::errors::OdbcError;
use stackable_odbc_core::types::{
    CDataType, ColumnDescriptor, ColumnValue, ExecuteOutcome, FetchResult, SqlState,
};
use trino_rust_client::{Row, TrinoTy};

use super::info::trino_bare_type_name;
use super::{TrinoConnection, TrinoError, TrinoStatement, log_page_stats, map_trino_error};
use crate::type_conversion::{
    TrinoTypeName, json_to_column_value, trino_ty_precision, trino_ty_scale, trino_ty_to_sql_type,
    type_name_precision, type_name_scale,
};

/// Convert rows from a Trino response page into `Vec<Vec<ColumnValue>>`.
///
/// Takes ownership of the page data to avoid cloning every `Row`. The caller
/// must extract `next_uri`, `stats`, `error`, etc. from the page before
/// passing `page.data` here.
fn convert_rows(
    data: Option<trino_rust_client::models::QueryResultData<Row>>,
    types: &[(String, TrinoTy)],
) -> Vec<Vec<ColumnValue>> {
    let rows = match data {
        Some(d) => d.into_vec(),
        None => return Vec::new(),
    };
    rows.into_iter()
        .map(|row| {
            row.into_json()
                .into_iter()
                .zip(types.iter())
                .map(|(val, (_, ty))| json_to_column_value(val, ty))
                .collect()
        })
        .collect()
}

pub(super) fn exec_direct(conn: &TrinoConnection, sql: &str) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(%sql, "TrinoBackend::exec_direct");

    // Submit the query to Trino.
    let submit_start = Instant::now();
    let mut page = {
        let _span = tracing::info_span!("trino.submit").entered();
        conn.runtime
            .block_on(conn.client.get::<Row>(sql.to_string()))
            .map_err(map_trino_error)?
    };
    let submit_elapsed = submit_start.elapsed();

    log_page_stats(&page.stats, 1);

    // Poll through initial pages until we get column metadata.
    let mut page_count: u32 = 1;
    let mut empty_page_count: u32 = 0;
    let mut total_fetch_time = submit_elapsed;

    if page.columns.is_none() {
        let _span = tracing::info_span!("trino.poll_metadata").entered();
        while page.columns.is_none() {
            if let Some(error) = page.error.take() {
                return Err(map_trino_error(error.into()));
            }
            let next_url = page.next_uri.as_ref().ok_or_else(|| TrinoError::General {
                message: "query returned no columns and no next page".into(),
            })?;

            empty_page_count += 1;
            let fetch_start = Instant::now();
            page = conn
                .runtime
                .block_on(conn.client.get_next(next_url))
                .map_err(map_trino_error)?;
            total_fetch_time += fetch_start.elapsed();
            page_count += 1;

            log_page_stats(&page.stats, page_count);
        }
        tracing::info!(empty_pages = empty_page_count, "metadata polling complete");
    }

    if let Some(error) = page.error.take() {
        return Err(map_trino_error(error.into()));
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

        columns.push(ColumnDescriptor {
            name: column_name.clone(),
            // Spec (SQL_DESC_TYPE_NAME / SQLColumns.TYPE_NAME): both list bare
            // examples ("CHAR", "VARCHAR", ...), not parameterised
            // declarations: "varchar(50)" is a *declaration*, and matches no
            // `SQLGetTypeInfo` row. `trino_bare_type_name` returns the bare
            // name that does (see its doc comment in `backend/info.rs`);
            // precision/scale (below) still come from `native_name`, so the
            // declared length is not lost, only moved out of the name.
            type_name: trino_bare_type_name(&native_name, sql_type),
            sql_type,
            precision: type_name_precision(&native_name)
                .and_then(|p| u32::try_from(p).ok())
                .unwrap_or_else(|| trino_ty_precision(&ty)),
            scale: type_name_scale(&native_name)
                .and_then(|s| i16::try_from(s).ok())
                .unwrap_or_else(|| trino_ty_scale(&ty)),
            nullable: true,
        });
        trino_types.push((column_name, ty));
    }

    // Extract fields before consuming page.data for conversion.
    let next_uri = page.next_uri;
    let query_id = page.id;

    // Convert the first batch of rows (if any).
    let convert_start = Instant::now();
    let batch = {
        let _span = tracing::info_span!("trino.convert_batch", page = page_count).entered();
        convert_rows(page.data, &trino_types)
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
        page_count,
        empty_page_count,
        total_rows_fetched,
        total_fetch_time,
        total_convert_time,
    })
}

pub(super) fn prepare(_conn: &TrinoConnection, sql: &str) -> Result<TrinoStatement, TrinoError> {
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
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

pub(super) fn execute(
    conn: &TrinoConnection,
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

    let mut result = exec_direct(conn, &sql)?;
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
pub(super) fn cancel(stmt: &mut TrinoStatement) -> Result<(), OdbcError> {
    let Some(ref query_id) = stmt.query_id else {
        tracing::debug!("SQLCancel: no query ID available (query may be finished)");
        return Ok(());
    };
    let client = stmt.client.as_ref().ok_or_else(|| OdbcError::General {
        message: "no client available for cancel".into(),
        sqlstate: SqlState::general_error(),
    })?;
    let runtime = stmt.runtime.as_ref().ok_or_else(|| OdbcError::General {
        message: "no runtime available for cancel".into(),
        sqlstate: SqlState::general_error(),
    })?;

    tracing::debug!(query_id = %query_id, "cancelling Trino query");

    runtime
        .block_on(client.cancel(query_id))
        .map_err(|e| OdbcError::from(map_trino_error(e)))?;

    // The query is now cancelled server-side. Clear next_uri so that
    // close_cursor (and Drop) do NOT try to poll get_next: after a
    // server-side cancel, get_next will fail and leave the pooled TCP
    // socket with residual bytes, corrupting subsequent queries on the
    // same reqwest connection pool.
    //
    // This means the pooled connection may have unread response bytes.
    // Callers that share a connection pool across queries should be
    // aware that a cancel may leave a dirty socket in the pool. The
    // socket will eventually be evicted by reqwest's idle timeout (90s).
    stmt.next_uri = None;
    stmt.query_id = None;
    stmt.batch.clear();
    stmt.batch_cursor = 0;

    Ok(())
}

impl TrinoStatement {
    /// Discard the result set after a failed page fetch and return `err`.
    ///
    /// The rows of the last successfully fetched page are still buffered when a
    /// fetch fails. Leaving them in place would let `SQLGetData` keep returning
    /// data for a row the application was told it never received, so the batch
    /// is dropped, the cursor is rewound and the statement is marked failed.
    fn abandon_result_set(&mut self, err: OdbcError) -> OdbcError {
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
}

impl StatementBackend for TrinoStatement {
    fn fetch(&mut self) -> Result<FetchResult, OdbcError> {
        // A previous page fetch failed, so the cursor position is undefined and
        // the result set cannot be resumed. Report that rather than the
        // `NoData` an exhausted `next_uri` would otherwise produce.
        if self.fetch_failed {
            return Err(OdbcError::General {
                message: "the result set was abandoned by an earlier fetch failure".into(),
                sqlstate: SqlState::invalid_cursor_state(),
            });
        }

        // Looping rather than recursing: Trino emits empty data pages while a
        // query is queued or planning, and a long-queued query on a busy
        // coordinator can produce arbitrarily many of them. Recursing once per
        // empty page exhausts the stack, and a stack overflow aborts the host
        // process rather than unwinding into panic_safe.
        loop {
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

            let client = self.client.as_ref().ok_or_else(|| OdbcError::General {
                message: "no client available for streaming fetch".into(),
                sqlstate: SqlState::general_error(),
            })?;
            let runtime = self.runtime.as_ref().ok_or_else(|| OdbcError::General {
                message: "no runtime available for streaming fetch".into(),
                sqlstate: SqlState::general_error(),
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
                Err(e) => return Err(self.abandon_result_set(map_trino_error(e).into())),
            };

            if let Some(error) = page.error.take() {
                return Err(self.abandon_result_set(map_trino_error(error.into()).into()));
            }

            log_page_stats(&page.stats, self.page_count);

            // Extract fields before consuming page.data for conversion.
            self.next_uri = page.next_uri;

            // Phase: row conversion
            let convert_start = Instant::now();
            self.batch = {
                let _span =
                    tracing::info_span!("trino.convert_batch", page = self.page_count).entered();
                convert_rows(page.data, &self.trino_types)
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
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, OdbcError> {
        if self.batch_cursor == 0 {
            return Err(OdbcError::General {
                message: "get_data called before fetch".into(),
                sqlstate: SqlState::general_error(),
            });
        }
        let col_idx = (col as usize)
            .checked_sub(1)
            .ok_or_else(|| OdbcError::General {
                message: "column index must be >= 1".into(),
                sqlstate: SqlState::general_error(),
            })?;
        let row = &self.batch[self.batch_cursor - 1];
        row.get(col_idx)
            .map(std::borrow::Cow::Borrowed)
            .ok_or_else(|| OdbcError::General {
                message: format!("column {col} out of range (have {} columns)", row.len()),
                sqlstate: SqlState::general_error(),
            })
    }

    fn column_count(&self) -> u16 {
        self.columns.len() as u16
    }

    fn describe_col(&self, col: u16) -> Result<ColumnDescriptor, OdbcError> {
        let idx = (col as usize)
            .checked_sub(1)
            .ok_or_else(|| OdbcError::General {
                message: "column index must be >= 1".into(),
                sqlstate: SqlState::general_error(),
            })?;
        self.columns
            .get(idx)
            .cloned()
            .ok_or_else(|| OdbcError::General {
                message: format!("column {col} out of range (have {})", self.columns.len()),
                sqlstate: SqlState::general_error(),
            })
    }

    fn row_count(&self) -> Option<usize> {
        // With streaming, we don't know the total row count until exhausted.
        None
    }

    fn close_cursor(&mut self) {
        // Drain remaining Trino response pages so the underlying HTTP
        // connection is cleanly returned to reqwest's connection pool.
        // Without this, residual bytes on the socket corrupt subsequent
        // queries that reuse the pooled connection.
        if let (Some(client), Some(runtime)) = (&self.client, &self.runtime) {
            let mut next = self.next_uri.take();
            while let Some(url) = next {
                match runtime.block_on(client.get_next::<trino_rust_client::Row>(&url)) {
                    Ok(page) => next = page.next_uri,
                    Err(e) => {
                        // The drain is best-effort (close_cursor has no way to
                        // report a diagnostic) but a silent break hides the
                        // fact that the pooled socket is now dirty, which
                        // surfaces later as an unrelated query failing.
                        tracing::warn!(
                            error = %e,
                            "failed to drain remaining Trino pages; the pooled \
                             connection may carry residual bytes"
                        );
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
        if self.next_uri.is_some() {
            self.close_cursor();
        }
    }
}
