//! Core type definitions for the Trino backend — [`TrinoBackend`],
//! [`TrinoConnection`], [`TrinoStatement`] — plus `connect`, `disconnect`,
//! `end_tran`, error mapping, and the thin [`Backend`] delegation layer.
//! Statement execution, catalog metadata, `SQLGetInfo`, and parameter binding
//! live in the submodules.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use snafu::Snafu;
use stackable_odbc_core::{
    backend::Backend,
    errors::OdbcError,
    types::{
        ColumnDescriptor, ColumnRow, ColumnValue, ConnectParams, ExecuteOutcome, ForeignKeyRow,
        IdentifierType, InfoValue, Nullable, PrimaryKeyRow, SQL_CB_NULL, SQL_CN_ANY,
        SQL_FN_CVT_CAST, SQL_FN_TSI_DAY, SQL_FN_TSI_HOUR, SQL_FN_TSI_MINUTE, SQL_FN_TSI_MONTH,
        SQL_FN_TSI_QUARTER, SQL_FN_TSI_SECOND, SQL_FN_TSI_WEEK, SQL_FN_TSI_YEAR,
        SQL_GB_GROUP_BY_CONTAINS_SELECT, SQL_IC_LOWER, SQL_NC_END, SQL_NNC_NON_NULL,
        SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN,
        SQL_SQ_QUANTIFIED, SQL_U_UNION, SQL_U_UNION_ALL, Scope, SpecialColumnRow, StatisticsRow,
        TableRow, TypeInfoRow, format_odbc_version, parse_dotted_version,
    },
};
use trino_rust_client::{Client, ClientBuilder, Trino, auth::Auth, ssl::Ssl};

mod execute;
// `pub(crate)` only under `cfg(test)`: the FFI integration tests
// (`ffi_integration_tests.rs`, a sibling of this module under `lib.rs`) need to
// reach the `TRINO_*` capability bitmap constants declared in `info`. Non-test
// callers of this module are descendants of `backend` and can already see a
// plain private `mod info`.
#[cfg(test)]
pub(crate) mod info;
#[cfg(not(test))]
mod info;
mod metadata;
mod params;
mod types;

/// The interval units Trino's `date_add` / `date_diff` accept, reported for
/// both `SQL_TIMEDATE_ADD_INTERVALS` and `SQL_TIMEDATE_DIFF_INTERVALS`.
///
/// Kept in step with `crate::escape_dialect::trino_interval_unit`, which is
/// what turns each of these into the quoted unit Trino wants;
/// `advertised_intervals_are_all_rewritable` asserts the two agree.
pub(crate) const TRINO_TIMESTAMP_INTERVALS: u32 = SQL_FN_TSI_SECOND
    | SQL_FN_TSI_MINUTE
    | SQL_FN_TSI_HOUR
    | SQL_FN_TSI_DAY
    | SQL_FN_TSI_WEEK
    | SQL_FN_TSI_MONTH
    | SQL_FN_TSI_QUARTER
    | SQL_FN_TSI_YEAR;

/// Maps a `trino_rust_client` error to the most appropriate [`TrinoError`] variant,
/// preserving SQLSTATE semantics for link failures, timeouts, and auth errors.
///
/// Every error from the client library must be routed through here; hand-building
/// a `TrinoError` at the call site silently degrades a specific SQLSTATE to `HY000`.
/// SQLSTATE for "operation canceled", which core provides no named
/// constructor for. See [`TrinoError::OperationCancelled`].
const SQL_STATE_CANCELLED: &str = "HY008";

/// Trino's `USER_CANCELED` error code, reported when a query is killed while a
/// request against it is in flight.
///
/// From Trino's `StandardErrorCode` enum, where it sits in the `USER_ERROR`
/// range alongside `PERMISSION_DENIED` (4), which the arm below it names.
const TRINO_ERROR_USER_CANCELED: i32 = 3;

/// The cause attached to a [`TrinoError::Query`] or
/// [`TrinoError::OperationCancelled`].
///
/// Two shapes, because the two kinds of failure carry very different amounts
/// of text. A transport error's own `Display` is a single line and is kept
/// whole. A server-side `QueryError` renders the coordinator's entire
/// `failure_info` — its Java stack — and core walks the whole causal chain into
/// the `SQLGetDiagRec` message, so keeping it whole put between 1,700 and
/// 15,000 characters into every diagnostic (measured against a live
/// coordinator; `DIVISION_BY_ZERO` was the worst at ~30 KB of UTF-16 across
/// ~168 frames).
///
/// None of that is actionable through ODBC: the stack describes the
/// coordinator's internals, the application already gets Trino's own error code
/// verbatim through `NativeErrorPtr`, and the summary that names what actually
/// went wrong is the first line. The stack is therefore logged at `debug`
/// rather than marshalled — `ODBC_LOG_LEVEL` / `ODBC_LOG_FILE` are what a
/// person debugging Trino itself reaches for.
#[derive(Debug, Snafu)]
pub enum QueryCause {
    /// A transport failure, kept verbatim; its `Display` is already one line.
    #[snafu(display("{source}"))]
    Transport {
        source: trino_rust_client::error::Error,
    },
    /// A server-side rejection, reduced to what an application can act on.
    #[snafu(display("query error [{error_name}]: {message}"))]
    Server { error_name: String, message: String },
}

/// Split a client error into the native error code and the cause to attach.
///
/// The single place `failure_info` is dropped, so the decision is made once for
/// every arm that carries a cause.
fn query_cause(e: trino_rust_client::error::Error) -> (QueryCause, i32) {
    use trino_rust_client::error::Error;
    match e {
        Error::Query(query_error) => {
            // Logged rather than discarded: this is the only copy, and it is
            // what a person debugging the coordinator actually wants.
            if let Some(ref failure) = query_error.failure_info {
                tracing::debug!(
                    error_name = %query_error.error_name,
                    failure_info = ?failure,
                    "Trino failure_info; omitted from the SQLGetDiagRec message, \
                     which carries the summary and the native error code"
                );
            }
            let native_error = query_error.error_code;
            (
                QueryCause::Server {
                    error_name: query_error.error_name,
                    message: query_error.message,
                },
                native_error,
            )
        }
        // A transport failure has no Trino error code, and `0` is the spec's
        // "no native code".
        other => (QueryCause::Transport { source: other }, 0),
    }
}

pub(crate) fn map_trino_error(e: trino_rust_client::error::Error) -> TrinoError {
    use trino_rust_client::error::Error;
    match e {
        // Checked before the catch-all `Error::Query` arm at the bottom, which
        // would otherwise swallow this into `TrinoError::Query` and report
        // HY000 for a cancellation the spec gives its own SQLSTATE.
        Error::Query(ref query_error) if query_error.error_code == TRINO_ERROR_USER_CANCELED => {
            let (source, native_error) = query_cause(e);
            TrinoError::OperationCancelled {
                source,
                native_error,
            }
        }
        Error::HttpError(ref req_err) if req_err.is_connect() => {
            TrinoError::CommunicationLinkFailure {
                message: format!("unable to reach Trino server: {req_err}"),
            }
        }
        Error::HttpError(ref req_err) if req_err.is_timeout() => TrinoError::QueryTimeout {
            message: format!("request timed out: {req_err}"),
        },
        // HTTP 401 Unauthorized or 403 Forbidden
        Error::HttpNotOk(ref status, ref reason)
            if status.as_u16() == 401 || status.as_u16() == 403 =>
        {
            TrinoError::AuthFailure {
                message: format!("HTTP {status}: {reason}"),
            }
        }
        Error::Forbidden { ref message } => TrinoError::AuthFailure {
            message: message.clone(),
        },
        Error::ReachMaxAttempt(n) => TrinoError::QueryTimeout {
            message: format!("query failed after {n} retry attempts"),
        },
        // Everything else, which is where a server-side `Error::Query` lands.
        // Its `QueryError` is the only shape carrying Trino's own error code;
        // a transport failure has none, and `0` is the spec's "no native
        // code".
        //
        // PERMISSION_DENIED (code 4) never reaches here: the client's
        // `From<QueryError> for Error` turns it into `Error::Forbidden` and
        // drops the code on the way, so that one maps to 28000 above with a
        // native code of 0.
        other => {
            let (source, native_error) = query_cause(other);
            TrinoError::Query {
                source,
                native_error,
            }
        }
    }
}

/// Log Trino server-side stats from a `QueryResult` page.
///
/// These stats come directly from the Trino coordinator and are the authoritative
/// measure of server-side execution time, separating it from HTTP/client overhead.
pub(crate) fn log_page_stats(stats: &trino_rust_client::Stat, page_number: u32) {
    tracing::info!(
        page = page_number,
        trino_elapsed_ms = stats.elapsed_time_millis,
        trino_cpu_ms = stats.cpu_time_millis,
        trino_wall_ms = stats.wall_time_millis,
        trino_queued_ms = stats.queued_time_millis,
        trino_rows = stats.processed_rows,
        trino_bytes = stats.processed_bytes,
        trino_peak_mem = stats.peak_memory_bytes,
        trino_state = %stats.state,
        "trino server stats"
    );
}

/// Execute a SQL query and collect all rows.
///
/// Delegates to `client.get_all()`, which paginates internally using
/// `get_retry()`/`get_next_retry()` (exponential backoff) and preserves column
/// metadata for zero-row results. Trino error code 4 (PERMISSION_DENIED) is
/// converted to `Error::Forbidden` by the client's `From<QueryError>` impl, so
/// [`map_trino_error`] yields SQLSTATE 28000; other query failures arrive as
/// `Error::Query` and map to the general variant.
///
/// Note: `get_all()` exposes no per-page stats, so [`log_page_stats`] is not
/// called here. The main query path in `execute.rs` still logs per-page stats.
pub(crate) fn query_all_rows(
    conn: &TrinoConnection,
    sql: String,
) -> Result<Vec<trino_rust_client::Row>, TrinoError> {
    let _span = tracing::info_span!("trino.query_all_rows").entered();

    let rows = conn
        .runtime
        .block_on(conn.client.get_all::<trino_rust_client::Row>(sql))
        .map_err(map_trino_error)?
        .into_vec();

    tracing::info!(total_rows = rows.len(), "query_all_rows complete");

    Ok(rows)
}

/// Build the statement used to prove the connection works.
///
/// With no catalog configured this is `SELECT 1`, which Trino answers without
/// touching a connector while still running the full authentication path.
///
/// When a catalog *is* configured the probe must also prove that the catalog
/// exists, and `SELECT 1` does not: Trino never resolves the session catalog
/// for a query that does not reference one, so a nonexistent catalog would
/// connect happily and only fail at the application's first real query.
///
/// `LIKE ''` is what keeps this bounded. Catalog resolution happens before the
/// pattern is applied, so an unknown catalog still fails with
/// `CATALOG_NOT_FOUND`, but no schema is named `''`, so the probe returns zero
/// rows instead of every schema in the catalog, which may be thousands.
fn validation_query(catalog: Option<&str>) -> String {
    match catalog {
        // Delimited identifier: a `"` inside the name is escaped by doubling.
        Some(cat) => format!("SHOW SCHEMAS FROM \"{}\" LIKE ''", cat.replace('"', "\"\"")),
        None => "SELECT 1".to_string(),
    }
}

/// Verify that the freshly built client can actually reach and use Trino.
///
/// `ClientBuilder::build` performs no I/O, so without this the driver would
/// report success from `SQLDriverConnect` against an unreachable coordinator,
/// wrong credentials or a catalog that does not exist, and the failure would
/// only surface on the application's first query, long after the point where
/// the ODBC spec (and every application) expects a connection error.
///
/// Failures are reclassified for the connect-time context: the SQLSTATEs that
/// belong to establishing a connection are 08001 and 28000, not the 08S01 that
/// [`map_trino_error`] produces for the same transport error mid-session. A
/// connection whose catalog does not resolve cannot run anything, so that
/// counts as a failure to establish a usable connection too.
fn validate_connection(conn: &TrinoConnection, catalog: Option<&str>) -> Result<(), TrinoError> {
    let _span = tracing::info_span!("trino.validate_connection").entered();

    match query_all_rows(conn, validation_query(catalog)) {
        Ok(_) => {
            tracing::debug!("connection validated");
            Ok(())
        }
        // Already correct for connect time, and more specific than 08001.
        Err(e @ (TrinoError::AuthFailure { .. } | TrinoError::QueryTimeout { .. })) => Err(e),
        Err(e) => Err(TrinoError::ConnectionFailed {
            message: e.to_string(),
        }),
    }
}

/// Ask the coordinator its version, for `SQL_DBMS_VER`.
///
/// Returns the empty string on any failure. A driver must not refuse a
/// connection because it could not learn the server version -- the connection
/// is already proven usable by [`validate_connection`] before this runs, and
/// `SQL_DBMS_VER` returning `""` is the spec's own "not available".
///
/// Trino's `version()` returns a bare integer for modern releases (`"467"`)
/// and a dotted string for pre-0.216 releases (`"0.215"`); development builds
/// append a suffix (`"468-SNAPSHOT"`). [`parse_dotted_version`] handles all
/// three.
fn fetch_server_version(conn: &TrinoConnection) -> ServerVersion {
    let rows = match query_all_rows(conn, "SELECT version()".to_string()) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the Trino server version");
            return ServerVersion::default();
        }
    };

    let raw = rows
        .first()
        .and_then(|row| row.value().first())
        .and_then(|v| v.as_str());

    let Some(raw) = raw else {
        tracing::warn!("SELECT version() returned no usable value");
        return ServerVersion::default();
    };

    match parse_dotted_version(raw) {
        Some((major, minor, release)) => {
            let formatted = format_odbc_version(major, minor, release);
            tracing::debug!(raw, formatted, major, "Trino server version");
            ServerVersion {
                // The spec permits appending the data source's own version
                // string after the ##.##.#### prefix.
                formatted: format!("{formatted} ({raw})"),
                major,
            }
        }
        None => {
            tracing::warn!(raw, "could not parse the Trino server version");
            ServerVersion::default()
        }
    }
}

/// What [`fetch_server_version`] learned from the coordinator.
///
/// The default -- empty string, major `0` -- is the "probe failed" state. It
/// makes `SQL_DBMS_VER` report the spec's "not available" and gates every
/// version-dependent capability flag off.
#[derive(Default)]
struct ServerVersion {
    /// Rendered for `SQL_DBMS_VER`, in the ODBC `##.##.####` form.
    formatted: String,
    /// The major version as a number, for capability gating.
    major: u32,
}

/// The Trino [`stackable_odbc_core::backend::Backend`] implementation.
///
/// A zero-sized type: it carries no state, serving only as the type parameter
/// that [`stackable_odbc_core::forward_ffi!`] instantiates the generic ODBC C ABI entry
/// points with. All per-connection state lives in `TrinoConnection`.
pub struct TrinoBackend;

pub struct TrinoConnection {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub client: Arc<Client>,
    /// The coordinator's version, already rendered in the ODBC `##.##.####`
    /// form `SQL_DBMS_VER` requires. Empty when the probe failed -- the ODBC
    /// spec's own representation of "not available".
    ///
    /// Captured once at connect rather than per `SQLGetInfo` call: a Trino
    /// coordinator's version cannot change under a live connection, and
    /// `SQLGetInfo` is called often enough by BI tools that a per-call query
    /// would be a visible cost.
    pub dbms_version: String,
    /// The coordinator's major version as a number, for capability gating.
    ///
    /// Several SQL-92 features Trino gained recently -- `CORRESPONDING` (475),
    /// `MATCH` and `UNIQUE` (482), `OVERLAPS` (483) -- change what
    /// `SQL_SQL92_PREDICATES` and `SQL_SQL92_RELATIONAL_JOIN_OPERATORS` may
    /// honestly claim; the capability-gating logic reads this field to decide.
    ///
    /// `0` when the probe failed, which gates every version-dependent flag
    /// off. Understating capability is the safe direction: a BI tool folds
    /// less than it could, rather than emitting SQL the server rejects.
    pub server_major: u32,
    /// The catalog this connection was opened against, from the `Catalog`
    /// connection-string key, or `None` when it named none.
    ///
    /// Reported as `SQL_DATABASE_NAME`, which the spec defines as the current
    /// database in use and treats as the `SQLGetConnectAttr` /
    /// `SQL_ATTR_CURRENT_CATALOG` value. Core's shared default is the empty
    /// string, correct only for a backend that cannot answer -- this one can.
    pub catalog: Option<String>,
}

/// The state a `SQLCancel` on one thread and the executing statement on
/// another both reach.
///
/// Held behind an `Arc` by both [`TrinoCancelToken`] and the
/// [`TrinoStatement`] the token's statement produced, which is what lets
/// `cancel` communicate with a statement it has no reference to.
#[derive(Debug, Default)]
pub(crate) struct CancelState {
    /// The Trino query id of the work currently in flight, or `None` when
    /// nothing cancellable is running.
    ///
    /// Trino only names a query once the coordinator has accepted it, so this
    /// is empty for the duration of the submitting request. A `SQLCancel`
    /// landing in that window finds nothing to cancel and succeeds, which is
    /// what the spec asks for when there is no processing to interrupt.
    query_id: Mutex<Option<String>>,
    /// Set once a query has been cancelled server-side, and read by the fetch
    /// and teardown paths to keep them off `next_uri`.
    ///
    /// After a server-side cancel, polling `get_next` fails and leaves the
    /// pooled TCP socket carrying residual bytes, which surfaces later as an
    /// unrelated query failing on the same reqwest pool. Before core moved
    /// cancellation onto a token, `cancel` held `&mut TrinoStatement` and
    /// cleared `next_uri` directly; it no longer has one, so the statement has
    /// to observe the cancellation itself.
    cancelled: AtomicBool,
}

impl CancelState {
    /// Record the query the statement just submitted, and clear any earlier
    /// cancellation.
    ///
    /// Called at the point a statement-producing call learns its query id. The
    /// reset matters because core builds one token per *statement* and never
    /// replaces it: a re-execute of the same prepared handle reuses the token
    /// that a previous `SQLCancel` may already have marked.
    pub(crate) fn begin_query(&self, query_id: String) {
        self.cancelled.store(false, Ordering::SeqCst);
        match self.query_id.lock() {
            Ok(mut slot) => *slot = Some(query_id),
            // A poisoned lock means a panic while the slot was held. Losing the
            // id costs cancellability for this query; it must not also fail the
            // query, which is running perfectly well.
            Err(_) => {
                tracing::warn!("cancel state was poisoned; this query will not be cancellable")
            }
        }
    }

    /// Whether the statement's query has been cancelled server-side.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// The value [`Backend::cancel`] receives, and the only handle it gets on the
/// query it is asked to interrupt.
///
/// The client and runtime are captured from the connection at construction
/// rather than resolved inside `cancel`, which is the rule
/// [`Backend::cancel_token`] states: MariaDB's ODBC-401 assembled its cancel
/// channel lazily, after the originating connection's TLS settings were gone,
/// and silently failed to cancel encrypted connections.
pub struct TrinoCancelToken {
    client: Arc<Client>,
    runtime: Arc<tokio::runtime::Runtime>,
    state: Arc<CancelState>,
}

/// Builds a `TrinoConnection` that performs no network I/O.
///
/// `ClientBuilder::build` only assembles a `reqwest::Client` (URL parsing,
/// timeout config), and building a `tokio::runtime::Runtime` is a local
/// operation; neither talks to a server. Validation happens in the separate
/// `validate_connection` call [`TrinoBackend::connect`] makes afterwards.
///
/// Every capability declaration takes a `&TrinoConnection` now, so the offline
/// tests that assert what this driver reports need one. They assert values that
/// do not depend on the coordinator, which is exactly what this stands in for.
#[cfg(test)]
pub(crate) fn disconnected_trino_conn() -> TrinoConnection {
    let client = ClientBuilder::new("test", "localhost")
        .port(8080)
        .build()
        .expect("ClientBuilder::build performs no I/O and cannot fail here");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Runtime::build performs no I/O and cannot fail here");
    TrinoConnection {
        runtime: Arc::new(runtime),
        client: Arc::new(client),
        // No live server was contacted, so no version was probed either --
        // this mirrors the "probe failed" state `TrinoBackend::connect` would
        // leave behind if `fetch_server_version` could not reach a coordinator.
        dbms_version: String::new(),
        server_major: 0,
        // No `Catalog` key was supplied, which is the SQL_DATABASE_NAME
        // "not available" case.
        catalog: None,
    }
}

pub struct TrinoStatement {
    /// SQL stored by SQLPrepare, consumed by SQLExecute.
    pending_sql: Option<String>,
    /// Column metadata (available from the first Trino response page).
    pub(crate) columns: Vec<ColumnDescriptor>,
    /// Column types from Trino (needed for converting subsequent pages).
    trino_types: Vec<(String, trino_rust_client::TrinoTy)>,
    /// Current in-memory batch of converted rows.
    pub(crate) batch: Vec<Vec<ColumnValue>>,
    /// Position within the current batch (0 = before first row).
    batch_cursor: usize,
    /// Set once a page fetch has failed. The result set is then unusable: the
    /// rows of the last good page must not be readable, and a further fetch
    /// must report an error rather than a spurious `NoData`.
    pub(crate) fetch_failed: bool,
    /// URL for the next page of results from Trino, or None if exhausted.
    next_uri: Option<String>,
    /// Trino query ID for cancellation via DELETE /v1/query/{id}.
    query_id: Option<String>,
    /// Shared reference to the Trino HTTP client (for fetching next pages).
    client: Option<Arc<Client>>,
    /// Shared reference to the tokio runtime (for block_on in fetch).
    runtime: Option<Arc<tokio::runtime::Runtime>>,
    /// The half of the cancel token this statement can see.
    ///
    /// `None` for the statements built entirely in memory — the catalog
    /// results and `tables_list_table_types` — which hold no `next_uri` and so
    /// have nothing for a cancellation to stop.
    pub(crate) cancel_state: Option<Arc<CancelState>>,

    // --- Profiling counters (always present; ~48 bytes, negligible overhead) ---
    /// Total number of Trino REST API pages fetched for this query.
    page_count: u32,
    /// Number of pages that contained zero data rows (Trino planning/empty pages).
    empty_page_count: u32,
    /// Total number of data rows fetched across all pages.
    total_rows_fetched: u64,
    /// Cumulative wall time spent in HTTP calls (`block_on(client.get_next(...))`).
    total_fetch_time: std::time::Duration,
    /// Cumulative wall time spent in `convert_rows()` (clone + JSON → ColumnValue).
    total_convert_time: std::time::Duration,
}

#[derive(Debug, Snafu)]
pub enum TrinoError {
    #[snafu(display("Trino error: {message}"))]
    General { message: String },
    #[snafu(display("Tokio runtime error: {source}"))]
    Runtime { source: std::io::Error },
    #[snafu(display("Missing parameter: {name}"))]
    MissingParam { name: String },
    #[snafu(display("{feature} is not implemented"))]
    NotImplemented { feature: String },
    /// A usable connection to Trino could not be established.
    ///
    /// Produced only by [`validate_connection`], which is the only place that
    /// runs before the ODBC connection exists and so the only place 08001 is
    /// the correct SQLSTATE.
    #[snafu(display("Connection failed: {message}"))]
    ConnectionFailed { message: String },
    /// The link to the Trino coordinator failed while a request was in flight.
    ///
    /// This is 08S01, not 08001: `TrinoBackend::connect` performs no network
    /// I/O (it only builds the HTTP client), so by the time any request can
    /// fail the ODBC connection is already established, and 08001 is reserved
    /// by the spec for the connection functions.
    #[snafu(display("Communication link failure: {message}"))]
    CommunicationLinkFailure { message: String },
    /// The HTTP request to Trino exceeded the configured timeout.
    #[snafu(display("Query timed out: {message}"))]
    QueryTimeout { message: String },
    /// Trino rejected the request with an authentication/authorization error.
    #[snafu(display("Authentication failed: {message}"))]
    AuthFailure { message: String },
    /// Authentication configuration is invalid (e.g. a bearer token supplied
    /// over plain HTTP, or both a password and a token). Maps to SQLSTATE 28000.
    #[snafu(display("{message}"))]
    AuthConfig { message: String },
    /// A failure from `trino-rust-client` that [`map_trino_error`] does not
    /// classify into one of the specific variants above.
    ///
    /// The client error is kept as the cause rather than flattened into a
    /// string, and `native_error` carries Trino's own error code when the
    /// coordinator supplied one. `SQLGetDiagRec` reports that code verbatim
    /// through `NativeErrorPtr`, where it is the only value an application can
    /// act on; every failure reporting `0` tells it nothing.
    ///
    /// The message deliberately does not interpolate `source`: core walks the
    /// whole causal chain when it builds the diagnostic record, so doing both
    /// would print the client error twice.
    #[snafu(display("query failed"))]
    Query {
        source: QueryCause,
        native_error: i32,
    },
    /// The query was cancelled while this request was in flight.
    ///
    /// Maps to `HY008`, which the spec defines for exactly this case:
    /// "the function was called, and before it completed execution,
    /// `SQLCancel` ... was called on the StatementHandle from a different
    /// thread in a multithreaded application". That clause carries no `(DM)`
    /// annotation, so it is the driver's to report, not the Driver Manager's.
    ///
    /// Recognised from Trino's own `USER_CANCELED` error code rather than from
    /// this driver's cancel flag, because the two race: the cancelling thread
    /// sets its flag only after its `DELETE` returns, by which time the
    /// coordinator may already have failed the in-flight page request. The
    /// server's verdict needs no cross-thread ordering, and it also covers a
    /// query killed by someone else entirely — another client, or
    /// `CALL system.runtime.kill_query`.
    #[snafu(display("query was cancelled"))]
    OperationCancelled {
        source: QueryCause,
        native_error: i32,
    },
    /// An error core itself produced.
    ///
    /// `Backend::Error` is bounded by `From<OdbcError>` so that a defaulted
    /// trait body can construct an error and still name `Self::Error`. This is
    /// that conversion's landing site. It wraps rather than flattens: the
    /// `From<TrinoError> for OdbcError` direction unwraps it unchanged, so the
    /// SQLSTATE core chose survives a round trip through this type instead of
    /// being degraded to `HY000`.
    #[snafu(display("{source}"))]
    Odbc { source: OdbcError },
}

impl From<OdbcError> for TrinoError {
    fn from(source: OdbcError) -> Self {
        TrinoError::Odbc { source }
    }
}

impl From<TrinoError> for OdbcError {
    fn from(e: TrinoError) -> Self {
        use stackable_odbc_core::types::SqlState;
        match e {
            // Unwrapped, not re-wrapped: this is the other half of
            // `From<OdbcError> for TrinoError`, and preserving the SQLSTATE
            // core already chose is the whole point of that variant.
            TrinoError::Odbc { source } => source,
            TrinoError::Query {
                source,
                native_error,
            } => OdbcError::general("query failed", SqlState::general_error())
                .with_native_error(native_error)
                .with_source(source),
            TrinoError::NotImplemented { ref feature } => OdbcError::NotImplemented {
                feature: feature.clone(),
            },
            TrinoError::ConnectionFailed { ref message } => OdbcError::general(
                message.clone(),
                SqlState::client_unable_to_establish_connection(),
            ),
            TrinoError::CommunicationLinkFailure { ref message } => {
                OdbcError::general(message.clone(), SqlState::communication_link_failure())
            }
            TrinoError::QueryTimeout { ref message } => {
                OdbcError::general(message.clone(), SqlState::timeout_expired())
            }
            TrinoError::AuthFailure { ref message } => {
                OdbcError::general(message.clone(), SqlState::invalid_auth_spec())
            }
            TrinoError::AuthConfig { ref message } => {
                OdbcError::general(message.clone(), SqlState::invalid_auth_spec())
            }
            // `SqlState::new` rather than a named constructor: core has none
            // for HY008, because it documents HY008 as never returned by a
            // driver ("not applicable; the `Backend` trait is synchronous").
            // Cross-thread `SQLCancel` made that false. See the CHANGELOG.
            TrinoError::OperationCancelled {
                source,
                native_error,
            } => OdbcError::general("query was cancelled", SqlState::new(SQL_STATE_CANCELLED))
                .with_native_error(native_error)
                .with_source(source),
            _ => OdbcError::general(e.to_string(), SqlState::general_error()),
        }
    }
}

/// Decide the client `Auth` from the resolved connection parameters.
///
/// - token + password    -> Err (ambiguous authentication)
/// - token + !secure     -> Err (a bearer token must not travel over plain HTTP)
/// - token + secure      -> Jwt
/// - no token + secure   -> Basic(user, password) -- Basic even when no
///   password is supplied (`Basic(user, None)`); preserves prior behavior
/// - no token + !secure  -> None (user-only `X-Trino-User` header; a password
///   supplied over HTTP is dropped with a warning by `connect`, not here)
fn resolve_auth(
    secure: bool,
    user: &str,
    password: Option<&str>,
    access_token: Option<&str>,
) -> Result<Option<Auth>, TrinoError> {
    match (access_token, password) {
        (Some(_), Some(_)) => Err(TrinoError::AuthConfig {
            message: "both a password and an access token were supplied; provide only one".into(),
        }),
        (Some(_), None) if !secure => Err(TrinoError::AuthConfig {
            message: "an access token requires Protocol=https; it will not be sent over plain HTTP"
                .into(),
        }),
        (Some(token), None) => Ok(Some(Auth::Jwt(token.to_string()))),
        // No token: preserve the pre-existing behavior, Basic over HTTPS
        // (even with no password: Basic(user, None)), user-only over HTTP.
        (None, _) if secure => Ok(Some(Auth::Basic(
            user.to_string(),
            password.map(str::to_string),
        ))),
        (None, _) => Ok(None),
    }
}

impl Backend for TrinoBackend {
    type CancelToken = TrinoCancelToken;
    type Connection = TrinoConnection;
    type Error = TrinoError;
    type Statement = TrinoStatement;

    fn connect(params: &ConnectParams) -> Result<TrinoConnection, TrinoError> {
        let p = types::connect_params::TrinoConnectParams::try_from(params)?;

        tracing::debug!(
            host = p.host(),
            port = p.port(),
            user = p.user(),
            secure = p.secure(),
            tls_verify = p.tls_verify(),
            query_timeout_secs = p.query_timeout().as_secs(),
            "TrinoBackend::connect"
        );

        // Over plain HTTP, Trino uses the X-Trino-User header set by ClientBuilder::new.
        // Basic auth (password) is only sent over HTTPS: Trino rejects passwords over HTTP
        // even when allow-insecure-over-http=true is configured.
        let mut builder = ClientBuilder::new(p.user(), p.host())
            .port(p.port())
            .secure(p.secure())
            .client_request_timeout(p.query_timeout());
        match resolve_auth(p.secure(), p.user(), p.password(), p.access_token())? {
            Some(auth) => {
                builder = builder.auth(auth).auth_http_insecure(false);
            }
            None => {
                if !p.secure() && p.password().is_some() {
                    tracing::warn!(
                        "Password was supplied but Protocol is http; it will not be sent. \
                         Use Protocol=https to authenticate with a password."
                    );
                }
            }
        }

        if let Some(cat) = p.catalog() {
            builder = builder.catalog(cat);
        }
        if let Some(sch) = p.schema() {
            builder = builder.schema(sch);
        }

        if p.secure() {
            if !p.tls_verify() {
                tracing::warn!(
                    "TlsVerify=false: TLS certificate verification is disabled. \
                     The connection is encrypted but not authenticated, so it is \
                     vulnerable to man-in-the-middle attacks. Use Certificate=<pem> \
                     to verify against a private CA instead."
                );
                builder = builder.no_verify(true);
            } else if let Some(cert_path) = p.certificate() {
                let root_cert =
                    Ssl::read_pem(&cert_path.to_owned()).map_err(|e| TrinoError::General {
                        message: format!("failed to read certificate at {cert_path}: {e}"),
                    })?;
                builder = builder.ssl(Ssl {
                    root_cert: Some(root_cert),
                });
            }
        }

        let client = builder.build().map_err(|e| TrinoError::General {
            message: format!("failed to build Trino client: {e}"),
        })?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| TrinoError::Runtime { source: e })?;

        let mut conn = TrinoConnection {
            runtime: Arc::new(runtime),
            client: Arc::new(client),
            dbms_version: String::new(),
            server_major: 0,
            catalog: p.catalog().map(str::to_owned),
        };
        validate_connection(&conn, p.catalog())?;
        let version = fetch_server_version(&conn);
        conn.dbms_version = version.formatted;
        conn.server_major = version.major;
        Ok(conn)
    }

    fn disconnect(_conn: &mut TrinoConnection) -> Result<(), TrinoError> {
        Ok(()) // runtime drops on its own
    }

    fn browse_connect_attrs() -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[
            Cow::Borrowed("host"),
            Cow::Borrowed("port"),
            Cow::Borrowed("user"),
        ])
    }

    /// This driver does not implement transactions, so every connection behaves
    /// as if it were in autocommit mode. SQLEndTran calls are accepted as no-ops
    /// so that tools like PowerBI do not fail.
    ///
    /// Trino itself does support transactions — `START TRANSACTION` / `COMMIT` /
    /// `ROLLBACK` over the `X-Trino-Transaction-Id` headers — so this is a
    /// driver limitation, not a platform one. Implementing it means changing
    /// all of the following together, since each is observable and they must
    /// agree:
    ///
    /// - `SQL_TXN_CAPABLE`, reported from `info::trino_get_info`, currently
    ///   `SQL_TC_NONE`
    /// - [`Backend::default_txn_isolation`] and
    ///   [`Backend::txn_isolation_options`], both currently `0`
    /// - [`Backend::set_txn_isolation`], whose default is unreachable while
    ///   the options bitmask is `0`
    /// - [`Backend::set_autocommit`], which currently reports `HYC00` for
    ///   manual-commit mode
    /// - [`Backend::cursor_commit_behavior`] / `cursor_rollback_behavior`,
    ///   left at core's `Preserve` default because no transaction ever begins
    fn end_tran(_conn: &TrinoConnection, _commit: bool) -> Result<(), TrinoError> {
        tracing::debug!("TrinoBackend::end_tran (no-op: this driver has no transaction support)");
        Ok(())
    }

    /// Trino names a query only once the coordinator accepts it, so the token
    /// is the empty shared slot [`Backend::cancel_token`] describes for exactly
    /// that case: the client and runtime are captured here, and whichever
    /// statement-producing call submits the query fills in the id.
    fn cancel_token(conn: &TrinoConnection) -> TrinoCancelToken {
        tracing::debug!("TrinoBackend::cancel_token");
        TrinoCancelToken {
            client: Arc::clone(&conn.client),
            runtime: Arc::clone(&conn.runtime),
            state: Arc::new(CancelState::default()),
        }
    }

    fn cancel(token: &TrinoCancelToken) -> Result<(), TrinoError> {
        execute::cancel(token)
    }

    // --- Delegations ---

    fn exec_direct(
        conn: &TrinoConnection,
        cancel: &TrinoCancelToken,
        sql: &str,
    ) -> Result<TrinoStatement, TrinoError> {
        execute::exec_direct(conn, cancel, sql)
    }

    fn prepare(
        conn: &TrinoConnection,
        cancel: &TrinoCancelToken,
        sql: &str,
    ) -> Result<TrinoStatement, TrinoError> {
        execute::prepare(conn, cancel, sql)
    }

    fn execute(
        conn: &TrinoConnection,
        cancel: &TrinoCancelToken,
        stmt: &mut TrinoStatement,
        params: &[ColumnValue],
    ) -> Result<ExecuteOutcome, TrinoError> {
        execute::execute(conn, cancel, stmt, params)
    }

    // --- Capability statements ---
    //
    // Core derives the matching `SQLGetInfo` values from these, so none of
    // them may also be answered from `info::trino_get_info` -- an arm there
    // would shadow the hook for `SQLGetInfo` while the hook kept driving
    // `SQLGetConnectAttr` and the `HY024` validation in `sql_set_connect_attr`.
    //
    // Every value below was measured against a live coordinator, not read off
    // the documentation; the probe for each is named in its doc comment.

    /// Trino qualifies names as `catalog.schema.table`, and this driver's
    /// catalog functions query `information_schema` in a named catalog.
    fn supports_catalogs(_conn: &TrinoConnection) -> bool {
        true
    }

    fn supports_schemas(_conn: &TrinoConnection) -> bool {
        true
    }

    fn alter_table_support(_conn: &TrinoConnection) -> u32 {
        info::TRINO_ALTER_TABLE
    }

    fn outer_join_capabilities(_conn: &TrinoConnection) -> u32 {
        info::TRINO_OUTER_JOIN_CAPABILITIES
    }

    /// `GROUP BY` must contain every non-aggregated column in the select list,
    /// and may contain columns that are not in it — `SELECT a, b ... GROUP BY a`
    /// fails with `EXPRESSION_NOT_AGGREGATE`, while `SELECT a ... GROUP BY a, b`
    /// succeeds. That is `SQL_GB_GROUP_BY_CONTAINS_SELECT` exactly.
    fn group_by(_conn: &TrinoConnection) -> u16 {
        SQL_GB_GROUP_BY_CONTAINS_SELECT
    }

    /// Trino's default null ordering is `NULLS LAST` *regardless of the
    /// ordering direction* — `ORDER BY x` and `ORDER BY x DESC` both place
    /// NULLs last. `SQL_NC_END` is the value for that; `SQL_NC_HIGH`, which
    /// this driver reported before, means the position follows `ASC`/`DESC`.
    ///
    /// <https://trino.io/docs/current/sql/select.html>
    fn null_collation(_conn: &TrinoConnection) -> u16 {
        SQL_NC_END
    }

    /// Table correlation names are supported and unrestricted:
    /// `FROM (VALUES 1) AS x(a)` binds a name unrelated to the table's own.
    fn correlation_name(_conn: &TrinoConnection) -> u16 {
        SQL_CN_ANY
    }

    /// The connection-string keywords whose values are bearer tokens.
    ///
    /// Core keeps a substring heuristic underneath that already catches all
    /// three of this driver's secrets (`password`, and `token` for both spellings
    /// below), so declaring them redacts nothing that leaked before. It states
    /// the driver's own vocabulary explicitly instead of relying on another
    /// crate's pattern list to keep matching it, and it is what makes a
    /// `{:?}` on the `ConnectParams` handed to `connect` redact too.
    ///
    /// `Password` is core's own spec-defined keyword, redacted there by name.
    fn sensitive_connect_keywords() -> Cow<'static, [Cow<'static, str>]> {
        use types::connect_params::{PARAM_ACCESS_TOKEN, PARAM_TOKEN};
        Cow::Borrowed(&[
            Cow::Borrowed(PARAM_ACCESS_TOKEN),
            Cow::Borrowed(PARAM_TOKEN),
        ])
    }

    /// Trino folds unquoted identifiers to lower case and stores them that
    /// way, so `SELECT * FROM Foo` and `SELECT * FROM foo` name the same
    /// table and `SQLTables` reports it as `foo`.
    ///
    /// This is what an application reads to decide how to quote generated
    /// SQL, which is why core requires the hook rather than defaulting it:
    /// every one of the four `SQL_IC_*` values is a different claim about how
    /// the data source folds identifiers, and no default can be legal.
    ///
    /// `SQL_QUOTED_IDENTIFIER_CASE` is separately `SQL_IC_SENSITIVE` — a
    /// double-quoted identifier keeps its case — and is answered by core's
    /// `common_get_info_raw`.
    ///
    /// <https://trino.io/docs/current/language/reserved.html>
    fn identifier_case(_conn: &TrinoConnection) -> u16 {
        SQL_IC_LOWER
    }

    /// `ALTER TABLE ... ADD COLUMN f integer NOT NULL` is accepted, so the
    /// `NOT NULL` column constraint is supported.
    fn non_nullable_columns(_conn: &TrinoConnection) -> u16 {
        SQL_NNC_NON_NULL
    }

    /// `ORDER BY lower(s)` is accepted, not just bare column references.
    fn expressions_in_order_by(_conn: &TrinoConnection) -> bool {
        true
    }

    /// Trino conforms to no SQL-92 level this info type can name.
    ///
    /// Entry level requires referential integrity in `CREATE TABLE`, and
    /// Trino's grammar rejects all four constraint forms outright — `PRIMARY
    /// KEY`, `UNIQUE`, `CHECK` and `REFERENCES` each fail with `SYNTAX_ERROR`
    /// — which is also why this driver reports `SQL_INTEGRITY = "N"`. Entry
    /// level further requires `COMMIT`/`ROLLBACK`, and this driver reports
    /// `SQL_TC_NONE` (see [`TrinoBackend::end_tran`]).
    ///
    /// `0` is not one of the four `SQL_SC_*` values — the spec's list has no
    /// "conforms to nothing" entry — but it is the only honest answer when the
    /// lowest named level is not met. Claiming `SQL_SC_SQL92_ENTRY` would be
    /// the overstatement the capability hooks exist to prevent.
    ///
    /// Note that [`Backend::group_by`] reporting
    /// `SQL_GB_GROUP_BY_CONTAINS_SELECT` is *not* a reason to avoid entry
    /// level: `CONTAINS_SELECT` is strictly more permissive than the
    /// `EQUALS_SELECT` the spec names for an entry-level driver, and the spec
    /// itself directs applications to read the general level here and "use the
    /// other information types to determine variations from the stated
    /// standards compliance level".
    fn sql_conformance(_conn: &TrinoConnection) -> u32 {
        0
    }

    /// The units `{fn TIMESTAMPADD}` / `{fn TIMESTAMPDIFF}` accept, which is
    /// every unit `crate::escape_dialect::trino_interval_unit` can rewrite —
    /// the two lists are the same list, and a unit named here that the
    /// dialect declines would be a claim an application cannot use.
    ///
    /// `SQL_FN_TSI_FRAC_SECOND` is deliberately absent. ODBC defines it as
    /// billionths of a second; Trino's `date_add`/`date_diff` reject
    /// `nanosecond` and their finest unit is `millisecond`, which ODBC has no
    /// bit for. Claiming it would be a factor of a million out.
    fn timedate_add_intervals(_conn: &TrinoConnection) -> u32 {
        TRINO_TIMESTAMP_INTERVALS
    }

    fn timedate_diff_intervals(_conn: &TrinoConnection) -> u32 {
        TRINO_TIMESTAMP_INTERVALS
    }

    /// Every `SQL_SQ_*` predicate accepts a subquery, correlated ones
    /// included — each of `= (SELECT ...)`, `= ANY (SELECT ...)`,
    /// `<= ALL (SELECT ...)`, `IN (SELECT ...)` and `EXISTS (SELECT ...)`
    /// runs, as does a subquery referencing the outer query's row.
    fn subqueries(_conn: &TrinoConnection) -> u32 {
        SQL_SQ_COMPARISON
            | SQL_SQ_EXISTS
            | SQL_SQ_IN
            | SQL_SQ_QUANTIFIED
            | SQL_SQ_CORRELATED_SUBQUERIES
    }

    fn column_alias(_conn: &TrinoConnection) -> bool {
        true
    }

    /// `concat('a', NULL)` and `'a' || NULL` both evaluate to NULL, which is
    /// `SQL_CB_NULL`.
    fn concat_null_behavior(_conn: &TrinoConnection) -> u16 {
        SQL_CB_NULL
    }

    fn union_support(_conn: &TrinoConnection) -> u32 {
        SQL_U_UNION | SQL_U_UNION_ALL
    }

    /// `CAST` only. Trino has no `CONVERT` scalar function —
    /// `CONVERT('1', INTEGER)` fails to resolve.
    fn convert_functions(_conn: &TrinoConnection) -> u32 {
        SQL_FN_CVT_CAST
    }

    /// `false`: `SELECT b FROM t ORDER BY a` runs, so a column may be ordered
    /// by without being selected.
    fn order_by_columns_in_select(_conn: &TrinoConnection) -> bool {
        false
    }

    /// `false`. Trino *can* filter `information_schema` by privilege, but only
    /// when the deployment configures access control — with the default
    /// allow-all it does not, and the driver cannot tell which it is talking
    /// to. `SQL_ACCESSIBLE_TABLES = "Y"` is a guarantee about the connected
    /// principal, so it must not be made on a maybe.
    fn accessible_tables(_conn: &TrinoConnection) -> bool {
        false
    }

    /// `false`: writes reach whichever connector backs the catalog —
    /// `CREATE TABLE`, `INSERT` and `DROP TABLE` all run against the
    /// PostgreSQL catalog in the test stack.
    fn data_source_read_only(_conn: &TrinoConnection) -> bool {
        false
    }

    /// Backslash, which is what `metadata.rs` emits: a catalog-function
    /// pattern containing a wildcard becomes `LIKE '...' ESCAPE '\'`.
    fn search_pattern_escape(_conn: &TrinoConnection) -> Cow<'static, str> {
        Cow::Borrowed("\\")
    }

    /// Trino's reserved words, raw — core subtracts ODBC's own, sorts and
    /// joins them into `SQL_KEYWORDS`. See [`info::TRINO_RESERVED_KEYWORDS`]
    /// for where the list comes from and why it is static rather than probed.
    fn keywords(_conn: &TrinoConnection) -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(info::reserved_keywords())
    }

    /// `0`, the spec's value for a data source that does not support
    /// transactions, matching the `SQL_TC_NONE` this driver reports. See
    /// [`TrinoBackend::end_tran`] for why that is a driver limitation rather
    /// than a Trino one.
    fn default_txn_isolation(_conn: &TrinoConnection) -> u32 {
        0
    }

    /// `0`: with no supported level, core's `validate_txn_isolation` rejects
    /// every `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` with `HY024`, and
    /// [`Backend::set_txn_isolation`] is never reached.
    fn txn_isolation_options(_conn: &TrinoConnection) -> u32 {
        0
    }

    fn get_info(
        conn: &TrinoConnection,
        info_type: stackable_odbc_core::types::InfoType,
    ) -> Result<InfoValue, TrinoError> {
        info::get_info(conn, info_type)
    }

    fn get_info_pre_connect(
        info_type: stackable_odbc_core::types::InfoType,
    ) -> Result<InfoValue, TrinoError> {
        info::get_info_pre_connect(info_type)
    }

    fn get_info_raw(
        conn: &TrinoConnection,
        info_type: u16,
    ) -> Option<Result<InfoValue, TrinoError>> {
        info::get_info_raw(conn, info_type)
    }

    fn get_functions() -> Cow<'static, [stackable_odbc_core::function_id::FunctionId]> {
        Cow::Borrowed(info::get_functions())
    }

    fn get_type_info(_conn: &TrinoConnection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(info::get_type_info())
    }

    // The six catalog functions, and the two enumerations that do I/O, take a
    // cancel token they do not record anything in, so `SQLCancel` cannot
    // interrupt them.
    //
    // Four of them (`primary_keys`, `foreign_keys`, `statistics`,
    // `special_columns`) return no rows without touching the network, so there
    // is nothing to cancel. `tables`, `columns`, `catalogs` and `schemas` do
    // query Trino, but through `query_all_rows` -> `Client::get_all`, which
    // pages to exhaustion inside the client and never surfaces the query id a
    // DELETE needs. Making them cancellable means replacing that with manual
    // paging.
    fn tables(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        table_type: Option<&str>,
    ) -> Result<Vec<TableRow>, TrinoError> {
        metadata::tables(conn, catalog, schema, table, table_type)
    }

    /// The two `information_schema.tables.table_type` values `metadata::tables`
    /// maps to an ODBC `TABLE_TYPE`; it drops every other row, so this is the
    /// complete list of types a `SQLTables` result set can carry.
    fn table_types(_conn: &TrinoConnection) -> Vec<Cow<'static, str>> {
        metadata::table_types()
    }

    /// Required rather than defaulted because [`Self::supports_catalogs`]
    /// answers `true`: a backend that claims catalogs and leaves this alone
    /// answers `HYC00` to `SQLTables`' `SQL_ALL_CATALOGS` enumeration.
    fn catalogs(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
    ) -> Result<Vec<String>, TrinoError> {
        metadata::catalogs(conn)
    }

    /// Required for the same reason as [`Self::catalogs`], against
    /// [`Self::supports_schemas`].
    fn schemas(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
    ) -> Result<Vec<String>, TrinoError> {
        metadata::schemas(conn)
    }

    fn columns(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        column: Option<&str>,
    ) -> Result<Vec<ColumnRow>, TrinoError> {
        metadata::columns(conn, catalog, schema, table, column)
    }

    fn primary_keys(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<PrimaryKeyRow>, TrinoError> {
        metadata::primary_keys(conn, catalog, schema, table)
    }

    #[allow(clippy::too_many_arguments)]
    fn foreign_keys(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
        pk_catalog: Option<&str>,
        pk_schema: Option<&str>,
        pk_table: Option<&str>,
        fk_catalog: Option<&str>,
        fk_schema: Option<&str>,
        fk_table: Option<&str>,
    ) -> Result<Vec<ForeignKeyRow>, TrinoError> {
        metadata::foreign_keys(
            conn, pk_catalog, pk_schema, pk_table, fk_catalog, fk_schema, fk_table,
        )
    }

    fn statistics(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        unique_only: bool,
    ) -> Result<Vec<StatisticsRow>, TrinoError> {
        metadata::statistics(conn, catalog, schema, table, unique_only)
    }

    #[allow(clippy::too_many_arguments)]
    fn special_columns(
        conn: &TrinoConnection,
        _cancel: &TrinoCancelToken,
        identifier_type: IdentifierType,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        scope: Scope,
        nullable: Nullable,
    ) -> Result<Vec<SpecialColumnRow>, TrinoError> {
        metadata::special_columns(
            conn,
            identifier_type,
            catalog,
            schema,
            table,
            scope,
            nullable,
        )
    }

    /// Trino's `{fn}`/`{d}`/`{t}`/`{ts}` escape-translation dialect. See
    /// `crate::escape_dialect` for the remap table and its justification
    /// against the `SQL_*_FUNCTIONS` bitmaps in `backend/info.rs`.
    fn escape_dialect(_conn: &TrinoConnection) -> stackable_odbc_core::escape::EscapeDialect {
        crate::escape_dialect::dialect()
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn jwt_over_https_selected() {
        let a = resolve_auth(true, "u", None, Some("tok")).unwrap();
        assert!(matches!(a, Some(Auth::Jwt(t)) if t == "tok"));
    }

    #[test]
    fn jwt_over_http_rejected() {
        let e = resolve_auth(false, "u", None, Some("tok")).unwrap_err();
        assert!(matches!(e, TrinoError::AuthConfig { .. }), "got {e:?}");
    }

    #[test]
    fn token_and_password_rejected() {
        let e = resolve_auth(true, "u", Some("pw"), Some("tok")).unwrap_err();
        assert!(matches!(e, TrinoError::AuthConfig { .. }), "got {e:?}");
    }

    #[test]
    fn basic_over_https_when_password_only() {
        let a = resolve_auth(true, "u", Some("pw"), None).unwrap();
        assert!(matches!(a, Some(Auth::Basic(u, Some(p))) if u == "u" && p == "pw"));
    }

    #[test]
    fn basic_over_https_when_no_credentials_preserves_prior_behavior() {
        let a = resolve_auth(true, "u", None, None).unwrap();
        assert!(matches!(a, Some(Auth::Basic(u, None)) if u == "u"));
    }

    #[test]
    fn no_auth_when_neither() {
        assert!(resolve_auth(false, "u", None, None).unwrap().is_none());
        // password over http is dropped upstream in connect(), not here:
        assert!(
            resolve_auth(false, "u", Some("pw"), None)
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use stackable_odbc_core::{
        backend::StatementBackend,
        types::{ColumnValue, FetchResult},
    };

    use super::*;

    #[test]
    fn get_type_info_returns_all_trino_types() {
        let conn = disconnected_trino_conn();
        let types = TrinoBackend::get_type_info(&conn);
        assert!(!types.is_empty(), "should return at least one type");

        let names: Vec<&str> = types.iter().map(|t| t.type_name()).collect();
        for expected in &[
            "BOOLEAN",
            "TINYINT",
            "SMALLINT",
            "INTEGER",
            "BIGINT",
            "REAL",
            "DOUBLE",
            "DECIMAL",
            "VARCHAR",
            "CHAR",
            "VARBINARY",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "JSON",
            "UUID",
        ] {
            assert!(names.contains(expected), "missing type: {expected}");
        }

        // Datetime types must have sql_data_type=9 (SQL_DATETIME) and a sub-type code
        let date = types.iter().find(|t| t.type_name() == "DATE").unwrap();
        assert_eq!(date.sql_data_type(), 9);
        assert_eq!(date.sql_datetime_sub(), Some(1));

        let time = types.iter().find(|t| t.type_name() == "TIME").unwrap();
        assert_eq!(time.sql_data_type(), 9);
        assert_eq!(time.sql_datetime_sub(), Some(2));

        let ts = types.iter().find(|t| t.type_name() == "TIMESTAMP").unwrap();
        assert_eq!(ts.sql_data_type(), 9);
        assert_eq!(ts.sql_datetime_sub(), Some(3));

        // Integer types must have num_prec_radix=10 and unsigned=false
        let bigint = types.iter().find(|t| t.type_name() == "BIGINT").unwrap();
        assert_eq!(bigint.num_prec_radix(), Some(10));
        assert_eq!(bigint.unsigned(), Some(false));
    }

    // -----------------------------------------------------------------------
    // ODBC special catalog enumeration mode tests
    // -----------------------------------------------------------------------

    /// The `SQL_ALL_TABLE_TYPES` enumeration is a static declaration and needs
    /// no Trino connection. Core turns these into the result set — every
    /// column but `TABLE_TYPE` NULL — so this pins the values, not the shape.
    ///
    /// Upper case is spec-mandated: applications specify table types in upper
    /// case and the driver maps them to whatever the data source needs.
    #[test]
    fn table_types_are_table_and_view_in_upper_case() {
        assert_eq!(metadata::table_types(), vec!["TABLE", "VIEW"]);
    }

    /// The `LIKE ''` filter must return nothing even for a catalog with many
    /// schemas: the probe proves the catalog resolves, it is not a listing.
    #[test]
    #[ignore = "requires Trino at localhost:8080 -- run with: cargo test -- --ignored backend"]
    fn validation_query_returns_no_rows_for_a_populated_catalog() {
        let params = ConnectParams::parse("Host=localhost;Port=8080;User=test").unwrap();
        let conn = TrinoBackend::connect(&params).unwrap();
        let rows = query_all_rows(&conn, validation_query(Some("tpcds"))).unwrap();
        assert!(rows.is_empty(), "probe returned {} rows", rows.len());
    }

    /// The validation query resolves the session catalog, so a catalog that
    /// does not exist is caught at connect rather than at the first query.
    #[test]
    #[ignore = "requires Trino at localhost:8080 -- run with: cargo test -- --ignored backend"]
    fn connect_with_unknown_catalog_fails_with_08001() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};

        let params =
            ConnectParams::parse("Host=localhost;Port=8080;User=test;Catalog=no_such_catalog")
                .unwrap();
        let Err(err) = TrinoBackend::connect(&params) else {
            panic!("connect with an unknown catalog must fail");
        };
        assert_eq!(
            OdbcError::from(err).sqlstate().as_str(),
            sql_state::CLIENT_UNABLE_TO_ESTABLISH_CONNECTION
        );
    }

    /// Ignored because `connect` now validates the connection with a real
    /// query: a successful connect requires a live Trino.
    #[test]
    #[ignore = "requires Trino at localhost:8080 -- run with: cargo test -- --ignored backend"]
    fn connect_creates_runtime() {
        let params =
            ConnectParams::parse("Host=localhost;Port=8080;User=test;Password=test").unwrap();
        let mut conn = TrinoBackend::connect(&params).unwrap();
        TrinoBackend::disconnect(&mut conn).unwrap();
    }

    #[test]
    fn connect_missing_host_returns_error() {
        let params = ConnectParams::parse("Port=8080;User=admin;Password=admin").unwrap();
        assert!(TrinoBackend::connect(&params).is_err());
    }

    #[test]
    fn connect_invalid_port_returns_error() {
        let params = ConnectParams::parse("Host=localhost;Port=notanumber;User=admin").unwrap();
        assert!(TrinoBackend::connect(&params).is_err());
    }

    // These assert on the parsed parameters rather than calling `connect`:
    // `connect` now validates the connection with a real query, so it cannot
    // succeed without a live Trino, and what these cover is the parsing.

    #[test]
    fn connect_custom_query_timeout_accepted() {
        let params =
            ConnectParams::parse("Host=localhost;Port=8080;User=test;QueryTimeout=60").unwrap();
        let p = types::connect_params::TrinoConnectParams::try_from(&params).unwrap();
        assert_eq!(p.query_timeout().as_secs(), 60);
    }

    #[test]
    fn connect_login_timeout_alias_accepted() {
        let params =
            ConnectParams::parse("Host=localhost;Port=8080;User=test;LoginTimeout=10").unwrap();
        let p = types::connect_params::TrinoConnectParams::try_from(&params).unwrap();
        assert_eq!(p.query_timeout().as_secs(), 10);
    }

    #[test]
    fn validation_query_without_catalog_is_catalog_free() {
        assert_eq!(validation_query(None), "SELECT 1");
    }

    #[test]
    fn validation_query_with_catalog_resolves_it() {
        assert_eq!(
            validation_query(Some("tpcds")),
            r#"SHOW SCHEMAS FROM "tpcds" LIKE ''"#
        );
    }

    #[test]
    fn validation_query_escapes_quotes_in_catalog_name() {
        assert_eq!(
            validation_query(Some(r#"we"ird"#)),
            r#"SHOW SCHEMAS FROM "we""ird" LIKE ''"#
        );
    }

    #[test]
    fn error_mapping_connection_failed_produces_08001() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};
        let err = TrinoError::ConnectionFailed {
            message: "connection refused".into(),
        };
        let odbc_err: OdbcError = err.into();
        assert_eq!(
            odbc_err.sqlstate().as_str(),
            sql_state::CLIENT_UNABLE_TO_ESTABLISH_CONNECTION
        );
    }

    /// An unreachable coordinator must be reported by `connect` itself, not
    /// deferred to the application's first query.
    #[test]
    fn connect_to_unreachable_server_fails_with_08001() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};

        // Port 1 is reserved and never listening; the client does not retry
        // connection-refused, so this fails fast.
        let params = ConnectParams::parse("Host=127.0.0.1;Port=1;User=test").unwrap();
        let Err(err) = TrinoBackend::connect(&params) else {
            panic!("connect to an unreachable server must fail");
        };
        assert_eq!(
            OdbcError::from(err).sqlstate().as_str(),
            sql_state::CLIENT_UNABLE_TO_ESTABLISH_CONNECTION
        );
    }

    /// A declaration naming a keyword this driver does not actually read is a
    /// silent no-op, so it is asserted against the constants the parser uses
    /// rather than against string literals repeated here.
    ///
    /// `Password` is absent deliberately: it is core's own spec-defined
    /// keyword, redacted there by name rather than by this hook.
    #[test]
    fn every_trino_specific_secret_keyword_is_declared_sensitive() {
        use types::connect_params::{PARAM_ACCESS_TOKEN, PARAM_TOKEN};

        let declared = TrinoBackend::sensitive_connect_keywords();
        for key in [PARAM_ACCESS_TOKEN, PARAM_TOKEN] {
            assert!(
                declared.iter().any(|d| d == key),
                "{key} carries a bearer token but is not declared sensitive; declared: {declared:?}"
            );
        }
    }

    /// Builds a Trino server-side query error with the given code and name.
    fn query_error(error_code: i32, error_name: &str) -> trino_rust_client::models::QueryError {
        trino_rust_client::models::QueryError {
            message: "line 1:8: mismatched input 'FROM'".into(),
            sql_state: None,
            error_code,
            error_name: error_name.into(),
            error_type: "USER_ERROR".into(),
            error_location: None,
            failure_info: None,
        }
    }

    /// `SQLGetDiagRec` reports the native error through `NativeErrorPtr`, and
    /// Trino's own error taxonomy is the only thing that can meaningfully go
    /// there. Zero for every failure tells an application nothing.
    #[test]
    fn a_query_error_carries_trinos_own_code_as_the_native_error() {
        use stackable_odbc_core::errors::OdbcError;

        // SYNTAX_ERROR is Trino error code 1.
        let odbc_err = OdbcError::from(map_trino_error(query_error(1, "SYNTAX_ERROR").into()));

        assert_eq!(odbc_err.native_error(), 1);
    }

    /// The diagnostic message is built from the whole causal chain, so the
    /// client error has to stay attached rather than being flattened into a
    /// string at the point of mapping.
    #[test]
    fn a_query_error_keeps_the_client_error_as_its_cause() {
        use stackable_odbc_core::errors::OdbcError;

        let odbc_err = OdbcError::from(map_trino_error(query_error(1, "SYNTAX_ERROR").into()));

        let cause = odbc_err.cause().expect("the client error must be retained");
        assert!(
            cause.to_string().contains("SYNTAX_ERROR"),
            "the cause must name the Trino error, got: {cause}"
        );
    }

    /// A failure that never came from the coordinator has no Trino code, and
    /// `0` is the spec's value for "no native code" rather than a made-up one.
    #[test]
    fn an_error_without_a_trino_code_reports_zero() {
        use stackable_odbc_core::errors::OdbcError;

        let odbc_err = OdbcError::from(TrinoError::General {
            message: "no runtime available".into(),
        });

        assert_eq!(odbc_err.native_error(), 0);
    }

    #[test]
    fn error_mapping_communication_link_failure_produces_08s01() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};
        let err = TrinoError::CommunicationLinkFailure {
            message: "connection refused".into(),
        };
        let odbc_err: OdbcError = err.into();
        assert_eq!(
            odbc_err.sqlstate().as_str(),
            sql_state::COMMUNICATION_LINK_FAILURE
        );
    }

    #[test]
    fn error_mapping_query_timeout_produces_hyt00() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};
        let err = TrinoError::QueryTimeout {
            message: "timed out".into(),
        };
        let odbc_err: OdbcError = err.into();
        assert_eq!(odbc_err.sqlstate().as_str(), sql_state::TIMEOUT_EXPIRED);
    }

    #[test]
    fn error_mapping_auth_failure_produces_28000() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};
        let err = TrinoError::AuthFailure {
            message: "HTTP 401: Unauthorized".into(),
        };
        let odbc_err: OdbcError = err.into();
        assert_eq!(odbc_err.sqlstate().as_str(), sql_state::INVALID_AUTH_SPEC);
    }

    #[test]
    fn error_mapping_auth_config_produces_28000() {
        use stackable_odbc_core::{errors::OdbcError, types::sql_state};
        let err = TrinoError::AuthConfig {
            message: "both a password and an access token were supplied; provide only one".into(),
        };
        let odbc_err: OdbcError = err.into();
        assert_eq!(odbc_err.sqlstate().as_str(), sql_state::INVALID_AUTH_SPEC);
    }

    // -----------------------------------------------------------------------
    // Integration tests (require a live Trino instance)
    // -----------------------------------------------------------------------
    //
    // All integration tests share a single TrinoConnection (via OnceLock).
    // This mirrors production usage where one ODBC connection serves many
    // queries, and avoids rapid connect/disconnect cycles that expose Trino
    // coordinator timing sensitivity between independent reqwest connection
    // pools.

    fn shared_trino_conn() -> &'static TrinoConnection {
        use std::sync::OnceLock;
        static CONN: OnceLock<TrinoConnection> = OnceLock::new();
        CONN.get_or_init(|| {
            let params = ConnectParams::parse(
                "Host=localhost;Port=8080;User=admin;Password=admin;Catalog=tpcds",
            )
            .expect("parse params");
            TrinoBackend::connect(&params).expect("shared backend connection")
        })
    }

    // These tests create a separate TrinoConnection (with its own reqwest
    // pool) from the FFI integration tests. Running both groups against the
    // same Trino coordinator causes intermittent failures because the two
    // pools' TCP sockets can interfere at the server level. Use
    // `cargo test -- --ignored backend` to
    // run them in isolation. The FFI integration tests provide equivalent
    // coverage via the ODBC call stack.

    #[test]
    #[serial(backend)]
    #[ignore = "requires Trino -- run in isolation: cargo test -- --ignored backend"]
    fn exec_direct_select_and_fetch() {
        use stackable_odbc_core::types::CDataType;
        let conn = shared_trino_conn();
        let cancel = TrinoBackend::cancel_token(conn);
        let mut stmt =
            TrinoBackend::exec_direct(conn, &cancel, "SELECT 1 AS n").expect("exec_direct");

        assert_eq!(stmt.column_count(), 1);
        assert_eq!(stmt.describe_col(1).expect("describe_col").name(), "n");

        assert_eq!(stmt.fetch().expect("fetch"), FetchResult::Row);
        let val = stmt.get_data(1, CDataType::SLong).expect("get_data");
        assert!(
            matches!(*val, ColumnValue::I32(1) | ColumnValue::I64(1)),
            "expected 1, got {val:?}"
        );
        assert_eq!(stmt.fetch().expect("fetch 2"), FetchResult::NoData);
    }

    #[test]
    #[serial(backend)]
    #[ignore = "requires Trino -- run in isolation: cargo test -- --ignored backend"]
    fn streaming_large_result_fetches_all_pages() {
        use stackable_odbc_core::types::CDataType;
        let conn = shared_trino_conn();
        let cancel = TrinoBackend::cancel_token(conn);
        let mut stmt = TrinoBackend::exec_direct(
            conn,
            &cancel,
            "SELECT c_customer_sk FROM tpcds.sf1.customer WHERE c_customer_sk <= 15000",
        )
        .expect("exec_direct");

        assert_eq!(stmt.column_count(), 1);

        let mut count = 0usize;
        while let FetchResult::Row = stmt.fetch().expect("fetch") {
            let val = stmt.get_data(1, CDataType::SLong).expect("get_data");
            assert!(
                matches!(*val, ColumnValue::I32(_) | ColumnValue::I64(_)),
                "unexpected value: {val:?}"
            );
            count += 1;
        }
        assert!(count >= 10_000, "expected >= 10,000 rows, got {count}");
    }

    #[test]
    #[serial(backend)]
    #[ignore = "requires Trino -- run in isolation: cargo test -- --ignored backend"]
    fn cancel_mid_stream() {
        // This test uses its own connection (not shared_trino_conn) because
        // cancel leaves the reqwest connection pool with a dirty TCP socket
        // that has unread response bytes. Using a separate connection ensures
        // the dirty pool is destroyed when this test ends, rather than
        // poisoning subsequent tests on the shared connection.
        let params = ConnectParams::parse(
            "Host=localhost;Port=8080;User=admin;Password=admin;Catalog=tpcds",
        )
        .expect("parse params");
        let conn = TrinoBackend::connect(&params).expect("connect");
        let cancel = TrinoBackend::cancel_token(&conn);
        let mut stmt = TrinoBackend::exec_direct(
            &conn,
            &cancel,
            "SELECT c_customer_sk FROM tpcds.sf1.customer",
        )
        .expect("exec_direct");

        // Fetch a few rows to ensure the query is running.
        for _ in 0..5 {
            assert_eq!(stmt.fetch().expect("fetch"), FetchResult::Row);
        }

        // Cancel should succeed.
        let result = TrinoBackend::cancel(&cancel);
        assert!(result.is_ok(), "cancel failed: {result:?}");

        // `cancel` no longer holds the statement, so it publishes the
        // cancellation through the token instead of clearing `next_uri`
        // directly. The statement must observe that on its next fetch and stop
        // polling: draining a cancelled query is what corrupts the pool.
        assert_eq!(
            stmt.fetch().expect("fetch after cancel"),
            FetchResult::NoData,
            "a cancelled statement must report NoData rather than resume paging"
        );
        assert!(
            stmt.next_uri.is_none(),
            "the cancelled statement must have dropped its next page URI"
        );
    }

    /// The scenario `Backend::CancelToken` exists for: `SQLCancel` arriving on
    /// a different thread from the one executing the statement.
    ///
    /// The previous `cancel(&mut Self::Statement)` signature could not express
    /// this at all — the executing thread holds that `&mut`. It also exercises
    /// two threads inside `block_on` on the same *current-thread* runtime,
    /// which is the shape this driver's Tokio bridge has and the one place
    /// this design could plausibly deadlock rather than cancel.
    #[test]
    #[serial(backend)]
    #[ignore = "requires Trino -- run in isolation: cargo test -- --ignored backend"]
    fn cancel_from_another_thread_while_fetching() {
        // Its own connection, for the dirty-socket reason `cancel_mid_stream`
        // documents above.
        let params = ConnectParams::parse(
            "Host=localhost;Port=8080;User=admin;Password=admin;Catalog=tpcds",
        )
        .expect("parse params");
        let conn = TrinoBackend::connect(&params).expect("connect");
        let cancel = Arc::new(TrinoBackend::cancel_token(&conn));

        // A query large enough that it is still streaming when the cancel
        // lands, so the cancelling thread really does contend for the runtime.
        let mut stmt = TrinoBackend::exec_direct(
            &conn,
            &cancel,
            "SELECT c_customer_sk FROM tpcds.sf1.customer",
        )
        .expect("exec_direct");

        assert_eq!(stmt.fetch().expect("first fetch"), FetchResult::Row);

        let canceller = {
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || TrinoBackend::cancel(&cancel))
        };

        // Keep fetching until the cancellation is observed. This must
        // terminate: either the rows run out or the cancellation stops the
        // loop. A hang here is the deadlock this test is looking for.
        let mut rows = 0u64;
        let outcome = loop {
            match stmt.fetch() {
                Ok(FetchResult::Row) => rows += 1,
                other => break other,
            }
        };

        let cancelled = canceller.join().expect("cancelling thread panicked");
        assert!(
            cancelled.is_ok(),
            "cross-thread cancel failed: {cancelled:?}"
        );

        // Both endings are correct, and which one occurs depends on whether a
        // page request happened to be in flight when the DELETE landed:
        //
        // - in flight: the coordinator fails that request with USER_CANCELED,
        //   and the spec's HY008 is what must reach the application;
        // - between requests: nothing was interrupted, so the statement simply
        //   reports the result set as finished.
        //
        // Accepting either is not slack in the assertion — pinning one would
        // make the test flaky on a timing detail neither the driver nor the
        // application controls. What is asserted unconditionally is that the
        // stream stopped early, below.
        match outcome {
            Err(e) => {
                let odbc = OdbcError::from(e);
                assert_eq!(
                    odbc.sqlstate(),
                    stackable_odbc_core::types::SqlState::new(SQL_STATE_CANCELLED),
                    "a fetch interrupted by SQLCancel must report HY008, got {odbc:?}"
                );
            }
            Ok(FetchResult::NoData) => {}
            Ok(other) => panic!("unexpected fetch outcome after cancel: {other:?}"),
        }

        // The proof the cancel actually took effect. tpcds.sf1.customer holds
        // 100,000 rows; had the cancel been a no-op the loop would have drained
        // all of them and every assertion above would pass vacuously.
        const CUSTOMER_ROWS: u64 = 100_000;
        assert!(
            rows < CUSTOMER_ROWS,
            "the cancel did not stop the stream: fetched all {rows} rows"
        );
        assert!(
            stmt.next_uri.is_none(),
            "the statement must not be left holding a next page URI, {rows} rows in"
        );

        // Not `fetch_failed`: a cancellation is not a failed fetch, so the
        // statement reports a finished result set rather than 24000.
        assert_eq!(
            stmt.fetch().expect("fetch after a cancelled fetch"),
            FetchResult::NoData
        );
    }

    /// Guards SQL_DBMS_VER: it must be queried from the server, not a frozen
    /// literal such as "467", which is wrong against every coordinator that is
    /// not 467 and malformed against the spec's ##.##.#### requirement
    /// regardless.
    #[test]
    #[ignore = "requires Trino at localhost:8080 -- run with: cargo test -- --ignored backend"]
    fn dbms_version_is_read_from_the_server() {
        let params = ConnectParams::parse("Host=localhost;Port=8080;User=test").unwrap();
        let conn = TrinoBackend::connect(&params).unwrap();
        assert!(
            !conn.dbms_version.is_empty(),
            "the version probe returned nothing"
        );
        let prefix = conn.dbms_version.split(' ').next().unwrap_or("");
        let parts: Vec<&str> = prefix.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "SQL_DBMS_VER must start with ##.##.####, got {:?}",
            conn.dbms_version
        );
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
            "SQL_DBMS_VER prefix must be all digits and dots: {:?}",
            conn.dbms_version
        );
    }
}
