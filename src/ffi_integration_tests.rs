//! FFI-level integration tests for the Trino backend.
//!
//! All tests require Trino running at localhost:8443.
//! Start with: `./integration-tests/setup.sh`
//!
//! Run with: `cargo test -- --ignored ffi_integration_tests`
//!
//! All tests share a single ODBC connection via [`SHARED_CONN`] (created once
//! via `OnceLock`). This mirrors production usage where one connection serves
//! many queries, and avoids creating multiple `reqwest` connection pools that
//! can interfere with each other on the same Trino coordinator.
//!
//! Tests are marked `#[serial]` (from the `serial_test` crate) to prevent
//! concurrent access to the shared connection. Do NOT run these alongside
//! the `backend::tests` integration tests: they use a separate
//! `TrinoConnection` with its own reqwest pool, and the two pools cause
//! intermittent TCP socket corruption. Run backend tests in isolation:
//! `cargo test -- --ignored backend`

use std::ffi::c_void;
use std::sync::OnceLock;

use serial_test::serial;
use stackable_odbc_core::conformance::{
    all_info_types, genuine_convert_info_types, observe_info_value_kind, observe_u32_value,
};
use stackable_odbc_core::ffi;
// Core's re-export, not a dependency of this crate's own: `odbc-sys` appears in
// the trait signatures core exposes, and two versions of a `#[repr(C)]` type
// are two different types to the compiler. `Timestamp` below is read back out
// of a buffer core wrote, so it has to be core's.
use stackable_odbc_core::odbc_sys;
use stackable_odbc_core::test_support::{attach_connection, detach_connection};
use stackable_odbc_core::types::{
    AttrOdbcVersion, CDataType, Desc, EnvironmentAttribute, HandleType, HeaderDiagnosticIdentifier,
    InfoType, ParamType, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_FETCH_BOOKMARK, SQL_IC_LOWER,
    SQL_INDEX_UNIQUE, SQL_LOCK_NO_CHANGE, SQL_NTS, SQL_NULL_DATA, SQL_PARAM_ERROR,
    SQL_PARAM_SUCCESS, SQL_POSITION, SQL_QUICK, SqlDataType, SqlReturn, StatementAttribute,
    expected_kind,
};

use crate::backend::info::{
    TRINO_AGGREGATE_FUNCTIONS, TRINO_NUMERIC_FUNCTIONS, TRINO_SQL92_VALUE_EXPRESSIONS,
    TRINO_STRING_FUNCTIONS, TRINO_SYSTEM_FUNCTIONS, TRINO_TIMEDATE_FUNCTIONS,
};
use crate::backend::{TrinoBackend, disconnected_trino_conn, disconnected_trino_conn_with_catalog};

/// The coordinator serves HTTPS only, so these tests verify against the test
/// CA. The path is resolved from `CARGO_MANIFEST_DIR` at compile time rather
/// than hardcoded: `generated/` is produced per checkout, and a literal path
/// would only work in one of them.
const CONN_STR: &str = concat!(
    "Host=localhost;Port=8443;Protocol=https;User=admin;Password=admin;Catalog=tpcds;Certificate=",
    env!("CARGO_MANIFEST_DIR"),
    "/integration-tests/generated/certs/ca.crt"
);

// ---------------------------------------------------------------------------
// Shared connection infrastructure
// ---------------------------------------------------------------------------
//
// Most tests need a connected ODBC handle but don't test the connection
// lifecycle itself. Reusing a single env + conn across tests mirrors how a
// real ODBC client works (one connection, many statements) and avoids rapid
// connect/disconnect cycles that expose Trino server-side timing sensitivity.
//
// Tests that specifically exercise connection/disconnection (e.g.
// connect_and_disconnect_lifecycle) use the standalone alloc_handles() +
// connect_trino() + cleanup() helpers instead.

/// Wrapper around raw ODBC handle pointers so they can be stored in OnceLock.
///
/// SAFETY: the raw pointers are heap-allocated ODBC handles that live for the
/// entire test process. They are only accessed by tests running under
/// #[serial], so there is no concurrent mutation.
struct SharedHandles(*mut c_void, *mut c_void);
unsafe impl Sync for SharedHandles {}
unsafe impl Send for SharedHandles {}

/// Process-wide shared env + conn handles, connected once.
static SHARED_CONN: OnceLock<SharedHandles> = OnceLock::new();

/// Returns (env, conn) that are connected to Trino. Created on first call,
/// reused thereafter. Panics if the connection fails.
fn shared_conn() -> (*mut c_void, *mut c_void) {
    let h = SHARED_CONN.get_or_init(|| unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            ),
            SqlReturn::SUCCESS
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Dbc as i16, env, &mut conn,),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            connect_trino(conn),
            SqlReturn::SUCCESS,
            "shared connection failed"
        );
        SharedHandles(env, conn)
    });
    (h.0, h.1)
}

/// Allocate a fresh statement handle on the shared connection.
unsafe fn alloc_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
    let (env, conn) = shared_conn();
    let mut stmt: *mut c_void = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Stmt as i16, conn, &mut stmt,),
            SqlReturn::SUCCESS
        );
    }
    (env, conn, stmt)
}

/// The first diagnostic record on a statement, as `SQLSTATE: message`, or a
/// placeholder when there is none.
///
/// For use in an assertion message: a catalog call that unexpectedly fails
/// says nothing useful on its own, and the SQLSTATE plus the server's message
/// is the difference between "the query was rejected" and a guess.
unsafe fn diag_message(stmt: *mut c_void) -> String {
    unsafe { handle_diag_message(HandleType::Stmt, stmt) }
}

/// The same, for a connection handle. A failed connect leaves its diagnostic
/// on the Dbc, and there is no statement to ask.
unsafe fn conn_diag_message(conn: *mut c_void) -> String {
    unsafe { handle_diag_message(HandleType::Dbc, conn) }
}

unsafe fn handle_diag_message(handle_type: HandleType, handle: *mut c_void) -> String {
    let mut state = [0u16; 6];
    let mut msg = [0u16; 1024];
    let mut msg_len: i16 = 0;
    let mut native: i32 = 0;
    let ret = unsafe {
        ffi::diag::sql_get_diag_rec_w::<TrinoBackend>(
            handle_type as i16,
            handle,
            1,
            state.as_mut_ptr(),
            &mut native,
            msg.as_mut_ptr(),
            msg.len() as i16,
            &mut msg_len,
        )
    };
    if ret != SqlReturn::SUCCESS {
        return "<no diagnostic record>".to_string();
    }
    // The SQLSTATE buffer is a 5-character string plus its NUL terminator.
    let state = String::from_utf16_lossy(&state[..5]);
    let text = String::from_utf16_lossy(&msg[..msg_len as usize]);
    format!("{state}: {text}")
}

/// Free just the statement handle. Drains any in-flight result first.
/// The shared env + conn are left intact for the next test.
unsafe fn cleanup_stmt(stmt: *mut c_void) {
    unsafe {
        while ffi::fetch::sql_fetch::<TrinoBackend>(stmt) == SqlReturn::SUCCESS {}
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
    }
}

// ---------------------------------------------------------------------------
// Standalone helpers for lifecycle tests (allocate + connect + disconnect)
// ---------------------------------------------------------------------------

/// Helper: allocate env + conn + stmt handles using the Trino backend.
unsafe fn alloc_handles() -> (*mut c_void, *mut c_void, *mut c_void) {
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = ffi::handle::sql_alloc_handle::<TrinoBackend>(
            HandleType::Env as i16,
            std::ptr::null_mut(),
            &mut env,
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ =
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Dbc as i16, env, &mut conn);
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ =
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Stmt as i16, conn, &mut stmt);
        (env, conn, stmt)
    }
}

/// Helper: connect to Trino at localhost:8443.
unsafe fn connect_trino(conn: *mut c_void) -> SqlReturn {
    // The terminator is part of the buffer, because the length argument below
    // is SQL_NTS: that tells the driver the string is null-terminated and to
    // find the end itself. Without it the driver reads past this Vec until it
    // meets a zero somewhere in the heap, which appends whatever bytes follow
    // to the last key in the connection string. It is the allocator's leftovers
    // that decide, so the same code connects or reports a certificate path that
    // does not exist depending on what ran before it.
    //
    // Every other wide buffer in this file passes an explicit length instead,
    // and needs no terminator.
    let wide: Vec<u16> = CONN_STR.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ffi::connect::sql_driver_connect_w::<TrinoBackend>(
            conn,
            std::ptr::null_mut(),
            wide.as_ptr(),
            SQL_NTS as i16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        )
    }
}

/// Helper: execute a SQL statement.
unsafe fn exec_direct(stmt: *mut c_void, sql: &str) -> SqlReturn {
    let wide: Vec<u16> = sql.encode_utf16().collect();
    unsafe {
        ffi::execute::sql_exec_direct_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32)
    }
}

/// Helper: free all handles (for lifecycle tests that own their connection).
unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
    unsafe {
        while ffi::fetch::sql_fetch::<TrinoBackend>(stmt) == SqlReturn::SUCCESS {}
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::connect::sql_disconnect::<TrinoBackend>(conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

// ---------------------------------------------------------------------------
// Fallback-chain tests: named InfoType variants with no get_info arm
// ---------------------------------------------------------------------------
//
// SqlFileUsage/SqlQuotedIdentifierCase and the ten PowerBI capability info
// types below have no arm in default_get_info or trino_get_info's match:
// they only get a real value via TrinoBackend::get_info_raw, reached through
// the get_info_raw-first fallback in sql_get_info_w. See the note on
// ordering at info_type_default_response in
// stackable-odbc-core/src/ffi/info.rs.
//
// These need `handle.connection = Some(_)` to reach that fallback at all
// (info_type_default_response skips get_info_raw entirely when conn is
// None), but going through TrinoBackend::connect requires a live Trino
// server, because it validates the connection with a real query. Building
// a TrinoConnection directly and injecting it into the handle sidesteps
// that: ClientBuilder::build() only constructs a reqwest::Client
// synchronously (see TrinoBackend::connect in backend.rs, which performs no
// I/O until the separate validate_connection call), so this test needs no
// live server and is not `#[ignore]`d like the rest of this file.

/// Allocates env + conn handles and injects a network-free `TrinoConnection`
/// directly into the connection handle, bypassing `TrinoBackend::connect`
/// (which requires a live server). This is enough to put `sql_get_info_w` on
/// the connected (`B::get_info` / `B::get_info_raw`) path: the fallback
/// chain under test here never touches the connection's fields.
unsafe fn alloc_conn_with_injected_trino_connection() -> (*mut c_void, *mut c_void) {
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            ),
            SqlReturn::SUCCESS
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Dbc as i16, env, &mut conn),
            SqlReturn::SUCCESS
        );
        attach_connection::<TrinoBackend>(conn, disconnected_trino_conn())
            .expect("valid conn handle");
        (env, conn)
    }
}

/// Frees handles allocated by `alloc_conn_with_injected_trino_connection`.
///
/// The connection is taken back out with `detach_connection` rather than closed
/// with `SQLDisconnect`: the spec has `SQLFreeHandle` refuse a connection handle
/// that still holds a connection (`HY010`), so something must remove it, and
/// this connection never opened a session for `TrinoBackend::disconnect` to
/// close.
unsafe fn cleanup_injected_conn(env: *mut c_void, conn: *mut c_void) {
    unsafe {
        let _ = detach_connection::<TrinoBackend>(conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// Asserts `sql_get_info_w` returns exactly `expected` (a `U32`) for `info_type`.
unsafe fn assert_get_info_u32(conn: *mut c_void, info_type: InfoType, expected: u32) {
    unsafe {
        let mut value: u32 = 0xDEAD_BEEF;
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<TrinoBackend>(
            conn,
            info_type as u16,
            &mut value as *mut u32 as *mut c_void,
            4,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "{info_type:?} must succeed");
        assert_eq!(str_len, 4, "{info_type:?} string_length_ptr");
        assert_eq!(
            value, expected,
            "{info_type:?} must come from get_info_raw, not the generic default"
        );
    }
}

/// Asserts `sql_get_info_w` returns exactly `expected` (a `U16`) for `info_type`.
unsafe fn assert_get_info_u16(conn: *mut c_void, info_type: InfoType, expected: u16) {
    unsafe {
        let mut value: u16 = 0xDEAD;
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<TrinoBackend>(
            conn,
            info_type as u16,
            &mut value as *mut u16 as *mut c_void,
            2,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "{info_type:?} must succeed");
        assert_eq!(str_len, 2, "{info_type:?} string_length_ptr");
        assert_eq!(
            value, expected,
            "{info_type:?} must come from get_info_raw, not the generic default"
        );
    }
}

/// Asserts `sql_get_info_w` returns exactly `expected` (a `String`) for `info_type`.
unsafe fn assert_get_info_str(conn: *mut c_void, info_type: InfoType, expected: &str) {
    unsafe {
        let mut buf = [0u16; 128];
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<TrinoBackend>(
            conn,
            info_type as u16,
            buf.as_mut_ptr() as *mut c_void,
            256,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "{info_type:?} must succeed");
        let result = String::from_utf16_lossy(&buf[..(str_len / 2) as usize]);
        assert_eq!(
            result, expected,
            "{info_type:?} must come from get_info_raw, not the generic default"
        );
    }
}

/// `SQL_KEYWORDS` through the real FFI path, which is the only place the
/// wiring is observable.
///
/// `TrinoBackend::keywords` returns Trino's raw reserved words and
/// `stackable-odbc-core` subtracts `ODBC_RESERVED_KEYWORDS`, sorts and joins
/// them. A unit test in `backend/info.rs` can only redo that subtraction
/// itself, which proves the list is right but not that core asks this
/// backend for it. A core that never calls the hook answers every
/// backend with the empty string, and `SQLGetInfo` still returns `SUCCESS`, so
/// only a call through the real entry point can tell the two apart.
///
/// There is no `odbc_sys::InfoType` variant for 89, so this goes through the
/// raw `u16` rather than `assert_get_info_str`.
#[test]
fn get_info_sql_keywords_reports_trinos_words_minus_odbcs() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        // 22 words, ~230 characters: sized well clear of the answer so a
        // truncation would show up as a short string rather than as SUCCESS.
        let mut buf = [0xEEu16; 512];
        let mut str_len: i16 = -1;
        let ret = ffi::info::sql_get_info_w::<TrinoBackend>(
            conn,
            stackable_odbc_core::types::SQL_KEYWORDS,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 2) as i16,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "SQL_KEYWORDS must succeed");
        assert!(
            str_len >= 0 && str_len % 2 == 0,
            "SQL_KEYWORDS is a character string, so StringLength is an even \
             byte count; got {str_len}"
        );
        let units = str_len as usize / 2;
        let value = String::from_utf16_lossy(&buf[..units]);
        assert_eq!(buf[units], 0, "SQL_KEYWORDS must be null-terminated");

        assert_eq!(
            value,
            "AUTO,CUBE,CURRENT_CATALOG,CURRENT_PATH,CURRENT_ROLE,CURRENT_SCHEMA,\
             GROUPING,JSON_ARRAY,JSON_EXISTS,JSON_OBJECT,JSON_QUERY,JSON_TABLE,\
             JSON_VALUE,LISTAGG,LOCALTIME,LOCALTIMESTAMP,NORMALIZE,RECURSIVE,\
             ROLLUP,SKIP,UESCAPE,UNNEST",
            "SQL_KEYWORDS must be Trino's reserved words minus ODBC's own"
        );
        assert!(
            !value.contains("SELECT"),
            "SELECT is reserved by both, so the spec excludes it from this list"
        );

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// Guards the get_info_raw-first ordering in `info_type_default_response`
/// (stackable-odbc-core/src/ffi/info.rs). Every info type asserted here has no arm in
/// `default_get_info` or `trino_get_info`'s match, so a passing `SUCCESS`
/// alone would prove nothing: reordering the fallback to try the numeric
/// defaults first would still return `SUCCESS`, just with the wrong value
/// (`U32(0)`/`0xFFFFFFFF` instead of the driver's real value). Asserting the
/// exact expected value is what makes this test fail on that regression.
///
/// The version-gated bitmaps (`Sql92Predicates`,
/// `Sql92RelationalJoinOperators`) are asserted at their `server_major == 0`
/// values here, since `disconnected_trino_conn` leaves that field at 0 (a
/// failed version probe); see `sql92_predicates`/`sql92_join_operators`'s
/// own tests in `backend/info.rs` for the version-gated cases.
#[test]
fn get_info_named_but_unhandled_types_fall_back_to_get_info_raw() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        // Trino-specific PowerBI/Power Query capability bitmaps, computed by
        // TrinoBackend::get_info_raw.
        assert_get_info_str(conn, InfoType::OuterJoins, "Y");
        assert_get_info_u32(conn, InfoType::NumericFunctions, TRINO_NUMERIC_FUNCTIONS);
        assert_get_info_u32(conn, InfoType::StringFunctions, TRINO_STRING_FUNCTIONS);
        assert_get_info_u32(conn, InfoType::SystemFunctions, TRINO_SYSTEM_FUNCTIONS);
        assert_get_info_u32(conn, InfoType::TimedateFunctions, TRINO_TIMEDATE_FUNCTIONS);
        assert_get_info_str(conn, InfoType::LikeEscapeClause, "Y");
        // Sql92Predicates and Sql92RelationalJoinOperators are version-gated
        // (computed by sql92_predicates/sql92_join_operators for the injected
        // server version), so they have no single named const to reference.
        assert_get_info_u32(conn, InfoType::Sql92Predicates, 0x3E07);
        assert_get_info_u32(conn, InfoType::Sql92RelationalJoinOperators, 0x17E);
        assert_get_info_u32(
            conn,
            InfoType::Sql92ValueExpressions,
            TRINO_SQL92_VALUE_EXPRESSIONS,
        );
        assert_get_info_u32(
            conn,
            InfoType::AggregateFunctions,
            TRINO_AGGREGATE_FUNCTIONS,
        );

        // Not Trino-specific: these fall through to stackable-odbc-core's
        // common_get_info_raw after TrinoBackend::get_info_raw's own match
        // misses, so Trino depends on that fallback ordering for them too.
        assert_get_info_u16(conn, InfoType::SqlFileUsage, 0);
        // `SQL_IC_LOWER`, because `common_get_info_raw` reads
        // `TrinoBackend::quoted_identifier_case`, and a quoted identifier is
        // case-insensitive in Trino and reported lower case by the system
        // catalog. See that hook for the coordinator probes.
        assert_get_info_u16(conn, InfoType::SqlQuotedIdentifierCase, SQL_IC_LOWER);

        cleanup_injected_conn(env, conn);
    }
}

// ---------------------------------------------------------------------------
// SQLGetInfoW info-type conformance test
// ---------------------------------------------------------------------------
//
// The failures this catches are all "nothing enumerated the spec": a value
// the Windows Driver Manager treats as an integer where the driver returns a
// string (or the reverse), and a conversion bitmap of 0, which makes the
// Windows DM block SQLGetData with HYC00. Line coverage cannot see any of
// them, because the code path producing the wrong answer runs constantly and
// only the info types a test happens to name are asserted on.
//
// These two tests iterate every `InfoType` odbc-sys
// compiles (derived from `info_type_from_raw`, not a hand-copied list; see
// `stackable_odbc_core::conformance`) through the real `sql_get_info_w` FFI entry
// point, against the real `TrinoBackend`. Both use the network-free
// connection injection (`alloc_conn_with_injected_trino_connection`) /
// unconnected allocation (`alloc_handles`) already established above, so
// (like `get_info_named_but_unhandled_types_fall_back_to_get_info_raw`)
// neither needs a live Trino server and neither is `#[ignore]`d.

/// Property 1: every `InfoType`'s returned value has the shape the
/// SQLGetInfo spec declares for it (`stackable_odbc_core::types::expected_kind`),
/// whether `TrinoBackend` answers it itself (`trino_get_info`), falls
/// through to the shared `default_get_info`, or reaches the generic
/// DM-safe default in `info_type_default_response`.
#[test]
fn get_info_every_named_info_type_has_the_declared_shape_connected() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        for info_type in all_info_types() {
            let (ret, kind, _string_length) =
                observe_info_value_kind::<TrinoBackend>(conn, info_type as u16);
            // Not `== SUCCESS`: `observe_info_value_kind` probes the write
            // shape with a non-null buffer it declares to be zero-length, and
            // core reports that as total truncation (SQL_SUCCESS_WITH_INFO plus
            // 01004) for a String-shaped info type. The assertion below is what
            // this message always said it was.
            assert_ne!(
                ret,
                SqlReturn::ERROR,
                "{info_type:?}: SQLGetInfoW must not return SQL_ERROR"
            );
            assert_eq!(
                kind,
                expected_kind(info_type),
                "{info_type:?}: TrinoBackend returned shape {kind:?}, expected \
                 {:?} per the SQLGetInfo spec",
                expected_kind(info_type)
            );
        }

        cleanup_injected_conn(env, conn);
    }
}

/// Property 1, pre-connect path: the Windows Driver Manager queries some
/// info types (e.g. `SQL_DRIVER_ODBC_VER`) before `SQLDriverConnectW`, which
/// routes through `TrinoBackend::get_info_pre_connect` instead of
/// `get_info`. Uses a plain unconnected handle (`alloc_handles`), not the
/// injected-connection helper, since there is no connection at all on this
/// path.
#[test]
fn get_info_every_named_info_type_has_the_declared_shape_pre_connect() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();

        for info_type in all_info_types() {
            let (ret, kind, _string_length) =
                observe_info_value_kind::<TrinoBackend>(conn, info_type as u16);
            // Not `== SUCCESS`: `observe_info_value_kind` probes the write
            // shape with a non-null buffer it declares to be zero-length, and
            // core reports that as total truncation (SQL_SUCCESS_WITH_INFO plus
            // 01004) for a String-shaped info type. The assertion below is what
            // this message always said it was.
            assert_ne!(
                ret,
                SqlReturn::ERROR,
                "{info_type:?}: SQLGetInfoW must not return SQL_ERROR pre-connect"
            );
            assert_eq!(
                kind,
                expected_kind(info_type),
                "{info_type:?}: TrinoBackend returned shape {kind:?} pre-connect, \
                 expected {:?} per the SQLGetInfo spec",
                expected_kind(info_type)
            );
        }

        // stmt was never used to run a query; free directly rather than via
        // the shared `cleanup`, which drains an in-flight result via fetch.
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// Property 1c: the `SQLGetInfo` groups whose members constrain each other
/// agree, for this backend's real answers.
///
/// The shape checks above police one info type at a time; this polices the
/// pairs. Core cannot check these at runtime, because `TrinoBackend::get_info`
/// runs first and is entitled to answer anything, so the invariants live in
/// `conformance` and each driver runs them against its own backend.
///
/// Two of the groups are ones this driver overrides part of and could easily
/// desynchronise: `SQL_CATALOG_TERM` / `SQL_CATALOG_NAME_SEPARATOR` against
/// `SQL_CATALOG_NAME` (which core derives from
/// [`crate::backend::TrinoBackend::supports_catalogs`], answering `true`), and
/// `SQL_TXN_CAPABLE` against `SQL_TXN_ISOLATION_OPTION` and
/// `SQL_DEFAULT_TXN_ISOLATION`. Those three move together: `SQL_TC_DML` with
/// either isolation declaration left at `0` is the inconsistency this catches,
/// and it is the shape a driver lands in by declaring transactions without
/// declaring which isolation levels they run at.
#[test]
fn get_info_groups_that_constrain_each_other_agree() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        let violations =
            stackable_odbc_core::conformance::info_group_inconsistencies::<TrinoBackend>(conn);
        assert!(
            violations.is_empty(),
            "SQLGetInfo answers contradict each other:\n  {}",
            violations.join("\n  ")
        );

        cleanup_injected_conn(env, conn);
    }
}

/// Property 2: no genuine `SQL_CONVERT_*` code ever returns 0 through
/// `TrinoBackend`: per `AGENTS.md`, a `0` conversion bitmap is what makes
/// the Windows Driver Manager block `SQLGetData` with `HYC00`.
#[test]
fn get_info_no_genuine_convert_info_type_ever_returns_zero() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        for info_type in genuine_convert_info_types() {
            let (ret, value) = observe_u32_value::<TrinoBackend>(conn, info_type);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "raw SQL_CONVERT_* info type {info_type} must not error"
            );
            assert_ne!(
                value, 0,
                "raw SQL_CONVERT_* info type {info_type} returned 0: this is the \
                 exact shape that makes the Windows Driver Manager block SQLGetData \
                 with HYC00 (AGENTS.md)"
            );
        }

        cleanup_injected_conn(env, conn);
    }
}

/// Helper: fetch column 1 as a WChar string after sql_fetch succeeds.
///
/// Calls sql_get_data with CDataType::WChar and returns the result as a String.
/// Panics if sql_get_data does not return SUCCESS.
unsafe fn fetch_wchar(stmt: *mut c_void) -> String {
    let mut buf = [0u16; 512];
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::WChar as i16,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as isize,
            &mut ind,
        )
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "sql_get_data failed");
    let code_units = (ind as usize) / 2;
    String::from_utf16_lossy(&buf[..code_units]).to_string()
}

/// Fetch the given 1-based column as a WChar string after `sql_fetch` succeeds.
///
/// One-column generalisation of [`fetch_wchar`].
unsafe fn get_wchar_col(stmt: *mut c_void, col: u16) -> String {
    let mut buf = [0u16; 512];
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            col,
            CDataType::WChar as i16,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as isize,
            &mut ind,
        )
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "sql_get_data failed");
    if ind == SQL_NULL_DATA {
        return String::new();
    }
    let code_units = (ind as usize) / 2;
    String::from_utf16_lossy(&buf[..code_units]).to_string()
}

/// Fetch the given 1-based column as an i64 after `sql_fetch` succeeds.
///
/// NULL (e.g. DECIMAL_DIGITS for a non-decimal column) is reported as 0,
/// matching the query path's `trino_ty_scale` default for non-decimal types.
unsafe fn get_i64_col(stmt: *mut c_void, col: u16) -> i64 {
    let mut val: i64 = 0;
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            col,
            CDataType::SBigInt as i16,
            (&raw mut val).cast(),
            8,
            &mut ind,
        )
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "sql_get_data failed");
    if ind == SQL_NULL_DATA { 0 } else { val }
}

// ---------------------------------------------------------------------------
// Lifecycle test
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn connect_and_disconnect_lifecycle() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        let ret = connect_trino(conn);
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "connect failed: {}",
            conn_diag_message(conn)
        );
        assert_eq!(
            exec_direct(stmt, "SELECT 1"),
            SqlReturn::SUCCESS,
            "exec_direct failed"
        );
        let ret = stackable_odbc_core::ffi::fetch::sql_fetch::<TrinoBackend>(stmt);
        assert_eq!(ret, SqlReturn::SUCCESS, "sql_fetch failed");
        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// Tests moved from backend.rs
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn tables_returns_tpcds_tables() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "tpcds".encode_utf16().collect();
        let schema: Vec<u16> = "sf1".encode_utf16().collect();
        let ret = ffi::metadata::sql_tables_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected at least one table"
        );
        cleanup_stmt(stmt);
    }
}

/// Trino sends VARBINARY as base64 text over the REST API. The driver decodes it
/// to `ColumnValue::Bytes`, so `SQLGetData(SQL_C_BINARY)` must yield the raw
/// payload, not the ASCII bytes of the base64 string ("3q2+7w==").
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn varbinary_get_data_returns_raw_bytes() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(exec_direct(stmt, "SELECT X'DEADBEEF'"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf = [0u8; 16];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::Binary as i16,
            buf.as_mut_ptr().cast(),
            buf.len() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetData(Binary) failed");
        assert_eq!(ind, 4, "expected 4 bytes of VARBINARY payload");
        assert_eq!(
            &buf[..4],
            &[0xDE, 0xAD, 0xBE, 0xEF],
            "VARBINARY did not decode to raw bytes"
        );
        cleanup_stmt(stmt);
    }
}

/// End-to-end proof that `TrinoBackend::escape_dialect()` is wired into the
/// execute path: `SQLExecDirect` is given raw ODBC escape syntax
/// (`{fn UCASE(...)}`, `{d '...'}`) that is not valid Trino SQL on its own,
/// and only succeeds because `sql_exec_direct_w` translates it first (see
/// `crate::escape_dialect`).
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn escape_fn_and_date_literal_translate_for_trino() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(
                stmt,
                "SELECT {fn UCASE('abc')}, CAST({d '2020-01-01'} AS VARCHAR)"
            ),
            SqlReturn::SUCCESS,
            "exec_direct with {{fn}}/{{d}} escapes failed to translate"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            get_wchar_col(stmt, 1),
            "ABC",
            "{{fn UCASE(...)}} not remapped to upper()"
        );
        assert_eq!(
            get_wchar_col(stmt, 2),
            "2020-01-01",
            "{{d '...'}} not rendered as Trino's DATE '...' literal"
        );
        cleanup_stmt(stmt);
    }
}

/// Every `{fn ...}` escape the `SQL_*_FUNCTIONS` bitmaps advertise, executed
/// against a real coordinator.
///
/// This is the check the bitmaps need. The unit tests assert that a rewrite
/// exists and what text it produces, and only the server can say whether that
/// text runs: an advertised escape can still fail there with
/// `FUNCTION_NOT_FOUND` or `COLUMN_NOT_FOUND`.
///
/// Where the value is deterministic it is asserted; where it is not (`now()`,
/// `current_user`) executing without error is the whole point. `DAYOFWEEK` is
/// asserted precisely, because a rename alone would return a *plausible*
/// wrong answer: 2020-02-03 is a Monday, which is 2 in ODBC's numbering and 1
/// in Trino's ISO one.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn every_advertised_scalar_function_escape_runs_on_trino() {
    // (escape, expected value as text, or None for "must merely run")
    let cases: &[(&str, Option<&str>)] = &[
        // Rewritten: argument syntax.
        ("{fn LOCATE('b', 'abc')}", Some("2")),
        // Passed through: ODBC already spells POSITION Trino's way.
        ("{fn POSITION('b' IN 'abc')}", Some("2")),
        // Rewritten: bare keywords, the trailing () removed.
        ("{fn CURDATE()}", None),
        ("{fn CURTIME()}", None),
        ("{fn CURRENT_DATE()}", None),
        ("{fn CURRENT_TIME()}", None),
        ("{fn CURRENT_TIMESTAMP()}", None),
        ("{fn USERNAME()}", None),
        // Rewritten: the interval keyword becomes a quoted unit.
        (
            "{fn TIMESTAMPADD(SQL_TSI_DAY, 1, TIMESTAMP '2020-01-01 00:00:00')}",
            Some("2020-01-02 00:00:00"),
        ),
        (
            "{fn TIMESTAMPDIFF(SQL_TSI_DAY, TIMESTAMP '2020-01-01 00:00:00', \
             TIMESTAMP '2020-01-03 00:00:00')}",
            Some("2"),
        ),
        // Rewritten: ISO numbering converted to ODBC's.
        ("{fn DAYOFWEEK(DATE '2020-02-03')}", Some("2")),
        // Remapped: a plain rename.
        ("{fn UCASE('a')}", Some("A")),
        ("{fn LCASE('A')}", Some("a")),
        ("{fn CHAR(65)}", Some("A")),
        ("{fn IFNULL(NULL, 'x')}", Some("x")),
        ("{fn DAYOFMONTH(DATE '2020-02-03')}", Some("3")),
        ("{fn DAYOFYEAR(DATE '2020-02-03')}", Some("34")),
        ("{fn LOG(1)}", Some("0E0")),
        // Passed through: spelled identically in Trino.
        ("{fn CONCAT('a', 'b')}", Some("ab")),
        ("{fn SUBSTRING('abc', 2, 1)}", Some("b")),
        ("{fn LENGTH('ab')}", Some("2")),
        ("{fn LTRIM(' a')}", Some("a")),
        ("{fn RTRIM('a ')}", Some("a")),
        ("{fn REPLACE('a', 'a', 'b')}", Some("b")),
        ("{fn SOUNDEX('Robert')}", Some("R163")),
        ("{fn NOW()}", None),
        ("{fn MONTH(DATE '2020-02-03')}", Some("2")),
        ("{fn QUARTER(DATE '2020-02-03')}", Some("1")),
        ("{fn WEEK(DATE '2020-02-03')}", Some("6")),
        ("{fn YEAR(DATE '2020-02-03')}", Some("2020")),
        ("{fn HOUR(TIMESTAMP '2020-02-03 04:05:06')}", Some("4")),
        ("{fn MINUTE(TIMESTAMP '2020-02-03 04:05:06')}", Some("5")),
        ("{fn SECOND(TIMESTAMP '2020-02-03 04:05:06')}", Some("6")),
        ("{fn EXTRACT(YEAR FROM DATE '2020-02-03')}", Some("2020")),
        ("{fn ABS(-1)}", Some("1")),
        ("{fn CEILING(1.2)}", Some("2")),
        ("{fn FLOOR(1.8)}", Some("1")),
        ("{fn MOD(5, 2)}", Some("1")),
        ("{fn POWER(2, 3)}", Some("8.0E0")),
        ("{fn ROUND(1.5, 0)}", Some("2.0")),
        ("{fn SIGN(-2)}", Some("-1")),
        ("{fn SQRT(4)}", Some("2.0E0")),
        // A zero digit count scales by nothing, so this is the bare
        // single-argument `truncate` and keeps the literal's decimal type,
        // where POWER and SQRT above are double and render in exponent form.
        ("{fn TRUNCATE(1.9, 0)}", Some("1")),
    ];

    unsafe {
        for (escape, expected) in cases {
            let (_env, _conn, stmt) = alloc_stmt();
            let sql = format!("SELECT CAST(({escape}) AS VARCHAR)");
            assert_eq!(
                exec_direct(stmt, &sql),
                SqlReturn::SUCCESS,
                "{escape} is advertised in a SQL_*_FUNCTIONS bitmap but did not \
                 execute: the translation is missing or produces invalid Trino SQL"
            );
            assert_eq!(
                ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
                SqlReturn::SUCCESS,
                "{escape} executed but returned no row"
            );
            if let Some(want) = expected {
                assert_eq!(
                    get_wchar_col(stmt, 1),
                    *want,
                    "{escape} returned the wrong value"
                );
            }
            cleanup_stmt(stmt);
        }
    }
}

/// The `{fn CONVERT}` counterpart to
/// `every_advertised_scalar_function_escape_runs_on_trino`.
///
/// That test walks the `SQL_*_FUNCTIONS` bitmaps, and `CONVERT` is not in any
/// of them: it is advertised through `SQL_CONVERT_FUNCTIONS` reporting
/// `SQL_FN_CVT_CAST` instead. Which is exactly how the escape stayed advertised
/// and untranslated: `SELECT {fn CONVERT('1', SQL_INTEGER)}` reached Trino as a
/// two-argument function call and failed with `COLUMN_NOT_FOUND` on
/// `sql_integer`, and no test walked the bitmap that promised it.
///
/// Every ODBC type keyword with a mapping is exercised, because a client
/// reading the bitmap may send any of them, and only the server can say whether
/// the `CAST` this produces is one Trino accepts.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn every_convert_escape_target_runs_on_trino() {
    // (value expression, ODBC type keyword, expected text, or None for "must
    // merely run")
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("'1'", "SQL_BIGINT", Some("1")),
        ("'1'", "SQL_INTEGER", Some("1")),
        ("'1'", "SQL_SMALLINT", Some("1")),
        ("'1'", "SQL_TINYINT", Some("1")),
        ("'1'", "SQL_DOUBLE", Some("1.0E0")),
        ("'1'", "SQL_FLOAT", Some("1.0E0")),
        ("'1'", "SQL_REAL", Some("1.0E0")),
        ("'1'", "SQL_DECIMAL", Some("1")),
        ("'1'", "SQL_NUMERIC", Some("1")),
        ("'true'", "SQL_BIT", Some("true")),
        // The truncation guard: a bare CHAR in Trino is CHAR(1), so a wrong
        // mapping here returns "h" rather than failing loudly.
        ("'hello world'", "SQL_CHAR", Some("hello world")),
        ("'hello world'", "SQL_VARCHAR", Some("hello world")),
        ("'hello world'", "SQL_LONGVARCHAR", Some("hello world")),
        ("'hello world'", "SQL_WCHAR", Some("hello world")),
        ("'hello world'", "SQL_WVARCHAR", Some("hello world")),
        ("'hello world'", "SQL_WLONGVARCHAR", Some("hello world")),
        // Trino rejects CAST(varbinary AS VARCHAR), so these are read back
        // through to_hex instead of the shared wrapper below. 0x61 is 'a'.
        ("'a'", "SQL_BINARY", Some("61")),
        ("'a'", "SQL_VARBINARY", Some("61")),
        ("'a'", "SQL_LONGVARBINARY", Some("61")),
        ("'2020-02-03'", "SQL_DATE", Some("2020-02-03")),
        ("'2020-02-03'", "SQL_TYPE_DATE", Some("2020-02-03")),
        ("'04:05:06'", "SQL_TIME", None),
        ("'04:05:06'", "SQL_TYPE_TIME", None),
        ("'2020-02-03 04:05:06'", "SQL_TIMESTAMP", None),
        ("'2020-02-03 04:05:06'", "SQL_TYPE_TIMESTAMP", None),
        (
            "'12151fd2-7586-11e9-8f9e-2a86e4085a59'",
            "SQL_GUID",
            Some("12151fd2-7586-11e9-8f9e-2a86e4085a59"),
        ),
    ];

    unsafe {
        for (value, keyword, expected) in cases {
            let (_env, _conn, stmt) = alloc_stmt();
            let escape = format!("{{fn CONVERT({value}, {keyword})}}");
            // The result has to come back as text to be compared, and Trino
            // has no VARBINARY -> VARCHAR cast, so those read through to_hex.
            let sql = if keyword.contains("BINARY") {
                format!("SELECT to_hex({escape})")
            } else {
                format!("SELECT CAST(({escape}) AS VARCHAR)")
            };
            assert_eq!(
                exec_direct(stmt, &sql),
                SqlReturn::SUCCESS,
                "{escape} is advertised through SQL_CONVERT_FUNCTIONS but did \
                 not execute: the translation is missing or produces invalid \
                 Trino SQL"
            );
            assert_eq!(
                ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
                SqlReturn::SUCCESS,
                "{escape} executed but returned no row"
            );
            if let Some(want) = expected {
                assert_eq!(
                    get_wchar_col(stmt, 1),
                    *want,
                    "{escape} returned the wrong value"
                );
            }
            cleanup_stmt(stmt);
        }
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn date_columns_return_column_date_not_string() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(
                stmt,
                "SELECT d_date FROM tpcds.sf1.date_dim WHERE d_date IS NOT NULL LIMIT 1"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // SQL_DATE_STRUCT layout: year (i16) + month (u16) + day (u16) = 6 bytes
        let mut date_buf = [0u8; 6];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::TypeDate as i16,
            date_buf.as_mut_ptr().cast(),
            date_buf.len() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "TypeDate conversion failed");
        cleanup_stmt(stmt);
    }
}

/// A `time(3)` value's milliseconds must survive `SQLGetData(SQL_C_WCHAR)` as
/// text, even though `SQL_TIME_STRUCT` (the target of `SQL_C_TYPE_TIME`) has
/// no field to hold them. `time(3)` is Trino's normal default precision for
/// `TIME` (unlike the ANSI SQL default of 0), so this is the common case, not
/// an edge case; the fraction must not be dropped before the C type of the
/// target is known. The literal below is used instead of the
/// `postgresql.public.types_test.col_time` column (a real `time(6)` column,
/// per `\d types_test` in the PostgreSQL container) because that table's
/// seed data (`test/postgres-init.sql`) only has whole-second values.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn time_with_fraction_keeps_milliseconds_via_get_data_string() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT TIME '13:30:15.123'"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf = [0u16; 32];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::WChar as i16,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetData(WChar) failed");
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(
            s, "13:30:15.123",
            "time(3) fraction did not survive as text"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn exec_direct_select_and_fetch() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(exec_direct(stmt, "SELECT 1 AS n"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut val: i32 = 0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::SLong as i16,
            (&raw mut val).cast(),
            4,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(val, 1);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA
        );
        cleanup_stmt(stmt);
    }
}

/// Bind a single i64 parameter on `stmt`.
unsafe fn bind_i64(stmt: *mut c_void, param: u16, val: &mut i64) -> SqlReturn {
    unsafe {
        ffi::params::sql_bind_parameter::<TrinoBackend>(
            stmt,
            param,
            ParamType::Input as i16,
            CDataType::SBigInt as i16,
            SqlDataType::EXT_BIG_INT.0,
            19,
            0,
            (val as *mut i64).cast(),
            std::mem::size_of::<i64>() as isize,
            std::ptr::null_mut(),
        )
    }
}

/// Fetch a single i64 from column 1 and assert there are no further rows.
unsafe fn fetch_one_i64(stmt: *mut c_void) -> i64 {
    unsafe {
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected a row"
        );
        let mut val: i64 = 0;
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                (&raw mut val).cast(),
                8,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        val
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn prepared_statement_binds_parameter() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let sql = "SELECT CAST(? AS BIGINT) AS n";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        let mut val: i64 = 4242;
        assert_eq!(bind_i64(stmt, 1, &mut val), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::execute::sql_execute::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(fetch_one_i64(stmt), 4242, "bound parameter was not sent");

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn prepared_statement_re_executes_with_new_parameter() {
    // The point of preparing is running the same statement with different
    // values; the second execute must not fail or reuse the first value.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let sql = "SELECT CAST(? AS BIGINT) AS n";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        for expected in [1i64, 2, 3] {
            let mut val: i64 = expected;
            assert_eq!(bind_i64(stmt, 1, &mut val), SqlReturn::SUCCESS);
            assert_eq!(
                ffi::execute::sql_execute::<TrinoBackend>(stmt),
                SqlReturn::SUCCESS,
                "execute failed for value {expected}"
            );
            assert_eq!(fetch_one_i64(stmt), expected);
            assert_eq!(
                ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt),
                SqlReturn::SUCCESS
            );
        }

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn string_parameter_with_quotes_is_not_injected() {
    // A payload that would break out of the literal must come back verbatim as
    // data. If escaping were wrong this would be a syntax error or return the
    // wrong row.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let sql = "SELECT CAST(? AS VARCHAR) AS s";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        let payload = "'; DROP TABLE users; --";
        let mut buf: Vec<u8> = payload.as_bytes().to_vec();
        // The indicator is passed explicitly rather than left NULL. A NULL
        // `StrLen_or_IndPtr` means "this buffer is null-terminated" per
        // SQLBindParameter, so the driver scans for a NUL, while `to_vec()`
        // produces exactly `payload.len()` bytes with no terminator. The two
        // together read past the allocation into whatever the heap holds
        // next, which appears here as the payload plus trailing garbage, and
        // only when an earlier test has dirtied the allocator: run alone,
        // this test passes either way. See
        // `string_parameter_bound_as_nts_is_not_injected` for the
        // null-terminated form of the same binding.
        let mut ind_in: isize = buf.len() as isize;
        assert_eq!(
            ffi::params::sql_bind_parameter::<TrinoBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Char as i16,
                SqlDataType::VARCHAR.0,
                buf.len(),
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as isize,
                &mut ind_in,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::execute::sql_execute::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut out = [0u8; 64];
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                out.as_mut_ptr().cast(),
                out.len() as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        let got = std::str::from_utf8(&out[..ind as usize]).expect("utf8");
        assert_eq!(got, payload, "payload was altered in transit");

        cleanup_stmt(stmt);
    }
}

/// The same payload bound the other legal way: a null-terminated buffer with a
/// NULL `StrLen_or_IndPtr`.
///
/// `SQLBindParameter` defines a NULL indicator as "the data is
/// null-terminated", so this is the path where the driver scans for the
/// terminator rather than being told the length. It was untested, which is why
/// the sibling test above could pass a buffer carrying no terminator down that
/// path and have the resulting over-read read as an injection failure.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn string_parameter_bound_as_nts_is_not_injected() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let sql = "SELECT CAST(? AS VARCHAR) AS s";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        let payload = "'; DROP TABLE users; --";
        // The trailing NUL is the whole point: it is what makes a NULL
        // indicator a legal binding.
        let mut buf: Vec<u8> = payload.as_bytes().to_vec();
        buf.push(0);
        assert_eq!(
            ffi::params::sql_bind_parameter::<TrinoBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Char as i16,
                SqlDataType::VARCHAR.0,
                payload.len(),
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as isize,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::execute::sql_execute::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut out = [0u8; 64];
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                out.as_mut_ptr().cast(),
                out.len() as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        let got = std::str::from_utf8(&out[..ind as usize]).expect("utf8");
        assert_eq!(got, payload, "payload was altered in transit");

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn columns_returns_tpcds_sf1_columns() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "tpcds".encode_utf16().collect();
        let schema: Vec<u16> = "sf1".encode_utf16().collect();
        let table: Vec<u16> = "customer".encode_utf16().collect();
        let ret = ffi::metadata::sql_columns_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
            std::ptr::null(),
            0,
        );
        if ret != SqlReturn::SUCCESS {
            // Read the diagnostic to understand the failure.
            let mut state = [0u16; 6];
            let mut msg = [0u16; 512];
            let mut msg_len: i16 = 0;
            let mut native: i32 = 0;
            let _ = ffi::diag::sql_get_diag_rec_w::<TrinoBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            );
            let state_str = String::from_utf16_lossy(&state[..5]);
            let msg_str = String::from_utf16_lossy(&msg[..msg_len as usize]);
            panic!("SQLColumnsW returned {ret:?}: SQLSTATE={state_str} msg={msg_str}");
        }
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected at least one column"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn tables_catalog_enumeration_mode() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // catalog="%" + schema="" + table="" → ODBC catalog enumeration mode
        let catalog: Vec<u16> = "%".encode_utf16().collect();
        let schema: Vec<u16> = "".encode_utf16().collect();
        let table: Vec<u16> = "".encode_utf16().collect();
        let ret = ffi::metadata::sql_tables_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected at least one catalog row"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn tables_schema_enumeration_mode() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // catalog="" + schema="%" + table="" → ODBC schema enumeration mode
        let catalog: Vec<u16> = "".encode_utf16().collect();
        let schema: Vec<u16> = "%".encode_utf16().collect();
        let table: Vec<u16> = "".encode_utf16().collect();
        let ret = ffi::metadata::sql_tables_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected at least one schema row"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn tables_table_type_enumeration_mode() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // catalog="" + schema="" + table="" + table_type="%" → table type enumeration mode
        let catalog: Vec<u16> = "".encode_utf16().collect();
        let schema: Vec<u16> = "".encode_utf16().collect();
        let table: Vec<u16> = "".encode_utf16().collect();
        let table_type: Vec<u16> = "%".encode_utf16().collect();
        let ret = ffi::metadata::sql_tables_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
            table_type.as_ptr(),
            table_type.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // TABLE and VIEW must be present
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected TABLE row"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "expected VIEW row"
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// New variant tests: exotic Trino types fetched as WChar strings
// ---------------------------------------------------------------------------

// The variant tests below verify the full type-conversion chain:
//   Trino REST response → ColumnValue variant → C WChar buffer
// At the FFI level, ColumnValue is not directly observable: sql_get_data
// has already marshalled it to a C string. Checking the WChar output
// is the correct way to assert "the correct variant is returned via
// sql_get_data" at this layer of the stack.

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn decimal_literal_returns_wchar_string() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT CAST(123.456 AS DECIMAL(6,3)) AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains("123.456"), "expected '123.456' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn json_literal_returns_wchar_string() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, r#"SELECT JSON '{"key":"value"}' AS v"#),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains("key"), "expected 'key' in {s:?}");
        assert!(s.contains("value"), "expected 'value' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn interval_year_month_returns_wchar() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT INTERVAL '3-7' YEAR TO MONTH AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains('3'), "expected '3' in {s:?}");
        assert!(s.contains('7'), "expected '7' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn interval_day_time_returns_wchar() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT INTERVAL '2 03:04:05.678' DAY TO SECOND AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains('2'), "expected '2' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_with_tz_returns_wchar() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT TIMESTAMP '2024-03-15 10:30:00 +00:00' AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains("2024"), "expected '2024' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_with_named_tz_returns_utc_via_get_data() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // America/New_York in March 2025 is EDT (UTC-4).
        // 20:21:22 EDT → 2025-03-11 00:21:22 UTC.
        assert_eq!(
            exec_direct(
                stmt,
                "SELECT TIMESTAMP '2025-03-10 20:21:22.123 America/New_York' AS v"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut buf = [0u8; std::mem::size_of::<odbc_sys::Timestamp>()];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::TypeTimestamp as i16,
            buf.as_mut_ptr().cast(),
            std::mem::size_of::<odbc_sys::Timestamp>() as isize,
            &mut ind,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "sql_get_data(TypeTimestamp) failed"
        );

        let ts = std::ptr::read(buf.as_ptr().cast::<odbc_sys::Timestamp>());
        assert_eq!(ts.year, 2025, "year");
        assert_eq!(ts.month, 3, "month");
        assert_eq!(ts.day, 11, "day (should roll forward from 10th)");
        assert_eq!(ts.hour, 0, "hour (20 EDT → 0 UTC)");
        assert_eq!(ts.minute, 21, "minute");
        assert_eq!(ts.second, 22, "second");
        assert_eq!(ts.fraction, 123_000_000, "fraction (nanoseconds)");

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_with_utc_tz_returns_utc_via_get_data() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT TIMESTAMP '2020-05-05 22:00:00.000 UTC' AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut buf = [0u8; std::mem::size_of::<odbc_sys::Timestamp>()];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::TypeTimestamp as i16,
            buf.as_mut_ptr().cast(),
            std::mem::size_of::<odbc_sys::Timestamp>() as isize,
            &mut ind,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "sql_get_data(TypeTimestamp) failed"
        );

        let ts = std::ptr::read(buf.as_ptr().cast::<odbc_sys::Timestamp>());
        assert_eq!(ts.year, 2020);
        assert_eq!(ts.month, 5);
        assert_eq!(ts.day, 5);
        assert_eq!(ts.hour, 22);
        assert_eq!(ts.minute, 0);
        assert_eq!(ts.second, 0);
        assert_eq!(ts.fraction, 0);

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn array_literal_returns_wchar() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT ARRAY[1, 2, 3] AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains('1'), "expected '1' in {s:?}");
        assert!(s.contains('2'), "expected '2' in {s:?}");
        assert!(s.contains('3'), "expected '3' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn map_literal_returns_wchar() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT MAP(ARRAY['a'], ARRAY[1]) AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains('a'), "expected 'a' in {s:?}");
        assert!(s.contains('1'), "expected '1' in {s:?}");
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn row_literal_returns_wchar() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT ROW(1, 'hello', true) AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let s = fetch_wchar(stmt);
        assert!(s.contains('1'), "expected '1' in {s:?}");
        assert!(s.contains("hello"), "expected 'hello' in {s:?}");
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P1: SQLGetData truncation
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn get_data_truncates_string_returns_success_with_info() {
    // Verifies that reading a string column into a buffer that is too small
    // returns SUCCESS_WITH_INFO (SQLSTATE 01004) and writes the truncated value.
    // "hello world here" (16 chars); buffer holds 4 u16 slots (8 bytes) →
    // capacity for 3 chars + null → truncated to "hel\0", ind = 32 bytes.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(stmt, "SELECT 'hello world here' AS v"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // 4 u16 slots = 8 bytes → capacity for 3 chars + null terminator.
        let mut wbuf = [0u16; 4];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::WChar as i16,
            wbuf.as_mut_ptr().cast(),
            8, // bytes
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        // ind reports the full byte count of the original string (no null).
        assert_eq!(ind, 32); // 16 chars × 2 bytes
        // Buffer contains "hel\0".
        assert_eq!(String::from_utf16_lossy(&wbuf[..3]), "hel");
        assert_eq!(wbuf[3], 0u16);

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P1: Fetch after NO_DATA returns NO_DATA again (not ERROR)
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn fetch_after_no_data_returns_no_data_again() {
    // After a result set is exhausted (SQLFetch returns NO_DATA), subsequent
    // SQLFetch calls must also return NO_DATA, not ERROR or panic.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "SELECT 1 AS v"), SqlReturn::SUCCESS);

        // Fetch the single row.
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // Cursor exhausted.
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA
        );
        // A second call past the end must still return NO_DATA, not ERROR.
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P1: Statement handle is reusable after an error
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn exec_direct_reuse_after_error() {
    // After a failed exec_direct (invalid SQL → SQL_ERROR), the same statement
    // handle must accept a valid query and succeed.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        // Invalid SQL: must fail.
        assert_eq!(exec_direct(stmt, "NOT VALID SQL AT ALL"), SqlReturn::ERROR);

        // Valid query on the same handle: must succeed.
        assert_eq!(exec_direct(stmt, "SELECT 1 AS v"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut val: i32 = 0;
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::SLong as i16,
                (&raw mut val).cast(),
                4,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, 1);

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P2: SQLColAttributeW: nullable, precision, octet_length via FFI
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn sql_col_attribute_w_returns_nullable() {
    // SQL_DESC_NULLABLE (1008): verify the field is readable and returns a
    // valid ODBC nullable value (0 = not nullable, 1 = nullable, 2 = unknown).
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT c_customer_sk, c_first_name FROM tpcds.sf1.customer LIMIT 1"
            ),
            SqlReturn::SUCCESS
        );

        for col in [1u16, 2u16] {
            let mut num_attr: isize = 99;
            let ret = ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
                stmt,
                col,
                Desc::Nullable as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut num_attr,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "col {col}");
            assert!(
                (0..=2).contains(&num_attr),
                "col {col}: nullable must be 0, 1, or 2, got {num_attr}"
            );
        }

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn sql_col_attribute_w_returns_precision_for_integer() {
    // SQL_DESC_PRECISION (1005): integer columns must return a positive precision.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(stmt, "SELECT c_birth_year FROM tpcds.sf1.customer LIMIT 1"),
            SqlReturn::SUCCESS
        );

        let mut num_attr: isize = -1;
        let ret = ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
            stmt,
            1,
            Desc::Precision as u16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!(
            num_attr >= 0,
            "precision must be non-negative, got {num_attr}"
        );

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn sql_col_attribute_w_returns_octet_length_for_integer() {
    // SQL_DESC_OCTET_LENGTH (1013): integer columns must return a positive length.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(stmt, "SELECT c_birth_year FROM tpcds.sf1.customer LIMIT 1"),
            SqlReturn::SUCCESS
        );

        let mut num_attr: isize = -1;
        let ret = ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
            stmt,
            1,
            Desc::OctetLength as u16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!(
            num_attr > 0,
            "octet_length must be positive, got {num_attr}"
        );

        cleanup_stmt(stmt);
    }
}

/// A `timestamp(6)` column's declared fractional-seconds scale must reach
/// three different, independently-meaningful descriptor fields with three
/// different correct values (a column's declared temporal scale must not be
/// ignored):
///
/// - `SQL_DESC_LENGTH`/`COLUMN_SIZE`: the character length of the string
///   representation, `20 + s` = 26 (ODBC "Column Size" appendix).
/// - `SQL_DESC_PRECISION`: the fractional-seconds scale itself, 6 (per the
///   `SQLColAttribute` spec: "For data types SQL_TYPE_TIME,
///   SQL_TYPE_TIMESTAMP, ... its value is the applicable precision of the
///   fractional seconds component").
///
/// If `has_precision_param()` excluded TIME/TIMESTAMP,
/// `type_name_precision("timestamp(6)")` would fall back to `fixed_precision()`,
/// a constant derived from the undeclared-column default scale (3), and
/// both fields would report the values for scale 3 (23/3) rather than the
/// column's actual declared scale 6 (26/6). Treating the parenthesised
/// argument as `SQL_DESC_LENGTH` directly would instead push `SQL_DESC_LENGTH`
/// to `6` (the scale, not the column size); this test pins both fields
/// independently so neither mistake can pass silently.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_6_reports_correct_length_and_precision_via_sql_col_attribute() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT CAST(TIMESTAMP '2024-03-05 13:30:15.123456' AS TIMESTAMP(6))"
            ),
            SqlReturn::SUCCESS
        );

        let mut length: isize = -1;
        assert_eq!(
            ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
                stmt,
                1,
                Desc::Length as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut length,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(length, 26, "SQL_DESC_LENGTH/COLUMN_SIZE must be 20 + 6");

        let mut precision: isize = -1;
        assert_eq!(
            ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
                stmt,
                1,
                Desc::Precision as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut precision,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            precision, 6,
            "SQL_DESC_PRECISION must be the fractional-seconds scale, not the column size"
        );

        cleanup_stmt(stmt);
    }
}

/// The `TIME` counterpart of the test above: `time(6)` must report
/// `SQL_DESC_LENGTH` = `9 + 6` = 15 and `SQL_DESC_PRECISION` = 6.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn time_6_reports_correct_length_and_precision_via_sql_col_attribute() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(stmt, "SELECT CAST(TIME '13:30:15.123456' AS TIME(6))"),
            SqlReturn::SUCCESS
        );

        let mut length: isize = -1;
        assert_eq!(
            ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
                stmt,
                1,
                Desc::Length as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut length,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(length, 15, "SQL_DESC_LENGTH/COLUMN_SIZE must be 9 + 6");

        let mut precision: isize = -1;
        assert_eq!(
            ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
                stmt,
                1,
                Desc::Precision as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut precision,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            precision, 6,
            "SQL_DESC_PRECISION must be the fractional-seconds scale, not the column size"
        );

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P2: SQLCloseCursor called twice returns 24000 on the second call
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn close_cursor_twice_returns_error() {
    // The second SQLCloseCursor call must return ERROR (SQLSTATE 24000, invalid
    // cursor state) because there is no open cursor after the first close.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "SELECT 1 AS v"), SqlReturn::SUCCESS);

        // First close: cursor is open, must succeed.
        assert_eq!(
            ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // Second close: no cursor open, must return ERROR (24000).
        assert_eq!(
            ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt),
            SqlReturn::ERROR
        );

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P2: SQLNumResultCols after SQLPrepare but before SQLExecute
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn num_result_cols_after_prepare_before_execute() {
    // After SQLPrepare (but before SQLExecute), SQLNumResultCols must return
    // SUCCESS. The Trino backend returns count=0 because column metadata is
    // only populated after execute.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        let sql = "SELECT c_customer_sk FROM tpcds.sf1.customer LIMIT 1";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        let ret =
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32);
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut count: i16 = 99;
        let ret = ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        // Column metadata is populated only after execute.
        assert_eq!(count, 0);

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P3: SQLGetDiagFieldW: field-by-field after an error
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn get_diag_field_number_after_error() {
    // SQL_DIAG_NUMBER (2) on the header record (rec_number=0) reports the count
    // of diagnostic records. After one error it must be 1.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        let mut count: i32 = 0;
        let ret = ffi::diag::sql_get_diag_field_w::<TrinoBackend>(
            HandleType::Stmt as i16,
            stmt,
            0, // header field: rec_number = 0
            HeaderDiagnosticIdentifier::Number as i16,
            &mut count as *mut i32 as *mut c_void,
            0,
            std::ptr::null_mut(),
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(count, 1, "one diagnostic record after one error");

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn get_diag_field_sqlstate_after_error() {
    // SQL_DIAG_SQLSTATE (4) on rec_number=1 returns the 5-character SQLSTATE.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        // 6 u16 slots: 5 SQLSTATE chars + null terminator = 12 bytes.
        let mut state_buf = [0u16; 6];
        let mut str_len: i16 = 0;
        let ret = ffi::diag::sql_get_diag_field_w::<TrinoBackend>(
            HandleType::Stmt as i16,
            stmt,
            1, // first record
            HeaderDiagnosticIdentifier::SqlState as i16,
            state_buf.as_mut_ptr() as *mut c_void,
            12, // buffer_length in bytes (6 u16s)
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // SQLSTATE is always exactly 5 characters = 10 bytes; StringLengthPtr
        // is spec'd in bytes for SQLGetDiagField.
        assert_eq!(str_len, 10);
        let state = String::from_utf16_lossy(&state_buf[..5]);
        assert_eq!(state.len(), 5, "SQLSTATE must be 5 chars");

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn get_diag_field_native_error_after_error() {
    // SQL_DIAG_NATIVE (5) returns the driver-specific native error code (i32).
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        let mut native: i32 = -999;
        let mut str_len: i16 = 0;
        let ret = ffi::diag::sql_get_diag_field_w::<TrinoBackend>(
            HandleType::Stmt as i16,
            stmt,
            1, // first record
            HeaderDiagnosticIdentifier::Native as i16,
            &mut native as *mut i32 as *mut c_void,
            0,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(str_len, 4); // i32 = 4 bytes
        let _ = native;

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn get_diag_field_message_text_after_error() {
    // SQL_DIAG_MESSAGE_TEXT (6) returns the diagnostic message string.
    // After an invalid-SQL error the message must be non-empty.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        let mut msg_buf = [0u16; 256];
        let mut str_len: i16 = 0;
        let buffer_length =
            i16::try_from(std::mem::size_of_val(&msg_buf)).expect("msg_buf byte size fits in i16");
        let ret = ffi::diag::sql_get_diag_field_w::<TrinoBackend>(
            HandleType::Stmt as i16,
            stmt,
            1, // first record
            HeaderDiagnosticIdentifier::MessageText as i16,
            msg_buf.as_mut_ptr() as *mut c_void,
            buffer_length,
            &mut str_len,
        );
        // SUCCESS_WITH_INFO is also valid when the message is longer than the buffer.
        assert!(
            matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO),
            "expected SUCCESS or SUCCESS_WITH_INFO, got {ret:?}"
        );
        assert!(str_len > 0, "diagnostic message must be non-empty");
        // str_len is a BYTE count (SQLGetDiagField spec); convert to UTF-16
        // code units and clamp to the buffer's element count before indexing:
        // the untruncated byte count can exceed the buffer capacity.
        let code_units =
            (usize::try_from(str_len).expect("non-negative length") / 2).min(msg_buf.len());
        let msg = String::from_utf16_lossy(&msg_buf[..code_units]);
        assert!(!msg.is_empty());

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// Empty result set: WHERE 1=0 must return SUCCESS with zero rows
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn empty_result_set_where_false() {
    // A query that returns no rows (WHERE 1=0) must:
    //   - exec_direct → SUCCESS (not ERROR)
    //   - sql_num_result_cols → SUCCESS with count > 0 (columns are known)
    //   - first sql_fetch → NO_DATA (not ERROR)
    // This is the response-to-DM path: the DM reads column count before
    // fetching rows, so metadata must survive even when the row list is empty.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT c_customer_sk FROM tpcds.sf1.customer WHERE 1 = 0"
            ),
            SqlReturn::SUCCESS,
            "exec_direct must succeed for an empty result set"
        );

        let mut col_count: i16 = -1;
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut col_count),
            SqlReturn::SUCCESS,
            "SQLNumResultCols must succeed"
        );
        assert_eq!(col_count, 1, "one column even for empty result set");

        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "first fetch on empty result set must return NO_DATA"
        );

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// P3: SQLGetEnvAttrW: ODBC version roundtrip (no Trino connection required)
// ---------------------------------------------------------------------------

#[test]
fn get_env_attr_odbc_version_roundtrip() {
    // Set SQL_ATTR_ODBC_VERSION (200) to SQL_OV_ODBC3 (3), then read it back.
    // Per spec HY010, SQLSetEnvAttr must be called before any connection handle
    // is allocated on the environment. We use a bare env handle here.
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            ),
            SqlReturn::SUCCESS
        );

        // Set SQL_ATTR_ODBC_VERSION = SQL_OV_ODBC3 (3).
        assert_eq!(
            ffi::env::sql_set_env_attr::<TrinoBackend>(
                env,
                EnvironmentAttribute::OdbcVersion as i32,
                AttrOdbcVersion::Odbc3 as usize as *mut c_void,
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Read it back.
        let mut version: i32 = 0;
        let mut str_len: i32 = 0;
        assert_eq!(
            ffi::env::sql_get_env_attr::<TrinoBackend>(
                env,
                EnvironmentAttribute::OdbcVersion as i32,
                &mut version as *mut i32 as *mut c_void,
                4,
                &mut str_len,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(version, AttrOdbcVersion::Odbc3 as i32);
        assert_eq!(str_len, 4); // sizeof(i32)

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

// ---------------------------------------------------------------------------
// Array-fetch path (SQLBindCol + SQLFetch)
// ---------------------------------------------------------------------------
//
// pyodbc retrieves column data via SQLGetData after each fetch; turbodbc and
// other drivers that pre-allocate column buffers use SQLBindCol + SQLFetch
// instead. This test exercises the bound-column path so regressions in
// sql_bind_col or the write_column_value call inside sql_fetch are caught
// independently of the sql_get_data path.

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn bind_col_and_fetch_reads_bound_column_values() {
    // Exercises SQL_ATTR_ROW_ARRAY_SIZE (27) and SQL_ATTR_ROWS_FETCHED_PTR (26)
    // attribute setting (accepted without error) plus the full SQLBindCol →
    // SQLFetch data path.
    //
    // NOTE: batch INSERT (SQL_ATTR_PARAMSET_SIZE) is not tested here because
    // the tpcds catalog is read-only; the writable postgresql catalog covers
    // the full bind-parameter path elsewhere in this suite.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        // Set SQL_ATTR_ROW_ARRAY_SIZE = 1.
        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<TrinoBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::without_provenance_mut(1usize), // 1 row per fetch
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Set SQL_ATTR_ROWS_FETCHED_PTR to a usize variable.
        let mut rows_fetched: usize = 0;
        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<TrinoBackend>(
                stmt,
                StatementAttribute::RowsFetchedPtr as i32,
                &mut rows_fetched as *mut usize as *mut c_void,
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Execute a SELECT that returns exactly 3 integer rows.
        assert_eq!(
            exec_direct(
                stmt,
                "SELECT c FROM (VALUES (10), (20), (30)) AS t(c) ORDER BY c"
            ),
            SqlReturn::SUCCESS
        );

        // Bind column 1 to an i32 buffer via SQLBindCol.
        // Trino returns VALUES integer literals as INTEGER (32-bit).
        let mut val_buf: i32 = 0;
        let mut val_ind: isize = 0;
        assert_eq!(
            ffi::bind::sql_bind_col::<TrinoBackend>(
                stmt,
                1, // column 1
                CDataType::SLong as i16,
                &mut val_buf as *mut i32 as *mut c_void,
                std::mem::size_of::<i32>() as isize,
                &mut val_ind,
            ),
            SqlReturn::SUCCESS
        );

        // Fetch each row and verify the bound buffer is populated.
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(val_buf, 10);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(val_buf, 20);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(val_buf, 30);

        // Result set exhausted.
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup_stmt(stmt);
    }
}

/// `SQL_ROW_SUCCESS`, the row-status value the spec fixes at 0. Core keeps its
/// own copy private to `ffi/fetch.rs`, and a test asserting a spec value names it
/// rather than writing the literal.
const SQL_ROW_SUCCESS: u16 = 0;

/// `SQLGetFunctions` advertises `SQL_API_SQLEXTENDEDFETCH`, so this is the
/// evidence for that claim: the function has to fetch rows rather than fail.
///
/// It reports through its own `RowCountPtr` and `RowStatusArray` arguments, which
/// the spec keeps separate from `SQL_ATTR_ROWS_FETCHED_PTR` and
/// `SQL_ATTR_ROW_STATUS_PTR`: that buffer "is used only by SQLExtendedFetch".
/// Asserting both arguments is what distinguishes a working implementation from
/// one that fetched a row and told the application nothing about it.
///
/// The forward-only rejection is asserted alongside, because an advertised
/// function that accepts an orientation it cannot honour is worse than one that
/// refuses: `HY106` is the clause of that row carrying no `(DM)` marker, so it is
/// this driver's to report.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn extended_fetch_reads_rows_and_reports_through_its_own_arguments() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(stmt, "SELECT c FROM (VALUES (10), (20)) AS t(c) ORDER BY c"),
            SqlReturn::SUCCESS
        );

        let mut val_buf: i32 = 0;
        let mut val_ind: isize = 0;
        assert_eq!(
            ffi::bind::sql_bind_col::<TrinoBackend>(
                stmt,
                1,
                CDataType::SLong as i16,
                &mut val_buf as *mut i32 as *mut c_void,
                std::mem::size_of::<i32>() as isize,
                &mut val_ind,
            ),
            SqlReturn::SUCCESS
        );

        let mut row_count: usize = 0;
        let mut row_status: u16 = 0xFFFF;
        assert_eq!(
            ffi::fetch::sql_extended_fetch::<TrinoBackend>(
                stmt,
                odbc_sys::FetchOrientation::Next as u16,
                0,
                &mut row_count,
                &mut row_status,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val_buf, 10, "the bound column must carry the first row");
        assert_eq!(row_count, 1, "RowCountPtr must report the rowset size");
        assert_eq!(
            row_status, SQL_ROW_SUCCESS,
            "RowStatusArray element 0 must report the row's status"
        );

        assert_eq!(
            ffi::fetch::sql_extended_fetch::<TrinoBackend>(
                stmt,
                odbc_sys::FetchOrientation::Next as u16,
                0,
                &mut row_count,
                &mut row_status,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val_buf, 20);

        // Exhausted. There is no row, so there is no status to report, but the
        // count still has to say zero rather than keep the previous rowset's.
        assert_eq!(
            ffi::fetch::sql_extended_fetch::<TrinoBackend>(
                stmt,
                odbc_sys::FetchOrientation::Next as u16,
                0,
                &mut row_count,
                &mut row_status,
            ),
            SqlReturn::NO_DATA
        );
        assert_eq!(row_count, 0, "an exhausted rowset holds no rows");

        cleanup_stmt(stmt);
    }
}

/// Null out-params are legal: both arguments are optional, and an application
/// that wants neither must not be made to supply them.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn extended_fetch_accepts_null_row_count_and_status_arguments() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(exec_direct(stmt, "SELECT 1"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_extended_fetch::<TrinoBackend>(
                stmt,
                odbc_sys::FetchOrientation::Next as u16,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );

        cleanup_stmt(stmt);
    }
}

/// Every orientation but `SQL_FETCH_NEXT` is `HY106` on this driver's
/// forward-only cursor, including `SQL_FETCH_BOOKMARK`, which `odbc-sys` has no
/// variant for and which an application can nevertheless pass.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn extended_fetch_refuses_every_orientation_but_next() {
    unsafe {
        for orientation in [
            odbc_sys::FetchOrientation::First as u16,
            odbc_sys::FetchOrientation::Last as u16,
            odbc_sys::FetchOrientation::Prior as u16,
            odbc_sys::FetchOrientation::Absolute as u16,
            odbc_sys::FetchOrientation::Relative as u16,
            SQL_FETCH_BOOKMARK as u16,
        ] {
            let (_env, _conn, stmt) = alloc_stmt();
            assert_eq!(exec_direct(stmt, "SELECT 1"), SqlReturn::SUCCESS);

            let mut row_count: usize = 99;
            assert_eq!(
                ffi::fetch::sql_extended_fetch::<TrinoBackend>(
                    stmt,
                    orientation,
                    0,
                    &mut row_count,
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
                "orientation {orientation} must be refused on a forward-only cursor"
            );
            assert_eq!(
                last_sqlstate(stmt),
                "HY106",
                "orientation {orientation} must report HY106"
            );

            cleanup_stmt(stmt);
        }
    }
}

// ---------------------------------------------------------------------------
// Batch parameter path (SQLBindParameter + SQLPrepare + SQLExecute)
// ---------------------------------------------------------------------------
//
// The tpcds catalog is read-only, so DML runs against the writable PostgreSQL
// catalog.

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443 with the postgresql catalog; run ./integration-tests/setup.sh first"]
fn paramset_size_and_bound_insert_into_postgresql() {
    // Exercises SQL_ATTR_PARAMSET_SIZE (22) attribute setting plus the full
    // SQLBindParameter -> SQLPrepare -> SQLExecute DML path.
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let table = "postgresql.public.h9_paramset_test";

        // Each statement is closed before the next runs on the shared handle;
        // otherwise SQLExecDirect returns 24000 (a cursor is already open).

        // Start clean in case a previous run left the table behind.
        let _ = exec_direct(stmt, &format!("DROP TABLE IF EXISTS {table}"));
        let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);

        assert_eq!(
            exec_direct(stmt, &format!("CREATE TABLE {table} (id bigint)")),
            SqlReturn::SUCCESS
        );
        let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);

        // SQL_ATTR_PARAMSET_SIZE = 1: one parameter row per execute.
        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<TrinoBackend>(
                stmt,
                StatementAttribute::ParamsetSize as i32,
                std::ptr::without_provenance_mut(1usize),
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Prepare and run a bound INSERT.
        let sql = format!("INSERT INTO {table} VALUES (?)");
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );
        let mut val: i64 = 42;
        assert_eq!(bind_i64(stmt, 1, &mut val), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::execute::sql_execute::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);

        // The row landed.
        assert_eq!(
            exec_direct(stmt, &format!("SELECT COUNT(*) FROM {table} WHERE id = 42")),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            fetch_one_i64(stmt),
            1,
            "bound INSERT did not persist the row"
        );
        let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);

        // Clean up the table.
        assert_eq!(
            exec_direct(stmt, &format!("DROP TABLE {table}")),
            SqlReturn::SUCCESS
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLPrimaryKeysW tests
// ---------------------------------------------------------------------------
// NOTE: Trino's information_schema does not include table_constraints or
// key_column_usage in any connector (PostgreSQL, tpcds, memory, etc.).
// These tests verify that the driver returns SQL_SUCCESS with an empty
// result set rather than SQL_ERROR.
// When Trino adds constraint metadata support, these tests should be
// updated to verify actual PK data.

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443 with the postgresql catalog; run ./integration-tests/setup.sh first"]
fn primary_keys_postgresql_returns_success() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "postgresql".encode_utf16().collect();
        let schema: Vec<u16> = "public".encode_utf16().collect();
        let table: Vec<u16> = "customers".encode_utf16().collect();
        let ret = ffi::metadata::sql_primary_keys_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "sql_primary_keys_w should succeed");
        // Trino doesn't expose table_constraints: empty result expected
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn primary_keys_no_constraints_returns_empty() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "tpcds".encode_utf16().collect();
        let schema: Vec<u16> = "sf1".encode_utf16().collect();
        let table: Vec<u16> = "customer".encode_utf16().collect();
        let ret = ffi::metadata::sql_primary_keys_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "should succeed even with no PKs");
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "tpcds has no PK constraints, so expect an empty result"
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLForeignKeysW tests
// ---------------------------------------------------------------------------
// NOTE: Same limitation as primary keys; Trino doesn't expose
// referential_constraints. Tests verify SQL_SUCCESS with empty results.

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443 with the postgresql catalog; run ./integration-tests/setup.sh first"]
fn foreign_keys_postgresql_returns_success() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "postgresql".encode_utf16().collect();
        let schema: Vec<u16> = "public".encode_utf16().collect();
        let fk_table: Vec<u16> = "orders".encode_utf16().collect();
        let ret = ffi::metadata::sql_foreign_keys_w::<TrinoBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            fk_table.as_ptr(),
            fk_table.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "sql_foreign_keys_w should succeed");
        // Trino doesn't expose referential_constraints: empty result expected
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn foreign_keys_no_constraints_returns_empty() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "tpcds".encode_utf16().collect();
        let schema: Vec<u16> = "sf1".encode_utf16().collect();
        let table: Vec<u16> = "customer".encode_utf16().collect();
        let ret = ffi::metadata::sql_foreign_keys_w::<TrinoBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "should succeed even with no FKs");
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLStatisticsW test
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn statistics_returns_empty_result_set() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "postgresql".encode_utf16().collect();
        let schema: Vec<u16> = "public".encode_utf16().collect();
        let table: Vec<u16> = "customers".encode_utf16().collect();
        let ret = ffi::metadata::sql_statistics_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
            SQL_INDEX_UNIQUE,
            SQL_QUICK,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "sql_statistics_w should succeed");

        // Verify it has columns (13 per ODBC spec) by checking num_result_cols
        let mut col_count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut col_count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            col_count, 13,
            "statistics result set should have 13 columns"
        );

        // Verify empty
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "statistics should return empty result set"
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// Query attribution
//
// `source` and `client_tags` are the two things a Trino operator uses to tell
// one client's traffic from another's and to route it to a resource group.
// Neither is observable through ODBC, so the assertion has to come from the
// server: `system.runtime.queries` records the source of every query.
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn queries_reach_trino_tagged_with_the_drivers_source() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // The shared connection names no Source, so this is the default.
        let sql = "SELECT source FROM system.runtime.queries \
                   WHERE query_id = (SELECT max(query_id) FROM system.runtime.queries \
                                     WHERE query LIKE 'SELECT 41 + 1%')";
        let seed: Vec<u16> = "SELECT 41 + 1".encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_exec_direct_w::<TrinoBackend>(stmt, seed.as_ptr(), seed.len() as i32),
            SqlReturn::SUCCESS
        );
        while ffi::fetch::sql_fetch::<TrinoBackend>(stmt) == SqlReturn::SUCCESS {}
        assert_eq!(
            ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_exec_direct_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS,
            "{}",
            diag_message(stmt)
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "no query found in system.runtime.queries"
        );

        let mut buf = [0u16; 128];
        let mut len: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::WChar as i16,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as isize,
                &mut len,
            ),
            SqlReturn::SUCCESS
        );
        let source = String::from_utf16_lossy(&buf[..(len as usize) / 2]);
        // `env!` rather than a literal, so the assertion tracks the version
        // the driver reports rather than needing a bump of its own.
        assert_eq!(
            source,
            format!("stackable-odbc-trino/{}", env!("CARGO_PKG_VERSION")),
            "Trino recorded the query under a source that does not name this driver and build"
        );

        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLDescribeParam tests
//
// Core's fallback describes every parameter as VARCHAR(SQL_DEFAULT_PARAM_SIZE),
// which is what makes a client send a number as text. Trino can be asked:
// `DESCRIBE INPUT` on a prepared statement returns a type per parameter.
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn describe_param_reports_the_type_trino_infers_for_each_parameter() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // Two parameters of different types, so a generic answer cannot pass
        // by coincidence: the WHERE comparison makes the first a bigint and
        // the second a char(20), which is c_first_name's declared type.
        let sql = "SELECT c_customer_sk FROM tpcds.sf1.customer \
                   WHERE c_customer_sk = ? AND c_first_name = ?";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        let describe = |n: u16| -> (i16, usize, i16, i16) {
            let (mut ty, mut size, mut digits, mut nullable) = (0i16, 0usize, 0i16, 0i16);
            let ret = ffi::params::sql_describe_param::<TrinoBackend>(
                stmt,
                n,
                &mut ty,
                &mut size,
                &mut digits,
                &mut nullable,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "sql_describe_param({n}) failed: {}",
                diag_message(stmt)
            );
            (ty, size, digits, nullable)
        };

        let (ty1, _, _, _) = describe(1);
        assert_eq!(
            ty1,
            SqlDataType::EXT_BIG_INT.0,
            "parameter 1 compares against a bigint column"
        );

        let (ty2, size2, _, _) = describe(2);
        assert_eq!(
            ty2,
            SqlDataType::EXT_W_CHAR.0,
            "parameter 2 compares against a char(20) column"
        );
        assert_eq!(
            size2, 20,
            "char(20) must carry its length for buffer sizing"
        );

        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn describe_param_re_describes_when_the_statement_changes() {
    // The descriptors are cached on the connection, keyed by SQL text, because
    // `Backend::describe_param` is called once per parameter and gets no
    // statement handle. If that key were ignored, a second statement would be
    // answered with the first one's types: a wrong specific type, which is
    // the one outcome worse than no answer at all.
    unsafe {
        let describe_first_param_of = |sql: &str| -> i16 {
            let (_env, _conn, stmt) = alloc_stmt();
            let wide: Vec<u16> = sql.encode_utf16().collect();
            assert_eq!(
                ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );
            let (mut ty, mut size, mut digits, mut nullable) = (0i16, 0usize, 0i16, 0i16);
            assert_eq!(
                ffi::params::sql_describe_param::<TrinoBackend>(
                    stmt,
                    1,
                    &mut ty,
                    &mut size,
                    &mut digits,
                    &mut nullable,
                ),
                SqlReturn::SUCCESS,
                "{}",
                diag_message(stmt)
            );
            cleanup_stmt(stmt);
            ty
        };

        let bigint_param =
            describe_first_param_of("SELECT 1 FROM tpcds.sf1.customer WHERE c_customer_sk = ?");
        assert_eq!(bigint_param, SqlDataType::EXT_BIG_INT.0);

        let char_param =
            describe_first_param_of("SELECT 1 FROM tpcds.sf1.customer WHERE c_first_name = ?");
        assert_eq!(
            char_param,
            SqlDataType::EXT_W_CHAR.0,
            "the second statement was answered from the first one's cache entry"
        );
    }
}

// ---------------------------------------------------------------------------
// SQLTablePrivilegesW / SQLColumnPrivilegesW / SQLProceduresW /
// SQLProcedureColumnsW tests
//
// All four answer an empty result set against this test stack, but for two
// different reasons, and the distinction is what these tests protect.
//
// `SQLTablePrivileges` runs a real query: Trino models table privileges in
// `information_schema.table_privileges`, and it is empty here only because
// neither test catalog implements permission management (`GRANT` on either
// answers NOT_SUPPORTED, and a grant made directly in PostgreSQL is not
// visible through the `postgresql` catalog: Trino synthesises its own
// `information_schema`). So the assertion that matters is that the query is
// accepted and its column list is the one the driver expects; a rename or
// reordering in `information_schema.table_privileges` fails it. The row
// conversion has unit coverage in `backend::metadata`.
//
// The other three read nothing. Trino publishes no column-privilege or
// procedure metadata at all, so their empty result set is a fact about Trino,
// not about this stack's configuration.
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn table_privileges_queries_trino_and_returns_the_spec_column_count() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "tpcds".encode_utf16().collect();
        let schema: Vec<u16> = "sf1".encode_utf16().collect();
        let table: Vec<u16> = "call_center".encode_utf16().collect();
        let ret = ffi::metadata::sql_table_privileges_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
        );
        // A failure here is the interesting outcome: it means the query
        // against information_schema.table_privileges was rejected.
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "sql_table_privileges_w should succeed: {}",
            diag_message(stmt)
        );

        let mut col_count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut col_count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            col_count, 7,
            "table privileges result set should have 7 columns"
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "neither test catalog implements permission management, so no rows"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn column_privileges_returns_empty_result_set() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "tpcds".encode_utf16().collect();
        let schema: Vec<u16> = "sf1".encode_utf16().collect();
        let table: Vec<u16> = "call_center".encode_utf16().collect();
        let column: Vec<u16> = "%".encode_utf16().collect();
        let ret = ffi::metadata::sql_column_privileges_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            table.as_ptr(),
            table.len() as i16,
            column.as_ptr(),
            column.len() as i16,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "sql_column_privileges_w should succeed: {}",
            diag_message(stmt)
        );

        let mut col_count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut col_count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            col_count, 8,
            "column privileges result set should have 8 columns"
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "Trino grants on tables, not columns, so there is nothing to report"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn procedures_returns_empty_result_set() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "system".encode_utf16().collect();
        let schema: Vec<u16> = "runtime".encode_utf16().collect();
        let proc_name: Vec<u16> = "%".encode_utf16().collect();
        let ret = ffi::metadata::sql_procedures_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            proc_name.as_ptr(),
            proc_name.len() as i16,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "sql_procedures_w should succeed: {}",
            diag_message(stmt)
        );

        let mut col_count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut col_count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(col_count, 8, "procedures result set should have 8 columns");

        // `system.runtime` really does hold callable procedures
        // (`kill_query`), so this asserts the documented gap: Trino publishes
        // no metadata naming them.
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "Trino publishes no procedure metadata, even where procedures exist"
        );
        cleanup_stmt(stmt);
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn procedure_columns_returns_empty_result_set() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        let catalog: Vec<u16> = "system".encode_utf16().collect();
        let schema: Vec<u16> = "runtime".encode_utf16().collect();
        let proc_name: Vec<u16> = "%".encode_utf16().collect();
        let column: Vec<u16> = "%".encode_utf16().collect();
        let ret = ffi::metadata::sql_procedure_columns_w::<TrinoBackend>(
            stmt,
            catalog.as_ptr(),
            catalog.len() as i16,
            schema.as_ptr(),
            schema.len() as i16,
            proc_name.as_ptr(),
            proc_name.len() as i16,
            column.as_ptr(),
            column.len() as i16,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "sql_procedure_columns_w should succeed: {}",
            diag_message(stmt)
        );

        let mut col_count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut col_count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            col_count, 19,
            "procedure columns result set should have 19 columns"
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::NO_DATA,
            "Trino publishes no procedure metadata"
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLBulkOperations / SQLSetPos: HYC00 tests
// ---------------------------------------------------------------------------

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn bulk_operations_returns_hyc00() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(exec_direct(stmt, "SELECT 1 AS n"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let ret = ffi::cursor::sql_bulk_operations::<TrinoBackend>(
            stmt,
            odbc_sys::BulkOperation::Add as i16,
        );
        assert_eq!(
            ret,
            SqlReturn::ERROR,
            "SQLBulkOperations should return ERROR"
        );
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLDescribeColW / SQLColumnsW type-metadata agreement
// ---------------------------------------------------------------------------

/// Describe every column of the current result set via `SQLDescribeColW`.
unsafe fn collect_describe_col(stmt: *mut c_void) -> Vec<(String, i16, usize, i16)> {
    let mut count: i16 = 0;
    unsafe {
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<TrinoBackend>(stmt, &mut count),
            SqlReturn::SUCCESS
        );
    }
    (1..=count)
        .map(|col| {
            let mut name = [0u16; 256];
            let mut name_len: i16 = 0;
            let mut data_type: i16 = 0;
            let mut col_size: usize = 0;
            let mut decimal_digits: i16 = 0;
            let mut nullable: i16 = 0;
            unsafe {
                assert_eq!(
                    ffi::metadata::sql_describe_col_w::<TrinoBackend>(
                        stmt,
                        u16::try_from(col).expect("column index fits u16"),
                        name.as_mut_ptr(),
                        i16::try_from(name.len()).expect("buffer fits i16"),
                        &mut name_len,
                        &mut data_type,
                        &mut col_size,
                        &mut decimal_digits,
                        &mut nullable,
                    ),
                    SqlReturn::SUCCESS
                );
            }
            let n = String::from_utf16_lossy(&name[..usize::try_from(name_len).unwrap_or(0)]);
            (n, data_type, col_size, decimal_digits)
        })
        .collect()
}

/// The query path (`SQLDescribeColW`) and the catalog path (`SQLColumnsW`)
/// derive type, size and scale from the same native Trino type text, so the
/// columns of `postgresql.public.types_test` must agree between them. The
/// ways they can disagree are `char` (WVARCHAR against WCHAR), `timestamp`
/// (23 against 29) and `varchar(n)` (0 against n).
///
/// Covers `VARCHAR`, `CHAR`, `DECIMAL`, integer and floating-point columns
/// plus `DATE`, `TIME`, `TIMESTAMP` and `BOOLEAN`. Those last four depend on
/// `SQLColumns`' `COLUMN_SIZE` gate (`backend/metadata.rs`) reporting
/// `type_name_precision`'s value for every type it can resolve one for. A
/// separately maintained "is_char || is_numeric" list leaves the four out,
/// and the catalog path then reports their `COLUMN_SIZE` as `NULL` whatever
/// the query path says.
///
/// The catalog path needs the session's default catalog and schema switched
/// to `postgresql`/`public` first. `SQLColumnsW` reads
/// `information_schema.columns` unqualified, which Trino resolves against the
/// *session's* default catalog, and the shared connection defaults to
/// `Catalog=tpcds`: a `table_catalog='postgresql'` filter on that session
/// returns zero rows though the table exists. That is Trino's
/// information_schema scoping, not a defect.
///
/// `USE` switches the shared connection and switches it back, rather than
/// opening a second connection. A second one means a second `reqwest` pool
/// against the same coordinator, which is the intermittent TCP socket
/// corruption this file's own docs warn about for `backend::tests`, and it
/// reproduces here. The query path works from any session, given a fully
/// qualified `SELECT`, so it runs before the switch.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn describe_col_and_columns_agree_on_type_metadata() {
    const CATALOG: &str = "postgresql";
    const SCHEMA: &str = "public";
    const TABLE: &str = "types_test";
    const COLUMNS: &str = "id, col_smallint, col_integer, col_bigint, col_real, \
         col_double, col_decimal, col_varchar, col_char, \
         col_boolean, col_date, col_time, col_timestamp";

    unsafe {
        // Query path.
        let (_, _, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(
                stmt,
                &format!("SELECT {COLUMNS} FROM {CATALOG}.{SCHEMA}.{TABLE} LIMIT 0")
            ),
            SqlReturn::SUCCESS
        );
        let described = collect_describe_col(stmt);
        cleanup_stmt(stmt);
        assert!(!described.is_empty(), "expected columns to describe");

        // Catalog path: SQLColumns queries `information_schema.columns`
        // unqualified, which Trino resolves against the *session's* default
        // catalog: the shared connection defaults to `Catalog=tpcds`, so a
        // filter on `table_catalog='postgresql'` would return zero rows on
        // that session even though the table exists. Switch the shared
        // connection's session catalog/schema with `USE` rather than opening
        // a second connection: this file's own docs warn that two
        // independent reqwest connection pools hitting the same Trino
        // coordinator cause intermittent TCP socket corruption, and that was
        // reproducible here too. `USE` restores the original session
        // afterwards so later tests on the shared connection are unaffected.
        let (_, _, use_stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(use_stmt, &format!("USE {CATALOG}.{SCHEMA}")),
            SqlReturn::SUCCESS,
            "USE {CATALOG}.{SCHEMA} failed"
        );
        cleanup_stmt(use_stmt);

        let (_, _, cat_stmt) = alloc_stmt();
        let cat: Vec<u16> = CATALOG.encode_utf16().collect();
        let sch: Vec<u16> = SCHEMA.encode_utf16().collect();
        let tbl: Vec<u16> = TABLE.encode_utf16().collect();
        assert_eq!(
            ffi::metadata::sql_columns_w::<TrinoBackend>(
                cat_stmt,
                cat.as_ptr(),
                i16::try_from(cat.len()).expect("fits i16"),
                sch.as_ptr(),
                i16::try_from(sch.len()).expect("fits i16"),
                tbl.as_ptr(),
                i16::try_from(tbl.len()).expect("fits i16"),
                std::ptr::null(),
                0,
            ),
            SqlReturn::SUCCESS
        );

        let mut cataloged: Vec<(String, i16, usize, i16)> = Vec::new();
        while ffi::fetch::sql_fetch::<TrinoBackend>(cat_stmt) == SqlReturn::SUCCESS {
            cataloged.push((
                get_wchar_col(cat_stmt, 4),
                i16::try_from(get_i64_col(cat_stmt, 5)).expect("DATA_TYPE fits i16"),
                usize::try_from(get_i64_col(cat_stmt, 7)).unwrap_or(0),
                i16::try_from(get_i64_col(cat_stmt, 9)).unwrap_or(0),
            ));
        }
        cleanup_stmt(cat_stmt);

        // Restore the shared connection's session catalog/schema so later
        // tests that assume the original `Catalog=tpcds` connect-string
        // default are unaffected.
        let (_, _, restore_stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(restore_stmt, "USE tpcds.sf1"),
            SqlReturn::SUCCESS,
            "restoring USE tpcds.sf1 failed"
        );
        cleanup_stmt(restore_stmt);

        assert!(!cataloged.is_empty(), "expected SQLColumns to return rows");
        for (name, sql_type, size, scale) in described {
            let c = cataloged
                .iter()
                .find(|c| c.0 == name)
                .unwrap_or_else(|| panic!("{name} missing from SQLColumns"));
            assert_eq!(sql_type, c.1, "{name}: DATA_TYPE disagrees");
            assert_eq!(size, c.2, "{name}: COLUMN_SIZE disagrees");
            assert_eq!(scale, c.3, "{name}: DECIMAL_DIGITS disagrees");
        }
    }
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn set_pos_returns_hyc00() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(exec_direct(stmt, "SELECT 1 AS n"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let ret =
            ffi::cursor::sql_set_pos::<TrinoBackend>(stmt, 1, SQL_POSITION, SQL_LOCK_NO_CHANGE);
        assert_eq!(ret, SqlReturn::ERROR, "SQLSetPos should return ERROR");
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// Column-size round-trip matrix
//
// Verifies that DISPLAY_SIZE (not OCTET_LENGTH) is the right field to size a
// text buffer from, across representative Trino types via ad hoc `SELECT`
// literals (no table/DDL needed, matching this file's existing
// `exec_direct_select_and_fetch` convention), plus the three cross-family
// priority cases.
//
// Omitted from the metadata-sized backbone below, with reasons:
// - BOOLEAN/TINYINT/SMALLINT: fixed-width types whose COLUMN_SIZE is an
//   appendix constant, covered by `column_size.rs`'s own spec-table test.
//   BIGINT and DOUBLE below cover the same numeric shape against Trino.
// - VARBINARY/JSON/UUID/INTERVAL/ARRAY: string-representable types whose
//   rendering is covered elsewhere in this file
//   (`varbinary_get_data_returns_raw_bytes` and friends), away from the
//   temporal and DECIMAL sizing this matrix is about. VARBINARY cannot be
//   given a bounded declared length at all, since Trino's VARBINARY carries
//   no length parameter, so its DISPLAY_SIZE is the "unbounded" convention
//   (i32::MAX * 2, see `is_binary_type` in col_attr.rs). That is not an
//   allocatable buffer size: an application reads such a column with chunked
//   `SQLGetData` calls rather than sizing one buffer from COLUMN_SIZE, which
//   is what `varbinary_get_data_returns_raw_bytes` does.

/// Mirrors `odbc_sys::Timestamp` (`SQL_TIMESTAMP_STRUCT`)'s field layout so
/// this test file can read a `SQL_C_TYPE_TIMESTAMP` buffer without adding
/// `odbc-sys` as a direct (non-dev) dependency of this crate.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawTimestamp {
    year: i16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    fraction: u32,
}

/// Read `SQL_DESC_DISPLAY_SIZE` for one column via `SQLColAttributeW`.
unsafe fn column_display_size(stmt: *mut c_void, column_number: u16) -> usize {
    let mut chars: isize = 0;
    unsafe {
        assert_eq!(
            ffi::metadata::sql_col_attribute_w::<TrinoBackend>(
                stmt,
                column_number,
                Desc::DisplaySize as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut chars,
            ),
            SqlReturn::SUCCESS
        );
    }
    usize::try_from(chars).expect("DISPLAY_SIZE must not be negative")
}

/// Fetch column `column_number` as `SQL_C_WCHAR`, using a buffer sized
/// exactly from `SQL_DESC_DISPLAY_SIZE` (plus one UTF-16 code unit of slack
/// for the null terminator, which `DISPLAY_SIZE` does not include per spec).
unsafe fn get_data_wchar_sized_from_metadata(
    stmt: *mut c_void,
    column_number: u16,
) -> (SqlReturn, String) {
    let chars = unsafe { column_display_size(stmt, column_number) };
    let code_units = chars + 1;
    let mut buf: Vec<u16> = vec![0u16; code_units];
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            column_number,
            CDataType::WChar as i16,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as isize,
            &mut ind,
        )
    };
    let char_count = if ind > 0 { (ind / 2) as usize } else { 0 };
    (
        ret,
        String::from_utf16_lossy(&buf[..char_count.min(buf.len())]),
    )
}

/// Read the first diagnostic record's 5-character SQLSTATE off `stmt`.
unsafe fn last_sqlstate(stmt: *mut c_void) -> String {
    let mut state = [0u16; 6];
    let mut native: i32 = 0;
    let mut msg = [0u16; 256];
    let mut msg_len: i16 = 0;
    unsafe {
        assert_eq!(
            ffi::diag::sql_get_diag_rec_w::<TrinoBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            ),
            SqlReturn::SUCCESS,
            "no diagnostic record was pushed"
        );
    }
    String::from_utf16_lossy(&state[..5])
}

#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn metadata_sized_wchar_round_trip_covers_representative_types() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        // col_varchar comes from the real `types_test` table rather than a
        // bare `CAST('...' AS VARCHAR)` literal, so that it exercises the
        // normal path: a catalogued VARCHAR(200) column whose precision comes
        // from `information_schema`. A computed VARCHAR expression has no
        // catalog entry and therefore no declared length for
        // `trino_ty_precision` to read, which under-reports DISPLAY_SIZE for
        // that shape alone, separately from the temporal and DECIMAL sizing
        // this test is about.
        assert_eq!(
            exec_direct(
                stmt,
                "SELECT \
                     CAST(1234567890 AS BIGINT), \
                     CAST(3.5 AS DOUBLE), \
                     col_varchar, \
                     DATE '2024-03-05', \
                     TIME '13:30:15', \
                     TIMESTAMP '2024-03-05 13:30:15', \
                     CAST(123.45 AS DECIMAL(10,2)), \
                     TIME '13:30:15.123', \
                     TIMESTAMP '2024-03-05 13:30:15.123' \
                 FROM postgresql.public.types_test WHERE id = 1"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // Columns 8/9: every temporal fixture above them has a zero
        // fraction, which does not exercise fractional-second rendering. A
        // `time(3)`/`timestamp(3)` value must not be padded to a fixed 9
        // nanosecond digits against a reported DISPLAY_SIZE of 12/23 (the
        // ODBC "Column Size" appendix's `9 + s`/`20 + s` formula at `s` = 3).
        // These two columns pin that: trailing zeros are trimmed, so the
        // rendered text is exactly the 3 significant digits Trino sent, not 6
        // fabricated zeros appended to them.
        let expectations: &[(u16, &str)] = &[
            (1, "1234567890"),
            (2, "3.5"),
            (3, "hello world"),
            (4, "2024-03-05"),
            (5, "13:30:15"),
            (6, "2024-03-05 13:30:15"),
            (7, "123.45"),
            (8, "13:30:15.123"),
            (9, "2024-03-05 13:30:15.123"),
        ];

        for &(col, expected) in expectations {
            let (ret, text) = get_data_wchar_sized_from_metadata(stmt, col);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "column {col}: DISPLAY_SIZE-sized buffer was not big enough \
                 (metadata under-reported the size, or SUCCESS_WITH_INFO/ERROR \
                 was otherwise returned)"
            );
            assert_eq!(text, expected, "column {col}: unexpected text rendering");
        }

        cleanup_stmt(stmt);
    }
}

// --- Cross-family conversions ---

/// A native Trino DECIMAL value (`ColumnValue::Decimal(String)`,
/// see `type_conversion.rs`'s `json_to_column_value`) read as `SQL_C_DOUBLE`.
/// DECIMAL arrives as text from Trino's JSON wire format and must go through
/// `write_column_value`'s numeric-pivot (`parse_numeric_text`) arm to reach
/// a `SQL_C_DOUBLE` buffer, rather than any native binary decimal
/// representation.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn decimal_column_read_as_double() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT CAST(123.45 AS DECIMAL(10,2))"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut buf: f64 = 0.0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::Double as i16,
            &mut buf as *mut f64 as *mut c_void,
            std::mem::size_of::<f64>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!((buf - 123.45).abs() < 1e-9, "got {buf}");

        cleanup_stmt(stmt);
    }
}

/// The three IEEE specials must be readable as `SQL_C_DOUBLE` from both a
/// `DOUBLE` and a `REAL` column.
///
/// Trino has no JSON literal for them and sends `"NaN"`, `"Infinity"` and
/// `"-Infinity"` as strings, which `trino_special_float` recognises. Without
/// it they reach the application as text and fail the C conversion with
/// `22018`, leaving the value unreadable as a number. Only a live coordinator
/// can confirm the wire encoding this depends on, which is why it is not left
/// to the unit tests over `json_to_column_value`.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn ieee_special_floats_are_readable_as_c_double() {
    /// Predicate on the `f64` read back, since the specials do not compare
    /// equal to themselves and cannot be asserted with `assert_eq!`.
    type Check = fn(f64) -> bool;

    // (Trino expression, predicate on the value read back)
    let cases: &[(&str, Check)] = &[
        ("CAST(nan() AS DOUBLE)", |v| v.is_nan()),
        ("CAST(infinity() AS DOUBLE)", |v| {
            v.is_infinite() && v.is_sign_positive()
        }),
        ("CAST(-infinity() AS DOUBLE)", |v| {
            v.is_infinite() && v.is_sign_negative()
        }),
        ("CAST(nan() AS REAL)", |v| v.is_nan()),
        ("CAST(infinity() AS REAL)", |v| {
            v.is_infinite() && v.is_sign_positive()
        }),
        ("CAST(-infinity() AS REAL)", |v| {
            v.is_infinite() && v.is_sign_negative()
        }),
    ];

    unsafe {
        for (expr, ok) in cases {
            let (_env, _conn, stmt) = alloc_stmt();
            assert_eq!(
                exec_direct(stmt, &format!("SELECT {expr}")),
                SqlReturn::SUCCESS,
                "{expr} did not execute"
            );
            assert_eq!(
                ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
                SqlReturn::SUCCESS,
                "{expr} returned no row"
            );

            let mut buf: f64 = 0.0;
            let mut ind: isize = 0;
            let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::Double as i16,
                &mut buf as *mut f64 as *mut c_void,
                std::mem::size_of::<f64>() as isize,
                &mut ind,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "{expr} could not be read as SQL_C_DOUBLE: this is the 22018 \
                 that made IEEE specials unreadable"
            );
            assert!(ok(buf), "{expr} read back as {buf}");

            cleanup_stmt(stmt);
        }
    }
}

/// A statement terminator must not fail the statement.
///
/// Trino's REST API takes one statement per request and its grammar has no
/// terminator, so `SELECT 1;` is a `SYNTAX_ERROR` at the semicolon. ODBC tools
/// send one routinely (`isql` submits the line as typed), so this covers both
/// entry points that carry application SQL, including the prepared path where
/// the terminator survives parameter interpolation.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn a_trailing_semicolon_does_not_fail_the_statement() {
    unsafe {
        for sql in ["SELECT 1 AS n;", "SELECT 1 AS n ;  ", "SELECT 1 AS n;;"] {
            let (_env, _conn, stmt) = alloc_stmt();
            assert_eq!(
                exec_direct(stmt, sql),
                SqlReturn::SUCCESS,
                "{sql:?} did not execute"
            );
            assert_eq!(
                ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
                SqlReturn::SUCCESS,
                "{sql:?} returned no row"
            );
            assert_eq!(
                get_wchar_col(stmt, 1),
                "1",
                "{sql:?} returned the wrong value"
            );
            cleanup_stmt(stmt);
        }

        // The prepared path: the terminator is still on the template when
        // parameters are interpolated into it.
        let (_env, _conn, stmt) = alloc_stmt();
        let sql = "SELECT CAST(? AS VARCHAR) AS s;";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );
        let mut buf: Vec<u8> = b"hi".to_vec();
        let mut ind_in: isize = buf.len() as isize;
        assert_eq!(
            ffi::params::sql_bind_parameter::<TrinoBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Char as i16,
                SqlDataType::VARCHAR.0,
                buf.len(),
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as isize,
                &mut ind_in,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::execute::sql_execute::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS,
            "a prepared statement with a terminator did not execute"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(get_wchar_col(stmt, 1), "hi");
        cleanup_stmt(stmt);
    }
}

/// A server-side error's diagnostic must carry the summary and the native
/// code, and must not carry Trino's `failure_info`.
///
/// `QueryError`'s own `Display` renders the coordinator's Java stack, and core
/// walks the whole causal chain into the message, so a diagnostic carrying it
/// runs to thousands of characters: measured between 1,700 and 15,000 against
/// a live coordinator, `DIVISION_BY_ZERO` worst at ~168 frames. `QueryCause`
/// is what keeps the two apart.
///
/// The bound below is loose on purpose. It asserts that no stack is in there,
/// not a particular length, and a returning stack would exceed it by orders
/// of magnitude.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn server_error_diagnostics_carry_the_summary_not_the_java_stack() {
    // (SQL, Trino error name, native error code)
    let cases: &[(&str, &str, i32)] = &[
        (
            "SELECT nope FROM tpcds.sf1.customer",
            "COLUMN_NOT_FOUND",
            47,
        ),
        ("SELECT 1/0", "DIVISION_BY_ZERO", 8),
        ("SELECT CAST('abc' AS INTEGER)", "INVALID_CAST_ARGUMENT", 9),
    ];
    const MAX_DIAGNOSTIC_CHARS: usize = 500;

    unsafe {
        for (sql, error_name, want_native) in cases {
            let (_env, _conn, stmt) = alloc_stmt();
            // Some of these are rejected at planning and some only once a page
            // is fetched, so drive both before reading the diagnostic.
            if exec_direct(stmt, sql) == SqlReturn::SUCCESS {
                let _ = ffi::fetch::sql_fetch::<TrinoBackend>(stmt);
            }

            let mut state = [0u16; 6];
            let mut msg = [0u16; 4096];
            let mut msg_len: i16 = 0;
            let mut native: i32 = 0;
            let ret = ffi::diag::sql_get_diag_rec_w::<TrinoBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                // In characters for SQLGetDiagRec, unlike SQLGetDiagField.
                msg.len() as i16,
                &mut msg_len,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "{sql}: expected a diagnostic record"
            );

            let text = String::from_utf16_lossy(&msg[..msg_len as usize]);
            assert!(
                text.contains(error_name),
                "{sql}: diagnostic does not name the error: {text}"
            );
            assert_eq!(native, *want_native, "{sql}: wrong native error code");
            assert!(
                !text.contains("io.trino") && !text.contains("java."),
                "{sql}: Trino's failure_info reached the diagnostic: {text}"
            );
            assert!(
                text.chars().count() <= MAX_DIAGNOSTIC_CHARS,
                "{sql}: diagnostic is {} chars, over the {MAX_DIAGNOSTIC_CHARS} \
                 bound that stands in for 'carries no stack': {text}",
                text.chars().count()
            );

            cleanup_stmt(stmt);
        }
    }
}

/// A VARCHAR value holding digit text, read as `SQL_C_SBIGINT`, must succeed
/// (the ODBC conversion matrix requires CHAR/VARCHAR to convert to every C
/// type); the same shape holding non-numeric text must fail with the
/// specific SQLSTATE the spec defines (22018), not merely "some error".
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn numeric_looking_text_column_read_as_sbigint() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(stmt, "SELECT CAST('12345' AS VARCHAR)"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut buf as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(buf, 12345);
        cleanup_stmt(stmt);

        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT CAST('not a number' AS VARCHAR)"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf2: i64 = 0;
        let mut ind2: isize = 0;
        let ret2 = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut buf2 as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>() as isize,
            &mut ind2,
        );
        assert_eq!(ret2, SqlReturn::ERROR);
        assert_eq!(last_sqlstate(stmt), "22018");
        cleanup_stmt(stmt);
    }
}

/// A VARCHAR value holding timestamp-shaped text (e.g. the result of
/// `CAST(... AS VARCHAR)` on a temporal expression, or any text column that
/// happens to look like a timestamp), read as `SQL_C_TYPE_TIMESTAMP`. This
/// is the Trino analogue of "a temporal column read as SQL_C_TYPE_TIMESTAMP
/// where the stored value is text": Trino's native TIMESTAMP columns are
/// already parsed to `ColumnValue::Timestamp` before `write_column_value`
/// runs (see `type_conversion.rs`), so only a VARCHAR-typed source reaches
/// `write_column_value`'s `(ColumnValue::String, CDataType::TypeTimestamp)`
/// arm end to end.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_shaped_text_column_read_as_type_timestamp() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(
                stmt,
                "SELECT CAST(TIMESTAMP '2024-03-05 13:30:15' AS VARCHAR)"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut buf = RawTimestamp {
            year: 0,
            month: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            fraction: 0,
        };
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::TypeTimestamp as i16,
            &mut buf as *mut RawTimestamp as *mut c_void,
            std::mem::size_of::<RawTimestamp>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "well-formed timestamp-shaped text");
        assert_eq!((buf.year, buf.month, buf.day), (2024, 3, 5));
        assert_eq!((buf.hour, buf.minute, buf.second), (13, 30, 15));
        cleanup_stmt(stmt);

        let (_env, _conn, stmt) = alloc_stmt();
        assert_eq!(
            exec_direct(stmt, "SELECT CAST('not-a-timestamp' AS VARCHAR)"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf2 = buf;
        let mut ind2: isize = 0;
        let ret2 = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::TypeTimestamp as i16,
            &mut buf2 as *mut RawTimestamp as *mut c_void,
            std::mem::size_of::<RawTimestamp>() as isize,
            &mut ind2,
        );
        assert_eq!(ret2, SqlReturn::ERROR);
        assert_eq!(last_sqlstate(stmt), "22018");
        cleanup_stmt(stmt);
    }
}

// ---------------------------------------------------------------------------
// Statement and connection attributes
// ---------------------------------------------------------------------------
//
// Core owns every one of these paths, but they are only observable through a
// driver: `sql_set_stmt_attr_w::<TrinoBackend>` is what an application calls,
// and what it returns is what an application plans around. The tests here
// drive the real entry points with `TrinoBackend` as the type parameter, so a
// core change that alters the contract fails this crate's suite rather than
// being noticed by a BI tool.
//
// None of them need a live coordinator: `SQLSetStmtAttr` and `SQLGetStmtAttr`
// touch the handle's attribute map and nothing else, and the connection-level
// tests use the injected network-free `TrinoConnection`.

/// `SQL_ATTR_ENLIST_IN_DTC` (1207), which `odbc_sys::ConnectionAttribute` does
/// not model.
const SQL_ATTR_ENLIST_IN_DTC: i32 = 1207;

/// Reads one statement attribute back through `SQLGetStmtAttr`.
///
/// The buffer is zeroed rather than poisoned, because core currently writes
/// only four bytes for the integer-valued attributes; see
/// [`statement_attribute_read_back_width_is_narrower_than_the_spec_declares`],
/// which owns that question. Every value these tests compare fits in 32 bits,
/// so this reads correctly both before and after that is fixed.
unsafe fn get_stmt_attr(stmt: *mut c_void, attribute: i32) -> (SqlReturn, usize) {
    let mut value: usize = 0;
    let mut string_length: i32 = 0;
    let ret = unsafe {
        ffi::stmt_attr::sql_get_stmt_attr_w::<TrinoBackend>(
            stmt,
            attribute,
            &mut value as *mut usize as *mut c_void,
            0,
            &mut string_length,
        )
    };
    (ret, value)
}

/// Sets one integer-valued statement attribute through `SQLSetStmtAttr`.
unsafe fn set_stmt_attr(stmt: *mut c_void, attribute: i32, value: usize) -> SqlReturn {
    unsafe {
        ffi::stmt_attr::sql_set_stmt_attr_w::<TrinoBackend>(
            stmt,
            attribute,
            std::ptr::without_provenance_mut(value),
            0,
        )
    }
}

/// The first diagnostic record's SQLSTATE on any handle, or `""` when there is
/// none. Unlike [`last_sqlstate`] this does not assert one exists: the
/// attribute tests below check *both* that a warning is posted and that a
/// plain success posts nothing.
unsafe fn sqlstate_of(handle_type: HandleType, handle: *mut c_void) -> String {
    let mut state = [0u16; 6];
    let mut native: i32 = 0;
    let mut msg = [0u16; 256];
    let mut msg_len: i16 = 0;
    let ret = unsafe {
        ffi::diag::sql_get_diag_rec_w::<TrinoBackend>(
            handle_type as i16,
            handle,
            1,
            state.as_mut_ptr(),
            &mut native,
            msg.as_mut_ptr(),
            msg.len() as i16,
            &mut msg_len,
        )
    };
    if ret == SqlReturn::SUCCESS {
        String::from_utf16_lossy(&state[..5])
    } else {
        String::new()
    }
}

/// The spec's `01S02` row closes the set of statement attributes a driver may
/// substitute for, and core stores the value it will use for each rather than
/// the one asked for. That is what makes the row's parenthesis true
/// ("`SQLGetStmtAttr` can be called to determine the temporarily substituted
/// value."), and it is the half an application acts on: a tool that sets
/// `SQL_ATTR_MAX_ROWS = 100` and reads back `100` expects at most a hundred
/// rows, while this driver returns every one.
///
/// `SQL_ATTR_CURSOR_SCROLLABLE` and `SQL_ATTR_PARAMSET_SIZE` are checked
/// alongside them although the spec's list names neither. Both are documented
/// deviations at their arms in core, and pinning them here stops the
/// deviation being undone by accident.
///
/// `SQL_ATTR_QUERY_TIMEOUT` is **not** in this list. This driver answers
/// `Backend::set_query_timeout`, so a statement on a live connection accepts
/// the value instead of substituting `0`; see
/// `query_timeout_is_accepted_on_a_connected_statement`. It would pass here
/// anyway, because these handles are never connected and core substitutes
/// with no connection to offer the value to, and that is what makes keeping
/// it misleading: the assertion would hold for a reason unrelated to what it
/// claims.
#[test]
fn set_stmt_attr_substitutes_and_reports_the_value_it_will_use() {
    // (attribute, requested value, the value core will report back)
    let cases: &[(StatementAttribute, usize, usize, &str)] = &[
        (
            StatementAttribute::Concurrency,
            2,
            1,
            "SQL_CONCUR_READ_ONLY",
        ),
        (
            StatementAttribute::CursorType,
            2,
            0,
            "SQL_CURSOR_FORWARD_ONLY",
        ),
        (StatementAttribute::KeysetSize, 50, 0, "fully keyset-driven"),
        (StatementAttribute::MaxLength, 4096, 0, "all available data"),
        (StatementAttribute::MaxRows, 100, 0, "no row limit"),
        (StatementAttribute::RowArraySize, 64, 1, "one-row rowset"),
        (
            StatementAttribute::SimulateCursor,
            2,
            0,
            "SQL_SC_NON_UNIQUE",
        ),
        // The two deviations from the spec's closed list.
        (
            StatementAttribute::CursorScrollable,
            1,
            0,
            "SQL_NONSCROLLABLE",
        ),
        (
            StatementAttribute::ParamsetSize,
            500,
            1,
            "one parameter set",
        ),
    ];

    unsafe {
        for &(attr, requested, substituted, why) in cases {
            let (env, conn, stmt) = alloc_handles();

            assert_eq!(
                set_stmt_attr(stmt, attr as i32, requested),
                SqlReturn::SUCCESS_WITH_INFO,
                "{attr:?} = {requested} must be reported as substituted ({why})"
            );
            assert_eq!(
                sqlstate_of(HandleType::Stmt, stmt),
                "01S02",
                "{attr:?}: the spec's SQLSTATE for a substituted attribute value"
            );

            let (ret, read_back) = get_stmt_attr(stmt, attr as i32);
            assert_eq!(ret, SqlReturn::SUCCESS, "{attr:?} must be readable");
            assert_eq!(
                read_back, substituted,
                "{attr:?}: SQLGetStmtAttr must report the substituted value \
                 ({substituted}, {why}), not the {requested} that was asked for"
            );

            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
        }
    }
}

/// The value each of those attributes already holds is accepted plainly,
/// `SQL_SUCCESS`, no diagnostic. Without this the test above would still pass
/// if core substituted unconditionally, which would post a warning on every
/// tool that sets an attribute to the value the driver already uses.
#[test]
fn set_stmt_attr_accepts_the_value_it_already_uses_without_a_warning() {
    let cases: &[(StatementAttribute, usize)] = &[
        (StatementAttribute::Concurrency, 1),
        (StatementAttribute::CursorType, 0),
        (StatementAttribute::KeysetSize, 0),
        (StatementAttribute::MaxLength, 0),
        (StatementAttribute::MaxRows, 0),
        (StatementAttribute::RowArraySize, 1),
        (StatementAttribute::SimulateCursor, 0),
        (StatementAttribute::CursorScrollable, 0),
        (StatementAttribute::ParamsetSize, 1),
    ];

    unsafe {
        for &(attr, value) in cases {
            let (env, conn, stmt) = alloc_handles();

            assert_eq!(
                set_stmt_attr(stmt, attr as i32, value),
                SqlReturn::SUCCESS,
                "{attr:?} = {value} is what the driver already does"
            );
            assert_eq!(
                sqlstate_of(HandleType::Stmt, stmt),
                "",
                "{attr:?} = {value} must post no diagnostic"
            );

            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
        }
    }
}

/// `SQL_ATTR_QUERY_TIMEOUT` is accepted plainly on a connected statement, and
/// reads back the value the application asked for.
///
/// This is the half that matters: core arms its timer only when
/// `Backend::set_query_timeout` answers `Ok`, and `SQLGetStmtAttr` reporting
/// the requested value rather than `0` is how an application learns its
/// deadline is really in force. The driver answers `QueryTimeout::CoreCancels`,
/// so both follow.
///
/// A *connected* handle is required, and that is the whole point of using
/// `attach_connection` here: `offer_to_data_source` substitutes without
/// consulting the backend when it has no connection, so the plain
/// `alloc_handles` used by the neighbouring tests would exercise the fallback
/// and never reach `TrinoBackend::set_query_timeout` at all.
#[test]
fn query_timeout_is_accepted_on_a_connected_statement() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();
        let mut stmt: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Stmt as i16, conn, &mut stmt),
            SqlReturn::SUCCESS
        );

        assert_eq!(
            set_stmt_attr(stmt, StatementAttribute::QueryTimeout as i32, 30),
            SqlReturn::SUCCESS,
            "this driver enforces SQL_ATTR_QUERY_TIMEOUT, so it must not be substituted"
        );
        assert_eq!(
            sqlstate_of(HandleType::Stmt, stmt),
            "",
            "an accepted attribute posts no diagnostic; 01S02 would tell the \
             application its deadline had been capped"
        );

        let (ret, read_back) = get_stmt_attr(stmt, StatementAttribute::QueryTimeout as i32);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            read_back, 30,
            "SQLGetStmtAttr must report the timeout that was set, not the 0 core \
             substitutes for a backend that cannot enforce one"
        );

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        cleanup_injected_conn(env, conn);
    }
}

/// An attribute off the `01S02` list has no substitution to offer, so the
/// value is refused outright with `HYC00`, "optional feature not
/// implemented". The distinction matters to an application: `01S02` says
/// "I did something else", `HYC00` says "I did nothing", and reading a
/// substituted value back is only meaningful for the first.
#[test]
fn set_stmt_attr_reports_hyc00_for_a_value_it_cannot_substitute_for() {
    // (attribute, an unhonourable value, what it would mean)
    let cases: &[(StatementAttribute, usize, &str)] = &[
        (StatementAttribute::UseBookmarks, 2, "SQL_UB_VARIABLE"),
        (StatementAttribute::RetrieveData, 0, "SQL_RD_OFF"),
        (StatementAttribute::CursorSensitivity, 2, "SQL_SENSITIVE"),
        (StatementAttribute::EnableAutoIpd, 1, "SQL_TRUE"),
        (StatementAttribute::AsyncEnable, 1, "SQL_ASYNC_ENABLE_ON"),
    ];

    unsafe {
        for &(attr, value, meaning) in cases {
            let (env, conn, stmt) = alloc_handles();

            assert_eq!(
                set_stmt_attr(stmt, attr as i32, value),
                SqlReturn::ERROR,
                "{attr:?} = {meaning} is not implemented and must be refused"
            );
            assert_eq!(
                sqlstate_of(HandleType::Stmt, stmt),
                "HYC00",
                "{attr:?} = {meaning}: the spec's SQLSTATE for a valid attribute \
                 whose value the driver does not support"
            );

            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
            let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
        }
    }
}

/// Every statement attribute `SQLSetStmtAttr` accepts can be read back.
///
/// A value stored but not readable is worse than one refused: the application
/// sets it, gets `SQL_SUCCESS`, and then gets `HYC00` asking what it is. The
/// pointer-valued attributes are the ones this covers that nothing else does:
/// a tool binding a row-status array reads the pointer back to confirm the
/// driver took it.
#[test]
fn every_statement_attribute_the_driver_accepts_is_readable() {
    let mut sink: usize = 0;
    let ptr = &mut sink as *mut usize as usize;

    // (attribute, a value the driver honours as-is)
    let cases: &[(StatementAttribute, usize)] = &[
        (StatementAttribute::NoScan, 0),
        (StatementAttribute::RowBindType, 0),
        (StatementAttribute::ParamBindType, 0),
        (StatementAttribute::MetadataId, 1),
        (StatementAttribute::RowsFetchedPtr, ptr),
        (StatementAttribute::RowStatusPtr, ptr),
        (StatementAttribute::RowBindOffsetPtr, ptr),
        (StatementAttribute::RowOperationPtr, ptr),
        (StatementAttribute::ParamsProcessedPtr, ptr),
        (StatementAttribute::ParamStatusPtr, ptr),
        (StatementAttribute::ParamBindOffsetPtr, ptr),
        (StatementAttribute::ParamOpterationPtr, ptr),
        (StatementAttribute::FetchBookmarkPtr, 0),
    ];

    unsafe {
        let (env, conn, stmt) = alloc_handles();

        for &(attr, value) in cases {
            assert_eq!(
                set_stmt_attr(stmt, attr as i32, value),
                SqlReturn::SUCCESS,
                "{attr:?} = {value:#x} must be accepted"
            );
            let (ret, read_back) = get_stmt_attr(stmt, attr as i32);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "{attr:?} was accepted, so SQLGetStmtAttr must answer it"
            );
            assert_eq!(read_back, value, "{attr:?} must read back as what was set");
        }

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// `SQL_ATTR_METADATA_ID` is one of exactly two attributes `SQLSetStmtAttr`'s
/// Comments allow an application to set at the connection level, and a
/// statement allocated afterwards must start from the connection's value.
///
/// This is not a cosmetic read-back: `metadata_id_enabled` consults the
/// *statement's* map, and it is what decides whether the catalog functions
/// treat their arguments as identifiers (case-folded, wildcards escaped) or as
/// search patterns. An application taking the connection-level route must not
/// get `SQL_SUCCESS`, see its value echoed by `SQLGetConnectAttr`, and then get
/// pattern semantics with no diagnostic saying so. For this driver that
/// mismatch means `SQLColumns(table_name = "my_table")` matching `my7table` as
/// well, because `_` is a wildcard.
///
/// The ODBC 2.x rule the connection-level route inherits makes this the
/// default for statements allocated *afterwards* only, so the statement that
/// already existed is asserted to be untouched in the same test.
#[test]
fn metadata_id_set_on_the_connection_reaches_statements_allocated_after_it() {
    unsafe {
        let (env, conn, before) = alloc_handles();

        // A statement that predates the connection-level setting.
        assert_eq!(
            get_stmt_attr(before, StatementAttribute::MetadataId as i32).1,
            0
        );

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::METADATA_ID.0,
                std::ptr::without_provenance_mut(1usize), // SQL_TRUE
                0,
            ),
            SqlReturn::SUCCESS
        );

        let mut after: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<TrinoBackend>(
                HandleType::Stmt as i16,
                conn,
                &mut after
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            get_stmt_attr(after, StatementAttribute::MetadataId as i32),
            (SqlReturn::SUCCESS, 1),
            "a statement allocated after SQLSetConnectAttr(SQL_ATTR_METADATA_ID, \
             SQL_TRUE) must inherit it"
        );

        assert_eq!(
            get_stmt_attr(before, StatementAttribute::MetadataId as i32).1,
            0,
            "a statement that already existed is untouched, per the ODBC 2.x \
             rule the connection-level route inherits"
        );

        // A later SQLSetStmtAttr still overrides the inherited value.
        assert_eq!(
            set_stmt_attr(after, StatementAttribute::MetadataId as i32, 0),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            get_stmt_attr(after, StatementAttribute::MetadataId as i32).1,
            0
        );

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, after);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, before);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// The two connection attributes whose state and support rules the
/// `SQLSetConnectAttr` page assigns to the driver rather than to the Driver
/// Manager.
///
/// `SQL_ATTR_PACKET_SIZE` is stated directly ("if the application sets packet
/// size after a connection has already been made, the driver will return
/// SQLSTATE HY011"), and needs a connection to be open, which is what the
/// injected `TrinoConnection` supplies without a coordinator. It is accepted
/// before one, since a driver that refused it there would have no legal moment
/// to accept it at all.
///
/// `SQL_ATTR_ENLIST_IN_DTC` and `SQL_ATTR_ASYNC_ENABLE = SQL_ASYNC_ENABLE_ON`
/// are `HYC00` at any time: this driver reports `SQL_AM_NONE` for
/// `SQL_ASYNC_MODE` and enlists in no distributed transaction, so accepting
/// either would leave an application believing in behaviour it does not get.
#[test]
fn set_connect_attr_enforces_the_rules_the_spec_assigns_to_the_driver() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::PACKET_SIZE.0,
                std::ptr::without_provenance_mut(8192usize),
                0,
            ),
            SqlReturn::ERROR,
            "SQL_ATTR_PACKET_SIZE after the connection is open"
        );
        assert_eq!(sqlstate_of(HandleType::Dbc, conn), "HY011");

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                SQL_ATTR_ENLIST_IN_DTC,
                std::ptr::without_provenance_mut(1usize),
                0,
            ),
            SqlReturn::ERROR,
            "SQL_ATTR_ENLIST_IN_DTC"
        );
        assert_eq!(sqlstate_of(HandleType::Dbc, conn), "HYC00");

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::ASYNC_ENABLE.0,
                std::ptr::without_provenance_mut(1usize), // SQL_ASYNC_ENABLE_ON
                0,
            ),
            SqlReturn::ERROR,
            "SQL_ATTR_ASYNC_ENABLE = SQL_ASYNC_ENABLE_ON, with SQL_ASYNC_MODE = SQL_AM_NONE"
        );
        assert_eq!(sqlstate_of(HandleType::Dbc, conn), "HYC00");

        // SQL_ASYNC_ENABLE_OFF is the value the driver already uses.
        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::ASYNC_ENABLE.0,
                std::ptr::without_provenance_mut(0usize),
                0,
            ),
            SqlReturn::SUCCESS
        );

        cleanup_injected_conn(env, conn);
    }
}

/// `SQL_ATTR_AUTOCOMMIT` round-trips through the exported entry points, and
/// `SQLEndTran` with nothing open succeeds.
///
/// Offline on purpose: `set_autocommit` records the mode and issues nothing,
/// and `end_tran` reads the session's transaction id without touching the
/// network, so both halves of the contract are exercised with no coordinator.
/// The commit that reaches a coordinator is covered by the backend tests and by
/// `integration-tests/suites/test_transactions.py`.
///
/// `SQLEndTran` returning `SQL_SUCCESS` here is the spec's own requirement:
/// "calling SQLEndTran with either SQL_COMMIT or SQL_ROLLBACK when no
/// transaction is active returns SQL_SUCCESS". Trino answers
/// `NOT_IN_TRANSACTION` to the same statement, so the driver must not send it.
#[test]
fn autocommit_round_trips_and_end_tran_with_nothing_open_succeeds() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        for (value, name) in [
            (SQL_AUTOCOMMIT_OFF, "SQL_AUTOCOMMIT_OFF"),
            (SQL_AUTOCOMMIT_ON, "SQL_AUTOCOMMIT_ON"),
        ] {
            assert_eq!(
                ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                    conn,
                    odbc_sys::ConnectionAttribute::AUTOCOMMIT.0,
                    std::ptr::without_provenance_mut(value),
                    0,
                ),
                SqlReturn::SUCCESS,
                "setting {name}"
            );
            assert_eq!(sqlstate_of(HandleType::Dbc, conn), "");

            let mut read: u32 = u32::MAX;
            assert_eq!(
                ffi::connect_attr::sql_get_connect_attr_w::<TrinoBackend>(
                    conn,
                    odbc_sys::ConnectionAttribute::AUTOCOMMIT.0,
                    (&raw mut read).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
                "reading back {name}"
            );
            assert_eq!(
                read as usize, value,
                "{name} did not survive the round trip"
            );
        }

        for (completion, name) in [
            (odbc_sys::CompletionType::Commit, "SQL_COMMIT"),
            (odbc_sys::CompletionType::Rollback, "SQL_ROLLBACK"),
        ] {
            assert_eq!(
                ffi::tran::sql_end_tran::<TrinoBackend>(
                    HandleType::Dbc as i16,
                    conn,
                    completion as i16,
                ),
                SqlReturn::SUCCESS,
                "SQLEndTran({name}) with no transaction open"
            );
            assert_eq!(sqlstate_of(HandleType::Dbc, conn), "");
        }

        cleanup_injected_conn(env, conn);
    }
}

/// `SQL_ATTR_PACKET_SIZE` before a connection exists is accepted, which is the
/// other half of the `HY011` rule above: the spec's restriction is on setting
/// it *after* connecting, so refusing it always would leave the attribute with
/// no legal moment.
#[test]
fn set_connect_attr_accepts_packet_size_before_connecting() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::PACKET_SIZE.0,
                std::ptr::without_provenance_mut(8192usize),
                0,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(sqlstate_of(HandleType::Dbc, conn), "");

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// Every statement attribute is written at the width the spec declares.
///
/// `SQLSetStmtAttr`'s page declares every non-pointer statement attribute it
/// lists as "An SQLULEN value": `SQL_ATTR_CONCURRENCY`,
/// `SQL_ATTR_CURSOR_TYPE`, `SQL_ATTR_NOSCAN`, `SQL_ATTR_METADATA_ID`,
/// `SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_MAX_ROWS`, `SQL_ATTR_ROW_ARRAY_SIZE`
/// and the rest. Not one is `SQLUINTEGER`, and `SQLULEN` is 64 bits on a
/// 64-bit platform.
///
/// A short write is the same class of defect as a wrongly-shaped `SQLGetInfo`
/// answer, in the other direction. `SQLGetStmtAttr`'s `BufferLength` is
/// ignored for a non-string value, so an application writing
/// `SQLULEN v; SQLGetStmtAttr(stmt, SQL_ATTR_MAX_ROWS, &v, 0, NULL);` keeps
/// whatever was on its stack in the top four bytes of `v` and reads an
/// enormous row limit rather than the `0` core reported. The buffer below is
/// poisoned rather than zeroed for that reason: a zeroed one cannot tell a
/// correct write from a short one.
#[test]
fn statement_attributes_are_written_at_the_full_sqlulen_width() {
    // The integer-valued attributes, each at a value whose top half is zero,
    // so a short write leaves the poison visible.
    let cases: &[(StatementAttribute, usize)] = &[
        (StatementAttribute::QueryTimeout, 0),
        (StatementAttribute::MaxRows, 0),
        (StatementAttribute::MaxLength, 0),
        (StatementAttribute::KeysetSize, 0),
        (StatementAttribute::RowArraySize, 1),
        (StatementAttribute::ParamsetSize, 1),
        (StatementAttribute::Concurrency, 1),
        (StatementAttribute::CursorType, 0),
        (StatementAttribute::NoScan, 0),
        (StatementAttribute::RowBindType, 0),
        (StatementAttribute::ParamBindType, 0),
        (StatementAttribute::MetadataId, 0),
        (StatementAttribute::CursorScrollable, 0),
        (StatementAttribute::CursorSensitivity, 0),
        (StatementAttribute::SimulateCursor, 0),
        (StatementAttribute::RetrieveData, 1),
        (StatementAttribute::UseBookmarks, 0),
        (StatementAttribute::EnableAutoIpd, 0),
        (StatementAttribute::AsyncEnable, 0),
    ];

    unsafe {
        let (env, conn, stmt) = alloc_handles();

        for &(attr, expected) in cases {
            let mut value: usize = usize::MAX;
            let mut string_length: i32 = 0;
            assert_eq!(
                ffi::stmt_attr::sql_get_stmt_attr_w::<TrinoBackend>(
                    stmt,
                    attr as i32,
                    &mut value as *mut usize as *mut c_void,
                    0,
                    &mut string_length,
                ),
                SqlReturn::SUCCESS,
                "{attr:?} must be readable"
            );
            assert_eq!(
                value, expected,
                "{attr:?}: the poisoned top half survived, so the write was \
                 narrower than the SQLULEN the spec declares"
            );
            assert_eq!(
                string_length,
                std::mem::size_of::<usize>() as i32,
                "{attr:?}: StringLength must be size_of::<SQLULEN>()"
            );
        }

        // The pointer-valued attributes were always full width; asserted
        // alongside so the two groups cannot drift apart.
        let mut sink: usize = 0;
        let ptr = &mut sink as *mut usize as usize;
        assert_eq!(
            set_stmt_attr(stmt, StatementAttribute::RowsFetchedPtr as i32, ptr),
            SqlReturn::SUCCESS
        );
        let mut back: usize = usize::MAX;
        let mut ptr_len: i32 = 0;
        assert_eq!(
            ffi::stmt_attr::sql_get_stmt_attr_w::<TrinoBackend>(
                stmt,
                StatementAttribute::RowsFetchedPtr as i32,
                &mut back as *mut usize as *mut c_void,
                0,
                &mut ptr_len,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(back, ptr, "SQL_ATTR_ROWS_FETCHED_PTR round-trips whole");
        assert_eq!(ptr_len, std::mem::size_of::<usize>() as i32);

        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

/// An execution reports its parameter set through
/// `SQL_ATTR_PARAMS_PROCESSED_PTR` and `SQL_ATTR_PARAM_STATUS_PTR`.
///
/// The parameter-side counterpart of what `SQLFetch` already writes through
/// `SQL_ATTR_ROWS_FETCHED_PTR`. An application that binds a status array to
/// detect per-set errors and gets nothing written back reads its own initial
/// buffer contents, which is indistinguishable from every set having
/// succeeded.
///
/// Driven against a live coordinator rather than a mock because the value of
/// the status element is decided by whether the *execution* succeeded, and a
/// real rejection by Trino is the only way to reach `SQL_PARAM_ERROR` through
/// the same path an application does.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn execution_writes_the_processed_count_and_the_parameter_status() {
    unsafe {
        // --- A successful execution ---
        let (_env, _conn, stmt) = alloc_stmt();

        let mut processed: usize = usize::MAX;
        let mut status: [u16; 4] = [0xBEEF; 4];
        assert_eq!(
            set_stmt_attr(
                stmt,
                StatementAttribute::ParamsProcessedPtr as i32,
                &mut processed as *mut usize as usize,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            set_stmt_attr(
                stmt,
                StatementAttribute::ParamStatusPtr as i32,
                status.as_mut_ptr() as usize,
            ),
            SqlReturn::SUCCESS
        );

        let mut value: i64 = 42;
        assert_eq!(bind_i64(stmt, 1, &mut value), SqlReturn::SUCCESS);
        assert_eq!(
            exec_direct(stmt, "SELECT CAST(? AS bigint)"),
            SqlReturn::SUCCESS,
            "{}",
            diag_message(stmt)
        );

        assert_eq!(
            processed, 1,
            "SQL_ATTR_PARAMSET_SIZE is pinned at 1, so exactly one set is processed"
        );
        assert_eq!(
            status[0], SQL_PARAM_SUCCESS,
            "the first status element after a successful execution"
        );
        assert_eq!(
            status[1], 0xBEEF,
            "only the sets actually processed are written; element 2 is untouched"
        );
        assert_eq!(fetch_one_i64(stmt), 42);
        cleanup_stmt(stmt);

        // --- A rejected execution ---
        let (_env, _conn, stmt) = alloc_stmt();

        let mut processed: usize = usize::MAX;
        let mut status: [u16; 4] = [0xBEEF; 4];
        assert_eq!(
            set_stmt_attr(
                stmt,
                StatementAttribute::ParamsProcessedPtr as i32,
                &mut processed as *mut usize as usize,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            set_stmt_attr(
                stmt,
                StatementAttribute::ParamStatusPtr as i32,
                status.as_mut_ptr() as usize,
            ),
            SqlReturn::SUCCESS
        );

        let mut value: i64 = 1;
        assert_eq!(bind_i64(stmt, 1, &mut value), SqlReturn::SUCCESS);
        // Trino rejects the reference to a table that does not exist, so the
        // failure comes from the coordinator rather than from parameter
        // handling, which is the case an application binds a status array for.
        assert_eq!(
            exec_direct(stmt, "SELECT ? FROM does_not_exist_zzz"),
            SqlReturn::ERROR
        );
        assert_eq!(
            processed, 1,
            "the processed count includes error sets, per the spec's \
             \"including error sets\""
        );
        assert_eq!(
            status[0], SQL_PARAM_ERROR,
            "the status element for a set whose execution failed"
        );
        cleanup_stmt(stmt);
    }
}

/// `SQL_ATTR_CURRENT_CATALOG` and `SQL_DATABASE_NAME` are one value under two
/// names, so they must agree.
///
/// The spec says so directly: "in ODBC 3.x, the value returned for this
/// InfoType can also be returned by calling `SQLGetConnectAttr` with an
/// Attribute argument of `SQL_ATTR_CURRENT_CATALOG`".
///
/// [`TrinoBackend::current_catalog`] is the single source both read, which is
/// why `info.rs` has an arm for neither. Two sources means a connection
/// opened against `tpcds` reporting `"tpcds"` under one name and `""` under
/// the other, since a handle-local attribute string has nothing to seed it.
#[test]
fn the_current_catalog_reads_the_same_under_both_of_its_names() {
    unsafe {
        for catalog in [Some("tpcds"), None] {
            let expected = catalog.unwrap_or("");

            let mut env: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                ffi::handle::sql_alloc_handle::<TrinoBackend>(
                    HandleType::Env as i16,
                    std::ptr::null_mut(),
                    &mut env,
                ),
                SqlReturn::SUCCESS
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                ffi::handle::sql_alloc_handle::<TrinoBackend>(
                    HandleType::Dbc as i16,
                    env,
                    &mut conn
                ),
                SqlReturn::SUCCESS
            );
            attach_connection::<TrinoBackend>(conn, disconnected_trino_conn_with_catalog(catalog))
                .expect("valid conn handle");

            let mut buf = [0u16; 128];
            let mut len: i32 = 0;
            assert_eq!(
                ffi::connect_attr::sql_get_connect_attr_w::<TrinoBackend>(
                    conn,
                    odbc_sys::ConnectionAttribute::CURRENT_CATALOG.0,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 2) as i32,
                    &mut len,
                ),
                SqlReturn::SUCCESS
            );
            let attr = String::from_utf16_lossy(&buf[..(len / 2).max(0) as usize]);
            assert_eq!(
                attr, expected,
                "SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG) for catalog {catalog:?}"
            );

            let (ret, info) = stackable_odbc_core::conformance::observe_string_value::<TrinoBackend>(
                conn,
                stackable_odbc_core::types::SQL_DATABASE_NAME,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                info, attr,
                "SQL_DATABASE_NAME and SQL_ATTR_CURRENT_CATALOG are the same \
                 value under two names and must not disagree"
            );

            cleanup_injected_conn(env, conn);
        }
    }
}

/// `SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` reports `HYC00`, because this
/// driver cannot switch catalogs.
///
/// Core's `set_current_catalog` default is left in place. Trino's only
/// catalog-switching statement is `USE`, whose grammar requires a schema
/// (`USE postgresql` is `NOT_FOUND`, parsed as a schema name), so honouring
/// the call would mean inventing a schema and silently moving the session's
/// unqualified name resolution into it. See the comment beside
/// `TrinoBackend::current_catalog` for the coordinator probes.
///
/// An application that sets the attribute therefore gets `SQL_ERROR`. That is
/// the honest answer: succeeding would report a switch that did not happen.
#[test]
fn setting_the_current_catalog_is_refused_rather_than_silently_ignored() {
    unsafe {
        let (env, conn) = alloc_conn_with_injected_trino_connection();

        let wide: Vec<u16> = "postgresql".encode_utf16().collect();
        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::CURRENT_CATALOG.0,
                wide.as_ptr() as *mut c_void,
                (wide.len() * 2) as i32,
            ),
            SqlReturn::ERROR
        );
        assert_eq!(sqlstate_of(HandleType::Dbc, conn), "HYC00");

        // And nothing was stored: a refused switch must not move what the
        // readers report, or the attribute would claim a catalog the session
        // is not using.
        let mut buf = [0u16; 128];
        let mut len: i32 = 0;
        assert_eq!(
            ffi::connect_attr::sql_get_connect_attr_w::<TrinoBackend>(
                conn,
                odbc_sys::ConnectionAttribute::CURRENT_CATALOG.0,
                buf.as_mut_ptr().cast(),
                (buf.len() * 2) as i32,
                &mut len,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            String::from_utf16_lossy(&buf[..(len / 2).max(0) as usize]),
            "",
            "the injected connection names no catalog, and the refused set \
             must not have changed that"
        );

        cleanup_injected_conn(env, conn);
    }
}

// ---------------------------------------------------------------------------
// Backend-reported fractional truncation (01S07)
// ---------------------------------------------------------------------------

/// Trino's temporal types reach twelve fractional digits and the client
/// advertises `PARAMETRIC_DATETIME`, so `timestamp(12)` arrives with all twelve
/// while `ColumnValue::Timestamp` carries nine. The three that fall off are
/// dropped inside this driver's own conversion, which core cannot observe, so
/// the driver reports them through `StatementBackend::take_value_warning` and
/// the read answers `SQL_SUCCESS_WITH_INFO` with `01S07`.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_beyond_nanoseconds_reports_fractional_truncation() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT CAST(TIMESTAMP '2020-01-02 03:04:05.123456789012' AS timestamp(12)) AS v"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut wbuf = [0u16; 64];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<TrinoBackend>(
            stmt,
            1,
            CDataType::WChar as i16,
            wbuf.as_mut_ptr().cast(),
            (wbuf.len() * 2) as isize,
            &mut ind,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS_WITH_INFO,
            "the driver dropped three fractional digits, so the read is not a plain success"
        );
        assert_eq!(sqlstate_of(HandleType::Stmt, stmt), "01S07");

        // The value still arrives, at the precision the driver can carry: the
        // warning is not an error channel.
        let chars = (ind / 2).max(0) as usize;
        let text = String::from_utf16_lossy(&wbuf[..chars.min(wbuf.len())]);
        assert!(
            text.contains("05.123456789"),
            "expected nine fractional digits in {text:?}"
        );

        cleanup_stmt(stmt);
    }
}

/// The counterpart: a column declared `timestamp(12)` whose value has nothing
/// past the ninth digit loses nothing, and must not draw the diagnostic. The
/// warning is asked of the value, not of the column's declared scale.
#[test]
#[serial]
#[ignore = "requires Trino at localhost:8443; run ./integration-tests/setup.sh first"]
fn timestamp_within_nanoseconds_reports_no_warning() {
    unsafe {
        let (_env, _conn, stmt) = alloc_stmt();

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT CAST(TIMESTAMP '2020-01-02 03:04:05.123456789000' AS timestamp(12)) AS v"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<TrinoBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut wbuf = [0u16; 64];
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<TrinoBackend>(
                stmt,
                1,
                CDataType::WChar as i16,
                wbuf.as_mut_ptr().cast(),
                (wbuf.len() * 2) as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(sqlstate_of(HandleType::Stmt, stmt), "");

        cleanup_stmt(stmt);
    }
}
