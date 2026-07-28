# Agent Guide

Implementation details for AI agents working on `stackable-odbc-trino`.

This crate is an ODBC driver for [Trino](https://trino.io). It contains **only**
Trino-specific code: the `Backend` and `StatementBackend` implementations,
connection-string parsing, Trino-to-ODBC type conversion, ODBC escape-sequence
translation, and the catalog and metadata functions. Everything generic — handle
management, UTF-16 marshalling, diagnostics, panic safety, and the 73 C ABI
entry points — lives in
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core).

## Quick Reference

| Topic | When to Read |
|-------|-------------|
| [Relationship to core](#relationship-to-stackable-odbc-core) | Deciding where a change belongs |
| [Conventions](#conventions) | Any code change |
| [Backend error mapping](#backend-error-mapping) | Touching an error path |
| [Architecture](#architecture-of-this-crate) | Understanding the module layout |
| [Connection string keys](#connection-string-keys) | Adding or changing a parameter |
| [Testing](#testing) | Writing or running tests |
| [Packaging](#packaging-and-the-power-query-connector) | Cutting a release |

```bash
cargo build                                  # needs unixodbc-dev
cargo test                                   # unit + FFI tests that need no server
cargo clippy --all-targets -- -D warnings
pre-commit run --all-files                   # the gate; run before every commit

./test/setup.sh                              # start Trino (Docker), write ODBC config
./test/run-tests.sh                          # run the integration suite
```

## Relationship to stackable-odbc-core

`stackable-odbc-core` is a path dependency on a sibling checkout until it is
published:

```toml
stackable-odbc-core = { path = "../stackable-odbc-core" }
```

There is a matching `TODO` in `Cargo.toml`. Until it is resolved, CI cannot pass
and `cargo publish` cannot run — a path dependency does not resolve on a runner
and cannot be published. Both clear when the path dep becomes a version dep.

| Concern | Owner |
|---------|-------|
| Handle allocation, tag validation, `panic_safe` | core |
| UTF-16 marshalling, diagnostics, `SQLGetDiagRec` | core |
| The 73 exported C ABI entry points (`forward_ffi!`) | core |
| Generic `SQLGetInfo` defaults, cursor-state tracking | core |
| `Backend` / `StatementBackend` trait definitions | core |
| Connecting to Trino, executing, fetching, cancelling | this crate |
| Trino type → SQL type mapping, value conversion | this crate |
| Catalog and metadata queries | this crate |
| Connection-string parsing | this crate |
| ODBC escape-sequence translation | this crate |

`src/lib.rs` is the whole export surface:

```rust
stackable_odbc_core::forward_ffi!(crate::backend::TrinoBackend);
```

That one line expands to every `#[unsafe(no_mangle)] pub unsafe extern "system"`
entry point. If a new ODBC function needs to be exported, it is added to core's
`forward_ffi!` macro, not here; this crate only implements whatever new trait
method it calls.

## Conventions

- Edition 2024, Rust 1.95.0 (pinned in `rust-toolchain.toml`)
- `snafu` for errors (the `unwrap_used`, `unwrap_in_result` and `panic` clippy
  lints are denied outside tests)
- `tracing` for logging (not `println!` or `log`)
- `odbc-sys` links against `libodbc`/`libodbcinst`, so building or testing needs
  the unixODBC dev libraries installed (`unixodbc-dev` on Debian/Ubuntu). No DSN
  or running Driver Manager is required for `cargo test`.

### Changelog

This project keeps a [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
`CHANGELOG.md` and follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Every user-facing change gets an entry under `## [Unreleased]` in the
appropriate `Added` / `Changed` / `Fixed` / `Removed` group. For a driver,
"user-facing" means anything an ODBC application can observe: a changed
SQLSTATE, a changed `SQLGetInfo` value, a new connection-string key, a different
type mapping.

### Logging in backend methods

Every `Backend` / `StatementBackend` method logs at entry with `tracing::debug!`,
naming the method as the application sees it:

```rust
tracing::debug!(%sql, "TrinoBackend::exec_direct");
```

Rules: never log passwords, tokens, or connection-string content — `ConnectParams`
wraps secrets in `Redacted` for exactly this reason. Use `warn!` for intentional
spec deviations and for degraded behaviour (an unparseable Trino type signature,
say). Do not `error!` for failures already expressed as a returned
`TrinoError` — core logs those at the FFI boundary, and doing both double-logs.

### Named constants

ODBC attribute values, info values, and bitmap constants must use named `const`
definitions — never raw integer literals for spec-defined values. Name them after
the ODBC spec name (`SQL_CB_PRESERVE`, `SQL_TC_NONE`, `SQL_OJ_LEFT`).

**This applies to tests too**, and `src/backend/info.rs`'s `EXPECTED` snapshot
table is where raw literals creep back in most easily, with the spec name
relegated to a trailing comment. A comment is not a constant.

Prefer an `odbc-sys` type over a new constant when one exists — most spec values
are already modelled, and all are re-exported from `stackable_odbc_core::types`:

| Value | Use |
|-------|-----|
| `SQL_BIGINT`, `SQL_VARCHAR`, … | `SqlDataType::EXT_BIG_INT.0` (note the `.0`) |
| `SQL_C_SBIGINT`, `SQL_C_WCHAR`, … | `CDataType::SBigInt as i16` |
| `SQL_PARAM_INPUT`, … | `ParamType::Input as i16` |
| `SQL_ATTR_*` | `StatementAttribute::*` / `ConnectionAttribute::*` |

This crate declares no `odbc-sys` dependency of its own. Core re-exports it as
`stackable_odbc_core::odbc_sys`, and that is the one to reach for when a type is
needed that `types` does not re-export (`odbc_sys::Timestamp`, say). Declaring
`odbc-sys` separately lets cargo resolve a different version, and two versions
of a `#[repr(C)]` type are two different types to the compiler — with two
layouts, for a struct read out of a buffer core wrote.

### Non-exhaustive types from core

`ColumnDescriptor`, `TypeInfoRow`, `EscapeDialect` and
`CatalogResultColumnWidths` are `#[non_exhaustive]`, so struct-literal syntax
does not compile here and `..Default::default()` is not an escape hatch either.
Build them with the constructor plus `with_*` builders:

```rust
ColumnDescriptor::new(name, sql_type)
    .with_type_name(type_name)
    .with_precision_scale(precision, scale)

EscapeDialect::ansi_default().with_identifier_quotes(&[('"', '"')])
```

`TypeInfoRow`'s string-setting builders (`new`, `with_literal_affixes`,
`with_create_params`, `with_local_type_name`) take `impl Into<Cow<'static,
str>>`, and `Into` cannot run in a const context, so those four are not `const`.
The rows therefore live in `info::trino_type_info()`, built once behind a
`OnceLock`, rather than in a `static`. The remaining builders touch no string
field and stay `const`.

Set only what differs from the default — the omitted builders are the row
claiming the least-committal value, which is why all 22 rows leave `nullable`
and `searchable` alone. `nullable` is a `Nullable`, not a raw `i16`, matching
`ColumnDescriptor::nullable`.

### Capability declarations take a connection

The 25 required capability methods, plus `get_type_info` and `escape_dialect`,
take `&Self::Connection`: `SQLGetInfo` is a per-connection call, so what the
data source can do belongs to the connection rather than to the driver binary.
This driver answers all of them without reading it — every value is a fact about
Trino-the-engine or about this driver's own SQL generation — but
`TrinoConnection::server_major` is there for the ones that should eventually
gate on the coordinator's version.

`cursor_commit_behavior`, `cursor_rollback_behavior` and
`catalog_result_column_widths` deliberately keep no connection, because they are
consumed on paths that have none.

Pre-connect, core passes `None` and skips every declaration that needs a
connection, substituting its own benign default — so a value this driver reports
when connected is not necessarily what `SQLGetInfo` returns before
`SQLDriverConnectW`. `info::get_info_snapshot` asserts the connected answers;
`get_info_every_named_info_type_has_the_declared_shape_pre_connect` covers the
other side.

### The catalog functions return rows, not statements

`tables`, `columns`, `primary_keys`, `foreign_keys`, `statistics` and
`special_columns` each return a `Vec` of core's typed row structs (`TableRow`,
`ColumnRow`, …, in `stackable_odbc_core::types`). Core converts them to
`ColumnValue`s in spec column order, sorts them, and serves the result set, so
this crate never builds a `TrinoStatement` for a catalog call and never names a
result-set descriptor. Three consequences, each of which is easy to undo by
accident:

- **No `ORDER BY` in `src/backend/metadata.rs`.** Core sorts every result set
  into its spec order, using `Backend::null_collation` so the sort cannot
  contradict what `SQLGetInfo` reports. A backend-side `ORDER BY` is redundant
  server-side work.
- **No `SQL_ATTR_METADATA_ID` handling.** Core normalises identifier arguments
  before it calls this crate, from `identifier_case` and
  `search_pattern_escape` — both of which this driver already declares. What
  arrives here is always an ordinary pattern.
- **No `SQL_ALL_*` special-casing in `tables`.** Core detects the three
  enumerations on the raw arguments and answers them from `catalogs`,
  `schemas` and `table_types` instead; `tables` is not called at all.

`table_types` is required and returns `["TABLE", "VIEW"]` — the two
`information_schema.tables.table_type` values `metadata::tables` maps, upper
case per the spec. `catalogs` and `schemas` are *defaulted* in the trait but
mandatory here: both `supports_catalogs` and `supports_schemas` answer `true`,
and a backend that claims either and leaves the method defaulted answers
`HYC00` to that enumeration. Both query `system.jdbc.*` rather than
`information_schema`, which is what lets them work before a session catalog is
set — exactly the state an application is in when it asks.

### Cancellation

`Backend::cancel` receives a `TrinoCancelToken`, never a statement: `SQLCancel`
may run on a thread holding no lock on the connection while another thread
executes on the same statement, and a `&mut Self::Statement` cannot exist under
that constraint.

The token carries the client and runtime, captured from the connection when core
builds it, plus a shared `CancelState`. Trino names a query only once the
coordinator accepts it, so `exec_direct` fills the state's `query_id` slot as
soon as the submit returns — before the metadata-polling loop, since a queued
query is exactly when an application reaches for `SQLCancel`.

`CancelState::cancelled` is the return path. A cancelled query cannot be paged
any further: `get_next` fails after a server-side cancel and leaves the pooled
TCP socket carrying residual bytes, which surfaces later as an unrelated query
failing. `fetch` and `close_cursor` read the flag and stop touching `next_uri`.
Core never replaces a statement's token, including across a re-execute, so
`begin_query` clears the flag as well as setting the id.

That flag covers only the cancel that lands *between* requests. A cancel
landing while a page request is in flight is recognised from Trino's own
`USER_CANCELED` code instead, in `map_trino_error` — the flag races here,
because the cancelling thread sets it only after its `DELETE` returns, by which
time the coordinator may already have failed the in-flight request. The server's
verdict needs no cross-thread ordering and also catches a query killed by
something else, such as `CALL system.runtime.kill_query`.

That path yields `TrinoError::OperationCancelled`, which is `HY008` — the
SQLSTATE the spec gives a function interrupted by `SQLCancel` from another
thread, with no `(DM)` annotation, so it is the driver's to report. Core has no
named constructor for it, because core documents `HY008` as never returned by a
driver ("not applicable; the `Backend` trait is synchronous"); cross-thread
`SQLCancel` made that false, so this driver builds it with `SqlState::new`.
`end_page_fetch` keeps that case off `abandon_result_set`: a cancellation the
application asked for is a finished result set, not the undefined cursor
position `24000` describes.

The six catalog functions, and the `catalogs` / `schemas` enumerations, take
the token but record nothing in it, so `SQLCancel` cannot interrupt them. Four
do no I/O at all; the other four go through `query_all_rows` →
`Client::get_all`, which pages to exhaustion inside the client and never
surfaces a query id.

### Type cast safety

Use `T::try_from(x)` over a bare `as T` wherever truncation is possible. Trino
returns 64-bit precision and scale values that ODBC exposes as 32- and 16-bit,
so `src/backend/execute.rs` and `src/backend/metadata.rs` are full of legitimate
narrowing — do it fallibly, with a `warn!` on the fallback path.

### Backend error mapping

Every error originating from `trino-rust-client` must be routed through
`map_trino_error` (`src/backend.rs`) — never hand-build a `TrinoError` or
`OdbcError` from a client error at the call site. That function is the single
place that decides the SQLSTATE, and bypassing it silently degrades specific
codes to `HY000`. It yields `08S01` for link failures, `HYT00` for timeouts and
`28000` for auth errors.

It also decides what reaches `SQLGetDiagRec` beyond the SQLSTATE. Anything it
does not classify becomes `TrinoError::Query`, which keeps the failure as its
`source` and lifts `QueryError::error_code` into the native error — so a
server-side rejection reaches the application as its own Trino code rather than
`0`. A variant that flattens the failure into a `String` throws both away,
which is why the specific arms are the exception and not the pattern.

The `source` is a [`QueryCause`], not the client error itself, and `query_cause`
is the one place that decides which. A transport error is kept whole; its
`Display` is a single line. A server-side `QueryError` is reduced to
`[error_name]: message`, because its own `Display` renders `failure_info` — the
coordinator's Java stack — and core walks the whole causal chain into the
diagnostic. Measured against a live coordinator that put 1,700–15,000 characters
into every message, `DIVISION_BY_ZERO` being the worst at roughly 30 KB of
UTF-16 across ~168 frames.

Nothing actionable is lost: the stack describes the coordinator's internals, the
application already gets Trino's error code verbatim through `NativeErrorPtr`,
and the summary naming the failure is what led the message anyway. The full
`failure_info` is logged at `debug` instead, which is what `ODBC_LOG_LEVEL` /
`ODBC_LOG_FILE` exist for.

Two of those arms are load-bearing beyond diagnostics: `validate_connection`
matches on `AuthFailure` and `QueryTimeout` to keep them at their own SQLSTATE
instead of reclassifying them as `08001`. Collapsing the classified variants
into one would move that silently.

Two shapes occur:

```rust
// Transport errors (trino_rust_client::error::Error) — map directly.
conn.runtime.block_on(conn.client.get::<Row>(sql)).map_err(map_trino_error)?

// Server-side query errors (QueryError on `page.error`) — convert first.
// `From<QueryError> for Error` routes Trino error code 4 (PERMISSION_DENIED)
// to `Error::Forbidden`, which is what produces 28000.
if let Some(error) = page.error.take() {
    return Err(map_trino_error(error.into()));
}
```

Every `Backend` and `StatementBackend` method returns `Self::Error`, which is
`TrinoError` for both, so there is no second error type to convert to at a call
site: `.map_err(map_trino_error)?` is the whole idiom.

Hand-built errors are correct only for *internal* invariant violations that never
came from the client ("get_data called before fetch", a missing runtime handle, a
poisoned mutex), and for connection-setup failures where the call-site context
("failed to build Trino client") is more useful than the mapped variant.

Build those as an `OdbcError` and convert with `.into()` when they need a
SQLSTATE no `TrinoError` variant carries — `24000` on an abandoned result set,
say. `TrinoError::Odbc` holds it and the reverse conversion unwraps it, so the
SQLSTATE and message survive intact rather than being remapped to `HY000`. That
variant is also what `From<OdbcError> for TrinoError` produces, which is the
bound core requires so a defaulted trait body can construct an error and still
name `Self::Error`.

### 08001 versus 08S01

`08001` ("client unable to establish connection") is only valid from the
connection functions. Once a connection exists, a failing link is `08S01`
("communication link failure") — that is the code the diagnostics tables of
`SQLExecute`, `SQLFetch`, `SQLGetInfo` and the rest actually list.

This driver's `connect` performs no network I/O — it only builds the HTTP client
— so every failure `map_trino_error` sees is post-connection, and maps to
`08S01`.

### Transactions

The driver reports `SQL_TC_NONE` and its `end_tran` is a no-op. This is a driver
limitation, not a Trino one: Trino supports `START TRANSACTION` / `COMMIT` /
`ROLLBACK` over the `X-Trino-Transaction-Id` headers, and `trino-rust-client`
models them. Because no transaction ever begins, `Backend::cursor_commit_behavior`
is correctly left at core's `CursorBehavior::Preserve` default, which is what
`SQL_CURSOR_COMMIT_BEHAVIOR` reports, and `default_txn_isolation` /
`txn_isolation_options` are both `0` — the spec's value for a data source
without transactions, which also makes core reject every
`SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` with `HY024`. Implementing
transactions means revisiting all of those together; see
`TrinoBackend::end_tran` for the full list.

## Architecture of this crate

| Module | What it does |
|--------|--------------|
| `src/lib.rs` | Module wiring and the `forward_ffi!` invocation. The entire export surface. |
| `src/backend.rs` | `TrinoBackend`, `TrinoConnection`, `TrinoStatement`; the `Backend` impl; `map_trino_error` |
| `src/backend/execute.rs` | `exec_direct`, `execute`, paging, `StatementBackend` (fetch, `column_count`, `describe_col`, `close_cursor`) |
| `src/backend/info.rs` | `SQLGetInfo` answers — the largest module. Typed `get_info` plus the raw `get_info_raw` path for info types with no `InfoType` variant |
| `src/backend/metadata.rs` | Catalog functions: tables, columns, primary keys, foreign keys, statistics, special columns, and the catalog / schema / table-type enumerations. Each returns typed rows; core builds and sorts the result set |
| `src/backend/params.rs` | Parameter interpolation. Trino has no wire-level parameter binding, so bound values are rendered into the SQL as literals — the escaping rules live here |
| `src/backend/types/connect_params.rs` | Connection-string parsing, with `Redacted` secrets |
| `src/escape_dialect.rs` | ODBC escape sequences (`{fn ...}`, `{d ...}`, `{oj ...}`) → Trino SQL |
| `src/type_conversion.rs` | Trino type signatures → `SqlDataType`, and Trino values → `ColumnValue` |
| `src/ffi_integration_tests.rs` | Tests that drive the real C ABI entry points |

### The Tokio bridge

Core's `Backend` trait is synchronous, but `trino-rust-client` is async. Each
`TrinoConnection` therefore owns a current-thread Tokio runtime and every call
into the client goes through `conn.runtime.block_on(...)`. Never introduce a
second runtime, and never `block_on` from inside an async context.

### Paging

Trino's REST protocol returns results as a chain of pages linked by `nextUri`.
`exec_direct` polls until the first page carrying column metadata arrives — a
query can return several empty pages first — and stores the descriptors on the
statement. This matters for correctness beyond fetching: core infers cursor
state from `StatementBackend::column_count`, which therefore must be accurate as
soon as `execute` / `exec_direct` returns, not merely after the first `fetch`.

## Connection string keys

Parsed in `src/backend/types/connect_params.rs`, which is the authoritative
list — keep this table in sync with it. Keys are case-insensitive.

| Key | Required | Meaning |
|-----|----------|---------|
| `Host` | Yes | Trino coordinator hostname |
| `Port` | Yes | Coordinator port |
| `User` | Yes | Username (Basic Auth) |
| `Password` | No | Password (Basic Auth) |
| `Protocol` | No | `http` (default) or `https` |
| `Catalog` | No | Default catalog |
| `Schema` | No | Default schema |
| `TlsVerify` | No | `true` (default) or `false` |
| `Certificate` | No | Path to a PEM CA certificate for server verification |
| `AccessToken` | No | JWT bearer token. Alias: `Token` |
| `QueryTimeout` | No | Per-request HTTP timeout in seconds (default 30). Alias: `LoginTimeout` |

## Testing

### Unit and FFI tests

```bash
cargo test          # must produce zero warnings
```

Backend tests exercise the `Backend` impl directly.
`src/ffi_integration_tests.rs` drives the real C ABI entry points, which is the
right place for the array-fetch and batch-parameter paths
(`SQL_ATTR_ROW_ARRAY_SIZE`, `SQL_ATTR_ROWS_FETCHED_PTR`,
`SQL_ATTR_PARAMSET_SIZE`): those require direct calls with pre-allocated
column and parameter buffers, which Rust handles cleanly and Python does not.

Tests needing a live Trino are `#[ignore]`d, so a bare `cargo test` stays
self-contained.

Core's `conformance` and `test_support` modules are behind its default-off
`test-support` feature, enabled by the `[dev-dependencies]` entry on
`stackable-odbc-core` so it never reaches the shipped `cdylib`. `conformance`
supplies the `SQLGetInfo` return-shape checks; `test_support` supplies
`attach_connection` / `detach_connection`, which put a network-free
`TrinoConnection` into a connection handle so the *connected* `SQLGetInfo` path
can be tested offline. Core's `handles` module is `pub(crate)`, so these are the
supported way to do that — do not look for a way to reach the handle directly.

`SQLFreeHandle` refuses a connection handle that still holds a connection
(`HY010`), so such a test must `detach_connection` before freeing, which is what
`cleanup_injected_conn` does. Calling `SQLDisconnect` instead would invoke
`TrinoBackend::disconnect` on a connection that never opened a session.

The FFI tests that do need a server share one ODBC connection (`OnceLock`) and
are `#[serial]`. The backend tests use a separate `TrinoConnection` and must run
in isolation:

```bash
cargo test -- --ignored backend
```

Do **not** run those alongside the FFI tests — two independent reqwest
connection pools hitting the same coordinator cause intermittent TCP socket
corruption.

### Capturing suite output

`uv run` in this environment fails when its stdout is a **regular file**: the
process exits 120 and the file is left empty. Piping is unaffected, so capture
output with `tee`, never with `>`:

```bash
uv run --with pyodbc python3 test/test_sql_surface.py "$CONN" 2>&1 | tee run.log   # good
uv run --with pyodbc python3 test/test_sql_surface.py "$CONN" > run.log 2>&1       # loses everything
```

Reproduced with `uv run --with pyodbc python3 -c "print('x')"` alone, so it is
neither the driver nor any suite (uv 0.11.21). `test/test_c_abi.py` needs no
`uv` — standard library only — and redirects fine.

This is worth knowing because the failure looks like a hang: the run completes,
the output vanishes, and the only evidence left is a non-zero exit.

### SQL surface pen test

```bash
uv run --with pyodbc python3 test/test_sql_surface.py "<connection-string>"
```

Walks the SQL a BI tool emits — join shapes, aggregates and the `GROUP BY`
extensions, window functions, subqueries and CTEs, set operations, parameters
in every clause that accepts one, the ODBC catalog functions, and the statement
forms whose result columns carry no declared length.

That last group is the one worth keeping: `DESCRIBE`, `SHOW` and `EXPLAIN`
return unbounded `varchar` columns, so the driver has to describe a column
whose size it cannot know, and an application sizes its buffers from what it
says.

### Raw C ABI pen test

```bash
python3 test/test_c_abi.py    # needs a running Trino; standard library only
```

`test/test_c_abi.py` loads the `.so` with `ctypes` and calls the exported entry
points **with no Driver Manager in the loop**. unixODBC answers a large part of
the ODBC state machine itself, so the driver's own handling of out-of-order and
malformed calls is invisible to the pyodbc and `isql` suites. This is the only
place it is exercised.

That also means the spec's **(DM)** diagnostics must not be expected here:
nothing produces them, so a probe demanding one would assert the absence of a
component rather than the presence of a behaviour. `SQLExecDirect` answering
`HY010` rather than `08003` on an unconnected connection is correct for this
reason, not a defect.

Output is `PASS` / `FAIL` / `NOTE`. A `NOTE` is an observation the driver is
entitled to make either way, not a gap — the one currently emitted records that
a statement can be allocated before connecting, because `SQLAllocHandle`'s
`08003` for that case is Driver-Manager-owned.

A `NOTE` may also be marked `KNOWN`, for a gap diagnosed and recorded rather
than asserted so the suite stays green until the owning crate changes. There are
none at present: the two that named core's `SQLFreeStmt` diagnostic and
`SQLFreeHandle` clearing behaviour were fixed in core and are now assertions.
Tighten a `KNOWN` into a `check` as soon as its fix lands, or it becomes a
permanent blind spot.

### Type-transform fuzz

```bash
python3 test/test_type_matrix.py    # needs a running Trino; standard library only
```

Drives every (Trino value, C data type) pair through `SQLGetData` — 37 values
against 13 C types, plus 14 NULLs against all 13 — and checks the result
against invariants rather than a transcribed copy of the ODBC conversion
matrix. Transcribing the matrix would mostly test the transcription; these are
the properties whose violation is an actual defect:

1. The call returns — no pair may crash or hang.
2. A failure carries a SQLSTATE. `SQL_ERROR` with no diagnostic record leaves
   an application with an error it cannot interpret.
3. NULL is reported as `SQL_NULL_DATA`, for every target type.
4. A value that does not fit reports `22003`, not a truncated number.
5. Text that is not a number reports `22018`, not a zero.
6. A successful text conversion round-trips.

Also covers the integer boundary values, the IEEE specials per float type, and
the statement terminator and comment placements.

A Trino `BOOLEAN` reads back as `"1"`/`"0"`, not `"true"`/`"false"`: it is
described as `SQL_BIT`, and that is what the conversion matrix renders. Do not
"fix" that expectation.

### Integration tests

Requires Docker and docker-compose. **This suite does not run in CI** — the
Trino + Postgres compose stack exceeds a standard GitHub runner, as recorded in
the TODO at the top of `.github/workflows/build.yaml`. Run it locally before a
release.

```bash
./test/setup.sh        # spin up Trino, build the driver, write ODBC config (~60s first run)
./test/run-tests.sh    # run Linux tests, then tear Trino down
```

`--skip-build` skips the cargo build; `--skip-delete` leaves Trino running.
Expected output is `XX passed, 0 failed` (pyodbc, once per config) then the same
for the FFI suite. What matters is `0 failed` — the totals move whenever tests
are added, so do not treat them as fixed.

The test instance has two catalogs: `tpcds` (TPC-DS benchmark data, read-only,
no constraints) and `postgresql` (PostgreSQL, whose test schema in
`test/postgres-init.sql` provides primary keys, foreign keys, indexes and the
ODBC-relevant column types).

For interactive testing with `isql`, after `setup.sh`:

```bash
export ODBCSYSINI=$(pwd)/test
export ODBCINI=$(pwd)/test/odbc.ini
isql -3 trino_http -v

# DSNs in test/odbc.ini: trino_http, trino_https,
# trino_https_verify_false (TlsVerify=false), trino_postgresql

docker compose -f test/docker-compose.yml logs -f    # watch incoming requests
```

### Windows VM tests

The same suites, driven through the Windows ODBC Driver Manager over WinRM. See
[`windows/WINDOWS.md`](windows/WINDOWS.md) for VM creation and the full
reference.

```bash
./test/run-tests.sh --windows                          # Linux + Windows
uv run --with pywinrm python3 test/windows_test.py     # Windows only; Trino must be up
```

### Benchmarks

A Criterion fetch-throughput benchmark lives in `benches/fetch_trino.rs`. It
needs a running Trino and a URL:

```bash
TRINO_BENCH_URL=http://localhost:8080 cargo bench
```

`BENCH_ROWS` and `TRINO_BENCH_QUERY` override the row count and query.

### What runs in core, not here

Miri and cargo-fuzz. Miri cannot execute the OpenSSL and aws-lc-rs code
`trino-rust-client` links in, and both fuzz targets exercise core APIs. Core is
pure Rust and holds all the raw-pointer marshalling, so that is where the
undefined-behaviour risk lives and where both are run.

## Packaging and the Power Query connector

`packaging/build-archives.sh` assembles three release artefacts into
`packaging/dist/`, given `$VERSION` and both release binaries:

- `stackable-odbc-trino-<version>-linux-x64.tar.gz` — `.so` + install scripts
- `stackable-odbc-trino-<version>-windows-x64.zip` — `.dll` + `.mez` + `.bat` scripts
- `StackableTrinoODBC-<version>.mez` — standalone Power BI asset

`connector/` holds the Power Query custom connector source; `connector/build.sh`
zips it into the `.mez`. Note that `connector/StackableTrinoODBC.pq` carries its
own `[Version = "..."]`, which Power BI reads — it is not the Cargo version and
does not track it.

### Cutting a release

```bash
release/release.sh minor            # dry run — cargo-release is dry-run by default
release/release.sh minor --execute
```

That bumps `Cargo.toml`, rewrites `CHANGELOG.md` and the version examples in
`packaging/README.md`, commits, and pushes a signed `v<version>` tag. The tag
triggers `.github/workflows/release.yaml`, which builds both binaries, runs
`build-archives.sh` and publishes the GitHub Release. `release.toml` restricts
this to `main`.

Publishing to crates.io is disabled (`publish = false`) and blocked anyway while
`stackable-odbc-core` is a path dependency.
