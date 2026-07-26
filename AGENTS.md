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

In functions returning `OdbcError`, wrap it:
`.map_err(|e| OdbcError::from(map_trino_error(e)))?`.

Hand-built errors are correct only for *internal* invariant violations that never
came from the client ("get_data called before fetch", a missing runtime handle, a
poisoned mutex), and for connection-setup failures where the call-site context
("failed to build Trino client") is more useful than the mapped variant.

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
| `src/backend/metadata.rs` | Catalog functions: tables, columns, primary keys, foreign keys, statistics, type info |
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

The FFI tests that do need a server share one ODBC connection (`OnceLock`) and
are `#[serial]`. The backend tests use a separate `TrinoConnection` and must run
in isolation:

```bash
cargo test -- --ignored backend
```

Do **not** run those alongside the FFI tests — two independent reqwest
connection pools hitting the same coordinator cause intermittent TCP socket
corruption.

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
