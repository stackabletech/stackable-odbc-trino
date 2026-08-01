# Agent Guide

Implementation details for AI agents working on `stackable-odbc-trino`.

This crate is an ODBC driver for [Trino](https://trino.io). It contains **only**
Trino-specific code: the `Backend` and `StatementBackend` implementations,
connection-string parsing, Trino-to-ODBC type conversion, ODBC escape-sequence
translation, and the catalog and metadata functions. Everything generic — handle
management, UTF-16 marshalling, diagnostics, panic safety, and the exported C ABI
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

./integration-tests/setup.sh                 # start the stack (Docker), write ODBC config
./integration-tests/run-tests.sh             # run the integration suite
./integration-tests/setup.sh --profile all   # plus keycloak and minio
./integration-tests/run-tests.sh --suite tls # one suite by name
./integration-tests/scripts/teardown.sh      # stop the stack
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
| The exported C ABI entry points (`forward_ffi!`) | core |
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
the ODBC spec name (`SQL_CB_CLOSE`, `SQL_TC_DML`, `SQL_OJ_LEFT`).

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

`ColumnDescriptor`, `TypeInfoRow`, `EscapeDialect`,
`CatalogResultColumnWidths` and the ten catalog row types (`TableRow`,
`ColumnRow`, `TablePrivilegeRow`, …) are `#[non_exhaustive]`, so struct-literal
syntax does not compile here and `..Default::default()` is not an escape hatch
either. Build them with the constructor plus `with_*` builders:

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

The catalog row types are the exception to the `with_*` naming: their setters
are named after their fields, because core generates them from the field list
with a `macro_rules!` that cannot build an identifier from parts. Each takes
`impl Into<T>`, so an `Option<String>` column accepts a bare `String` *or* the
`Option`, and a `String` column accepts a `&str`:

```rust
TableRow::default()
    .catalog(cat_val.as_str().map(str::to_string))   // Option<String>
    .name(name)                                       // String
    .table_type(odbc_type.to_string())
```

Adding a column to a spec result set is a core-only change that generates one
more setter, so leave the columns the data source cannot answer unset rather
than spelling out a `None` for each.

### Capability declarations take a connection

The 33 required capability methods, plus `get_type_info` and `escape_dialect`,
take `&Self::Connection`: `SQLGetInfo` is a per-connection call, so what the
data source can do belongs to the connection rather than to the driver binary.
This driver answers all but one of them without reading it — every value is a
fact about Trino-the-engine or about this driver's own SQL generation — and
`TrinoConnection::server_major` is there for the ones that should eventually
gate on the coordinator's version. `dbms_version` is the exception, and reads
`TrinoConnection::dbms_version`.

`cursor_commit_behavior`, `cursor_rollback_behavior`,
`catalog_result_column_widths`, `driver_name` and `driver_version` deliberately
keep no connection. The first three are consumed on paths that have none; the
last two describe the driver rather than the data source, and the Windows
Driver Manager asks for them before `SQLDriverConnectW`. Declaring those two is
what lets core answer the whole pre-connect identity group itself, so this
driver's `get_info_pre_connect` no longer overrides anything for the DM's
benefit.

**Nothing that a capability method declares may also have an arm in
`backend/info.rs`.** An arm there wins for `SQLGetInfo` while the method keeps
driving `SQLGetConnectAttr` and the `HY024` validation in
`sql_set_connect_attr`, so the two can disagree for one connection. The list of
info types this applies to is in the `_ => {}` comment at the end of
`trino_get_info`'s match; ten joined it when core made them required.

Pre-connect, core passes `None` and skips every declaration that needs a
connection, substituting its own benign default — so a value this driver reports
when connected is not necessarily what `SQLGetInfo` returns before
`SQLDriverConnectW`. `info::get_info_snapshot` asserts the connected answers;
`get_info_every_named_info_type_has_the_declared_shape_pre_connect` covers the
other side.

### Why the catalog cannot be set

`TrinoBackend::current_catalog` reports the catalog the **session** is on, read
from `Client::session_snapshot`, and core feeds it to both readers the spec
makes synonyms — `SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` and
`SQLGetInfo(SQL_DATABASE_NAME)`. That is the one place the value lives; neither
has an arm in `info.rs`, and adding one would let the two disagree. The
`Catalog` connection-string value is the fallback, for the window before any
response has been seen.

The session is the source because `USE postgresql.public` moves the
coordinator's catalog and reports it back in `X-Trino-Set-Catalog`, which the
client tracks. Measured against the live stack: `SQL_DATABASE_NAME` is `tpcds`
before, `postgresql` after, and an unqualified `SELECT count(*) FROM customers`
then resolves in `postgresql.public`. Reporting the connection-string value
there would name a catalog the session had left, while the application's own
unqualified names resolved somewhere else.
`backend_current_catalog_follows_a_use_statement` pins it; the snapshot takes a
read lock and performs no I/O, so a pool reading the attribute on every
checkout pays nothing for it.

`Backend::set_current_catalog` is deliberately **not** implemented, so
`SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` reports core's defaulted `HYC00`.
Trino cannot switch a catalog without also switching the schema, and it is the
second half that makes accepting the call a lie. Measured against a live
coordinator:

| Statement | Result |
|---|---|
| `USE postgresql.public` | `X-Trino-Set-Catalog: postgresql`, `X-Trino-Set-Schema: public` |
| `USE postgresql` | `NOT_FOUND` — parsed as a *schema* named `postgresql` |

`USE` is the only statement that moves the session catalog and its grammar
requires a schema, so honouring "set the catalog to X" means inventing one.
Every catalog has an `information_schema`, so the invention would succeed and
then leave an unqualified `SELECT ... FROM orders` resolving inside it — the
application's names pointing somewhere it never asked for, which is the failure
`HYC00` exists to avoid, displaced from the catalog to the schema. Reconnecting
under a new catalog is worse: it drops the session's prepared statements and
its connection pool under a call the application thinks is an attribute write.

`trino-rust-client` is not the constraint and would need no change — it already
tracks `X-Trino-Set-Catalog` into its session and carries the new value on later
requests. Trino's grammar is. Revisit if `SET SESSION CATALOG`, or any
catalog-only form of `USE`, ever lands.

This is a behaviour change from the driver's previous store-and-succeed, so if
a tool turns out to set the attribute during connection setup it will now see
`SQL_ERROR`. Neither unixODBC nor the Power Query connector does — the connector
passes `Catalog` in the connection string — and all four
`integration-tests/run-tests.sh` configurations (DSN and DSN-less, verified and
unverified TLS) plus the Windows Driver Manager suite connect unaffected.

### The three identity strings

`SQL_DATA_SOURCE_NAME`, `SQL_SERVER_NAME` and `SQL_USER_NAME` are answered from
arms in `backend/info.rs`, reading fields `connect` fills in. Core answers each
with the empty string and says why: "the DM supplies the DSN; core has none",
and the other two are "carried in the connection string, not known here". Both
reasons are true of core and false of this driver.

Only `SQL_DATA_SOURCE_NAME` has a spec-defined empty answer, and only "if the
connection string did not contain the `DSN` keyword". The other two have no such
clause, so an empty answer is a non-answer — which is what an application
rendering "connected as" was getting.

| Value | Source |
|---|---|
| `SQL_DATA_SOURCE_NAME` | `ConnectParams::dsn()`, empty when the application connected by driver |
| `SQL_SERVER_NAME` | the `Host` connection-string value |
| `SQL_USER_NAME` | Trino's `current_user`, read at connect |

**`SQL_USER_NAME` is probed, not taken from `User`.** The spec defines it as
"the name used in a particular database, which can be different from the login
name", and here it does. Under `ExternalAuthentication` there is no `User` at
all, since `connect` calls `ClientBuilder::without_user`, and the coordinator
derives the identity from the token: the connection string names nobody while
the session runs as somebody. `SessionUser` is the other direction, but only
where the deployment grants impersonation. Against the test stack it is refused
at connect with `Access Denied: User admin cannot impersonate user analyst`,
which is the same rule `suites/test_oauth.py` measures for a disagreeing `User`,
so **`SessionUser` cannot be demonstrated here** and the `ExternalAuthentication`
case is the one that carries the argument.

`session_user_name` orders the fallbacks for a failed probe: `SessionUser`,
then `User`, then empty. `SessionUser` comes first because a connection that
carried one and still succeeded is one whose impersonation Trino permitted. It
is pinned by unit test.

It costs no round trip: `connect` already asked `SELECT version()` for
`SQL_DBMS_VER`, and `probe_session` widened that to
`SELECT version(), current_user`. An unparseable version does not discard the
user, which is why the two are read independently rather than through a shared
early return.

Unlike the catalog, this is a **connect-time capture rather than a
`Client::session_snapshot` read**: Trino has no set-user response header and no
statement that moves the identity a session runs as, so there is nothing for a
snapshot to follow.

Arms rather than capability declarations, because none of the three is a fact
about Trino-the-engine — each is a property of the one connection, the way
`SQL_DBMS_VER` is. Pre-connect, core passes `None` and its empty default stands.

`disconnected_trino_conn` sets `server_name` and `user_name` to match the
`ClientBuilder` it fabricates, so `get_info_snapshot` asserts real values;
leaving them blank would let both regress to core's non-answer with the
snapshot still green. The DSN path has no unit coverage at all — it arrives
through `SQLDriverConnectW`'s connection string — so it is asserted in
`suites/test_integration.py`, which `run-tests.sh` drives over both a DSN and a
DSN-less configuration.

### The catalog functions return rows, not statements

The ten catalog methods — `tables`, `columns`, `primary_keys`, `foreign_keys`,
`statistics`, `special_columns`, `table_privileges`, `column_privileges`,
`procedures`, `procedure_columns` — each take one of core's sealed query types
(`TablesQuery`, `ColumnsQuery`, …) and return a `Vec` of its typed row structs
(`TableRow`, `ColumnRow`, …), both in `stackable_odbc_core::types`. Core
converts the rows to `ColumnValue`s in spec column order, sorts them, and serves
the result set, so this crate never builds a `TrinoStatement` for a catalog
call and never names a result-set descriptor. Four consequences, each of which
is easy to undo by accident:

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
- **No `TableType` value-list parsing.** Core splits and unquotes it;
  `TablesQuery::table_types` is a `&[String]`, where an empty slice means no
  filter.

**The query object is passed on to `src/backend/metadata.rs`, not unpacked in
the `Backend` impl.** Each type is `#[non_exhaustive]` with crate-private
fields and an accessor per argument, so a filter core adds later reaches this
crate without changing a signature anywhere. Destructuring at the trait impl
and handing `metadata` a positional list would spend that: the argument run it
removes — six `Option<&str>` on `foreign_keys`, where a crossed pk/fk pair
compiles silently — would simply move one call down.

`table_types` is required and returns `["TABLE", "VIEW"]` — the two
`information_schema.tables.table_type` values `metadata::tables` maps, upper
case per the spec. `catalogs` and `schemas` are *defaulted* in the trait but
mandatory here: both `supports_catalogs` and `supports_schemas` answer `true`,
and a backend that claims either and leaves the method defaulted answers
`HYC00` to that enumeration. Both query `system.jdbc.*` rather than
`information_schema`, which is what lets them work before a session catalog is
set — exactly the state an application is in when it asks.

#### What Trino can and cannot answer

Six of the ten return no rows, and the reason differs by group. Each is stated
explicitly rather than left to the trait default, so the reason is recorded
beside the answer and the call is logged like every other backend method.

| Method | Source | Why |
|--------|--------|-----|
| `tables`, `columns` | `information_schema` | Real data. |
| `catalogs`, `schemas` | `system.jdbc.*` | Real data, no session catalog needed. |
| `table_privileges` | `information_schema.table_privileges` | Real query; see below. |
| `primary_keys`, `foreign_keys` | — | Trino has no key metadata ([trino#22408]). |
| `statistics`, `special_columns` | — | No cross-connector index metadata, no rowid. |
| `column_privileges` | — | Trino grants on tables, never on columns. |
| `procedures`, `procedure_columns` | — | See below. |

**`table_privileges` queries, and gets nothing here.** Every catalog has an
`information_schema.table_privileges` whose columns line up with ODBC's, and
Trino's own JDBC driver reads the same table. It is populated from the
connector's permission management, so only connectors that implement it — Hive
and Iceberg under `sql-standard` security — return rows. A connector without it
answers zero rows rather than an error, which is why the driver queries
unconditionally instead of gating on the catalog.

Both catalogs in the test stack are in that group: `GRANT` on either answers
`NOT_SUPPORTED: Catalog does not support permission management`. **Adding
`GRANT` statements to `integration-tests/stack/postgres/init.sql` would not change this** — Trino
synthesises its own `information_schema` rather than passing it through, and
the base JDBC connector implements no permission management, so a grant made
directly in PostgreSQL is visible in PostgreSQL's `information_schema.table_privileges`
and not in the `postgresql` catalog's. Verified against the running stack; do
not retry it. Getting a non-empty row would mean adding a Hive or Iceberg
catalog with `sql-standard` security and a metastore, which the compose stack
already cannot afford (see the TODO in `.github/workflows/build.yaml`).

That leaves the row conversion untestable end to end, so it lives in
`metadata::table_privilege_row` — a pure function with unit tests feeding it
the rows such a coordinator would return. The integration tests assert what
they can: that the query is accepted and the result set is described.

**`procedures` publishes nothing to read.** Trino has callable procedures —
`CALL system.runtime.kill_query(...)` is one, and an unregistered name answers
`PROCEDURE_NOT_FOUND` — but no metadata names them. `system.jdbc.procedures`
and `system.jdbc.procedure_columns` exist for JDBC compatibility and are
hardwired empty, and `system.metadata` has no procedures table. This is
consistent with the `SQL_ACCESSIBLE_PROCEDURES` = `"N"` already reported from
`info`.

[trino#22408]: https://github.com/trinodb/trino/issues/22408

### Describing parameters

`SQLDescribeParam` is answered from Trino, not guessed: `DESCRIBE INPUT` on a
prepared statement returns a type per parameter. Three things about the path
are load-bearing, and `src/backend/describe_param.rs` documents each at its
site:

- **The `PREPARE` goes through `Client::execute`, never the bound-parameter
  path.** `params::interpolate` would replace the statement's own `?` markers
  with the absent parameter values, registering a statement with no parameters
  — which is exactly what `SQLExecDirect("PREPARE p FROM ... ?")` does today,
  and why `DESCRIBE INPUT` reads empty when driven that way.
- **`PREPARE` and `DEALLOCATE` are not `query_all_rows`.** They declare no
  columns, and `query_all_rows` deserialises rows, so it fails on them.
- **`DEALLOCATE` is not housekeeping.** A session's prepared statements ride on
  every subsequent request as an `X-Trino-Prepared-Statement` header, so a
  leaked entry grows every later request by the whole query text.

`Backend::describe_param` is called once per *parameter* and gets no statement
handle, so the result is cached on `TrinoConnection`, keyed by SQL text. Core
walks a statement's parameters consecutively, so one entry is enough to collapse
n round trips into one. The key is what stops a second statement being answered
from the first one's entry — the failure mode being a *wrong specific type*,
which an application cannot distinguish from a real answer, and which
`describe_param_re_describes_when_the_statement_changes` pins.

Anything unanswerable returns `Ok(None)` and lets core report its documented
`VARCHAR` guess. That is deliberate: Trino declines to prepare plenty of
legitimate statements, and a uniform documented guess beats both a failed call
and an invented type.

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

**A cancelled `fetch` reports `HY008`, never `NoData`.** `NoData` says "your
result set ended", which is false when rows were discarded, and it is not a
cosmetic difference: core relabels a fetch *error* to `HYT00` when its query
timer fired, and has nothing to relabel when the fetch succeeds. While `fetch`
returned `NoData` here, a query timeout whose cancel happened to land between
page requests reached the application as an empty result set with no diagnostic
at all — indistinguishable from an empty table. Rare, because the cancel
usually lands while a request is in flight, which is the other path; caught by
`test_query_timeout_fires_through_the_driver_manager` on roughly one run in
twenty. `cancelled_between_requests` builds that error, and
`cancel_from_another_thread_while_fetching` now requires `HY008` from both
paths rather than accepting either ending.
`begin_query` clears the flag as well as setting the id. Core mints a **new
token at every statement-producing call**, so a re-execute already arrives with
a fresh `CancelState`, and `cancel` sets the flag only after a `DELETE` that
needed an id `begin_query` had recorded — there is no reachable path on which a
cancellation is pending there. The clear is kept because the flag's purpose is
to keep a live query off a stale one's teardown, and the one point that knows a
new query has begun is where that belongs.

`Backend::is_cancelled` reads the same flag, and is what turns it into `HY008`:
`cancel` signals, `is_cancelled` observes, and core discards the backend's own
SQLSTATE when it answers `true`.

That flag covers only the cancel that lands *between* requests. A cancel
landing while a page request is in flight is recognised from Trino's own
`USER_CANCELED` code instead, in `map_trino_error` — the flag races here,
because the cancelling thread sets it only after its `DELETE` returns, by which
time the coordinator may already have failed the in-flight request. The server's
verdict needs no cross-thread ordering and also catches a query killed by
something else, such as `CALL system.runtime.kill_query`. The two are
complementary, which is why implementing `is_cancelled` did not replace the
`OperationCancelled` arm.

#### `Threading = 2` is required, not tuning

`packaging/linux/install.sh` and `integration-tests/setup.sh` both write `Threading = 2`
into the driver's `odbcinst.ini` section. unixODBC's default is `3`, which
serialises at the environment level and holds a cross-thread `SQLCancel` behind
the call it was meant to interrupt. Measured against a live coordinator on a
query that runs ~24s, cancelling after 2s:

| | `Threading = 3` | `Threading = 2` |
|---|---|---|
| `SQLCancel` returns | only after the fetch does | immediately |
| `SQLFetch` raises | `HY010` after 23.9s | `HY008` after 2.0s |

`HY010` after the query completed on its own is the cancel accomplishing
nothing, reported to the application as a sequence error it did not commit.

**`SQL_ATTR_QUERY_TIMEOUT` is not affected by unixODBC's threading policy and
fires under either setting.** Core enforces that deadline from a timer thread
that calls `Backend::cancel` *directly*, inside the `.so`, so it does not cross
unixODBC and no threading policy can serialise it: `HYT00` at 2.0s under both
settings, measured. Do not cite the query timeout as the reason for
`Threading = 2`; the reason is `SQLCancel`.

Neither the Rust FFI tests nor `integration-tests/suites/test_c_abi.py` catch a regression here:
both call the exported entry points directly, with no Driver Manager in the
loop, so unixODBC's threading policy never applies to them. Only the pyodbc and
`isql` paths go through it, which is why
`test_cross_thread_cancel_interrupts_a_running_fetch` lives in
`integration-tests/suites/test_integration.py` and says so in its failure message.

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

### Timeouts, liveness, and the hooks left defaulted

Core offers a defaulted hook for each attribute whose spec row makes it the
data source's job. This driver takes up three and deliberately leaves three:

| Hook | Answer | Why |
|------|--------|-----|
| `set_query_timeout` | `QueryTimeout::CoreCancels` | Trino has no per-statement server-side deadline this driver can set. See below. |
| `connection_dead` | `TrinoConnection::liveness` | A flag the error path sets; never a probe. |
| `is_cancelled` | `CancelState::cancelled` | The observing half of `cancel`; see [Cancellation](#cancellation). |
| `set_access_mode` | defaulted `Ok(())` | Trino has no read-only session mode, and the spec makes the attribute a *hint*: "the driver is not required to prevent such statements from being submitted". Accepting and ignoring misleads nobody. |
| `set_max_rows` | defaulted → `01S02` | Trino can cap a result set only through `LIMIT` in the SQL the application wrote. The spec forbids emulating: "a driver should not emulate SQL_ATTR_MAX_ROWS behavior". |
| `set_max_length` | defaulted → `01S02` | Same. The attribute exists "to reduce network traffic", which truncating after the bytes arrive cannot achieve. |

**Why `CoreCancels` and not `DataSource`.** `SET SESSION query_max_run_time`
would work — `trino-rust-client` tracks `X-Trino-Set-Session`, so it would
stick — and it is rejected on two counts. It is a *session* property, so every
statement on the connection would get the most recently set value, where core's
timer is armed per statement and matches the attribute's real scope. And it
would put a round trip inside `SQLSetStmtAttr`, which applications call freely
and the spec does not expect to block. Core's usual argument for `DataSource` —
that the server stops the work rather than the client abandoning it — does not
bite here, because `Backend::cancel` issues Trino's `DELETE /v1/query/{id}` and
the coordinator does stop.

**The deadline covers the fetch, which is where Trino's time goes.** Trino
answers with column metadata before it has computed a row, so `exec_direct`
returns in milliseconds and every second of a slow query is spent paging inside
`fetch`. Core arms its timer at `SQLFetch` as well as at the
statement-producing calls for exactly this reason — `SQLFetch`'s diagnostics
table carries `HYT00` ("the query timeout period expired before the data source
returned the requested result set") with no `(DM)` marker. `SQLGetData` is
deliberately unarmed on core's side: its table carries `HYT01` and no `HYT00`
row at all.

Asserted end to end in two places, and both are needed. `integration-tests/suites/test_c_abi.py`
proves the driver and core cooperate with no Driver Manager in the loop;
`integration-tests/suites/test_integration.py` proves it survives unixODBC. Each also asserts the
*elapsed time*, because `HYT00` arriving after the query finished on its own is
the timeout not working, reported as though it were. Each then runs a further
query on the same connection: a server-side cancel leaves the pooled socket
carrying residual bytes if anything keeps paging it, and that surfaces later as
an unrelated query failing.

**`SQL_ATTR_CONNECTION_DEAD` is answered from a flag, never a probe.** A
connection pool reads it on every checkout, so a round trip would be paid far
more often than a query runs. `Liveness` is an `Arc<AtomicBool>` shared by the
connection, every statement it produced and its cancel tokens — a `SQLFetch`
that cannot reach the coordinator is the most likely place to learn the link is
gone, and the fact belongs to the connection. Only
`TrinoError::CommunicationLinkFailure` sets it: a timeout, an auth rejection and
a server-side query error all leave the link up, and `SQL_CD_TRUE` asserts the
connection *has been lost*, not that something went wrong.

**`SQL_ATTR_LOGIN_TIMEOUT` and `SQL_ATTR_CONNECTION_TIMEOUT` arrive on
`ConnectParams`**, as dedicated accessors rather than connection-string keys —
they came from `SQLSetConnectAttr`, and `to_connection_string` is what
`SQLDriverConnect` echoes back to the application. `connect` maps them with two
pure functions, `request_timeout` and `login_deadline`, which is where their
`Some(0)` cases are pinned by test:

- `connection_timeout` becomes the HTTP client's per-request timeout,
  overriding the `QueryTimeout` connection-string key when the application set
  one. `Some(0)` is "there is no timeout" and must **not** be read as unset,
  which would silently reimpose the key's 30-second cap.
- `login_timeout` bounds `validate_connection` — the one round trip that
  decides whether `SQLDriverConnect` succeeds — via `query_all_rows_within`.
  It is applied there rather than on the client because the client's timeout
  also bounds every later query, and the two attributes are set separately.
  `Some(0)` is "wait indefinitely", the same as unset.

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

**Prefer `map_trino_error_on(&liveness, e)` wherever a `Liveness` handle is in
scope** — that is every path with a `TrinoConnection`, a `TrinoCancelToken`, or
a `TrinoStatement` (through its `map_client_error` helper). It delegates the
entire classification to `map_trino_error` and only observes the result, so the
"one place decides the SQLSTATE" rule is intact; what it adds is the
connection-level failure reaching `SQL_ATTR_CONNECTION_DEAD`. The bare
`map_trino_error` stays correct where no handle exists, and is what the wrapper
calls.

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

`SQL_ATTR_AUTOCOMMIT` selects manual-commit mode and `SQLEndTran` commits or
rolls back, over Trino's own `START TRANSACTION` / `COMMIT` / `ROLLBACK` and the
`X-Trino-Transaction-Id` header the client tracks.

`SQLSetConnectAttr` records the mode and issues nothing; the transaction opens
at the first statement, from `TrinoConnection::ensure_transaction` in
`exec_direct`. That is narrower than it looks, and the limit is worth stating:
Trino carries the transaction id in a **session** header, so once one is open
every request the client makes joins it, the catalog functions included, and a
failing one aborts the application's transaction. The lazy open decides when the
window opens, never who is inside it.

#### Any statement error aborts the whole transaction

Measured against Trino 483. After any failure — a `NOT_SUPPORTED` one included —
every later statement answers `TRANSACTION_ALREADY_ABORTED`, **`COMMIT`
included**, and the transaction id is left in place. Only `ROLLBACK` recovers
the session and clears it.

So `SQLEndTran(SQL_COMMIT)` on an aborted transaction sends a `ROLLBACK` and
then reports failure, with `25S03`. Both halves matter:

- Reporting success would tell an application its writes landed when they were
  discarded.
- `25S03` rather than `HY000` because `SQLEndTran`'s **Suspended State** section
  names `25S03`, `40001`, `40002` and `HYC00` as the four SQLSTATEs that confirm
  the transaction did not complete. Any other one leaves the Driver Manager
  holding the connection in a suspended state, where only read-only functions
  work until `SQLDisconnect` — and the rollback has just left this connection
  perfectly usable. Core has no named constructor for it, so `sql_state`
  gained `TRANSACTION_ROLLED_BACK`.

**The failure is not always visible where the statement was submitted.** Trino
sends column metadata before it has evaluated a row, so `SELECT 1/0` returns
successfully from `exec_direct` and fails while its pages are read. `TransactionState`
is therefore shared between the connection and every statement it produces, the
way `CancelState` already is, and `TrinoStatement::map_client_error` marks the
abort. `query_all_rows` routes through the same place.

#### A commit closes every open cursor

`cursor_commit_behavior` and `cursor_rollback_behavior` are both
`CursorBehavior::Close` (`SQL_CB_CLOSE`), measured rather than assumed: paging a
result set after its transaction ends answers `GENERIC_INTERNAL_ERROR: Already
finished`. Three controls rule out the alternatives — the same held cursor
resumes across an unrelated statement on the same session, and across no
transaction at all, delivering every remaining row.

That has a sharp edge for `close_cursor`, which drains the remaining pages to
keep the pooled socket clean. After a commit those pages are dead, so the drain
would fail rather than clean anything. `TransactionState` carries an epoch, a
statement records the one it executed under, and `close_cursor` skips the drain
when the connection has moved past it. The connection bumps the epoch *before*
sending the `COMMIT`, so a `close_cursor` racing it cannot slip through.

#### Isolation levels are vetted by the connector, not the parser

`START TRANSACTION ISOLATION LEVEL X` always parses; the failure lands on the
first statement that touches a catalog, as `UNSUPPORTED_ISOLATION_LEVEL`:

| Level | tpcds | postgresql | hive |
|---|---|---|---|
| READ UNCOMMITTED | ok | ok | ok |
| READ COMMITTED | ok | ok | `UNSUPPORTED_ISOLATION_LEVEL` |
| REPEATABLE READ | ok | `UNSUPPORTED_ISOLATION_LEVEL` | same |
| SERIALIZABLE | ok | `UNSUPPORTED_ISOLATION_LEVEL` | same |

One connection can span catalogs that disagree, so `txn_isolation_options`
advertises `SQL_TXN_READ_UNCOMMITTED` alone — the level a bare
`START TRANSACTION` gets, and the only one every catalog accepts. Core then
rejects the rest with `HY024` before they reach the wire, which is a refused
attribute rather than a mysterious failed query. `SQL_TXN_CAPABLE` is
`SQL_TC_DML`: DDL in a transaction is an error on every JDBC-backed catalog,
and understating the hive catalog, where it works, is the safe direction.

#### `SQLEndTran` with nothing open must not reach the coordinator

Trino answers `NOT_IN_TRANSACTION`, while `SQLEndTran`'s page requires
`SQL_SUCCESS` when no transaction is active. `end_tran` therefore returns early
without I/O. `disconnect` rolls back an open transaction rather than leaving it
to Trino's idle timeout.

#### A pooled connection keeps the commit mode it was returned with

**Measured, and it bites.** pyodbc enables ODBC connection pooling by default. A
pooled connection is handed back to the application without the driver being
reconnected — a dozen pyodbc connections produced two `TrinoBackend::connect`
calls and no `disconnect` at all — so it arrives still in whatever commit mode
the previous borrower left. A `CREATE TABLE` on a "fresh" connection then runs
inside that manual-commit transaction, reports success, and is discarded when
the connection is next recycled.

The driver cannot see the reuse: the Driver Manager neither disconnects nor
tells it. The ODBC-sanctioned signal is `SQL_ATTR_RESET_CONNECTION`, which the
Driver Manager sets before returning a connection to the pool and which core
does not implement yet. Until it does, this is a real hazard for a pooling
application that ever turns autocommit off, and `suites/test_transactions.py`
sets `pyodbc.pooling = False` so it measures the driver rather than the pool.

#### Where this is tested

`suites/test_transactions.py` drives the whole contract through unixODBC and
needs no profile, since the hive catalog is in the base stack. The backend tests
in `src/backend.rs` cover the same ground against the `Backend` impl directly
(`cargo test -- --ignored backend`), and
`autocommit_round_trips_and_end_tran_with_nothing_open_succeeds` plus the
`transactions` group in `suites/test_c_abi.py` cover the entry points with no
Driver Manager in the loop.

Every scenario that writes names the `hive` catalog. Two Hive limits shape them,
and both look like test bugs when met cold: two inserts into the same
*unpartitioned* table in one transaction fail (`Inserting into an unpartitioned
table that were added, altered, or inserted into in the same transaction is not
supported`), so the multi-statement case uses two tables; and a table written in
a transaction cannot be read back before the commit, so row counts are taken
from a second connection afterwards.

#### What is not implemented

Trino's `SET SESSION` transaction access mode has no ODBC counterpart here:
`set_access_mode` stays defaulted, since the spec makes `SQL_ATTR_ACCESS_MODE` a
hint. `multiple_active_txn` is `true` — each connection carries its own Trino
session and therefore its own transaction, while one *session* holds at most
one, which is what `NOT_SUPPORTED: Nested transactions not supported` reports.

## Architecture of this crate

| Module | What it does |
|--------|--------------|
| `src/lib.rs` | Module wiring and the `forward_ffi!` invocation. The entire export surface. |
| `src/backend.rs` | `TrinoBackend`, `TrinoConnection`, `TrinoStatement`; the `Backend` impl; `map_trino_error` |
| `src/backend/execute.rs` | `exec_direct`, `execute`, paging, `StatementBackend` (fetch, `column_count`, `describe_col`, `close_cursor`) |
| `src/backend/info.rs` | `SQLGetInfo` answers — the largest module. Typed `get_info` plus the raw `get_info_raw` path for info types with no `InfoType` variant |
| `src/backend/metadata.rs` | All ten catalog functions, plus the catalog / schema / table-type enumerations. Each returns typed rows; core builds and sorts the result set |
| `src/backend/describe_param.rs` | `SQLDescribeParam`, answered from `DESCRIBE INPUT` on a prepared statement, plus the per-connection cache that keeps it to one round trip per statement |
| `src/backend/params.rs` | Parameter interpolation. Trino has no wire-level parameter binding, so bound values are rendered into the SQL as literals — the escaping rules live here |
| `src/backend/prompt.rs` | Presenting an interactive OAuth 2.0 login URL: core's `Prompter` implemented as `BrowserPrompter`, and the adapter to the client's `RedirectHandler`. The only user of the `open` dependency |
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

`README.md` carries the same table for the people who install the driver rather
than work on it, so **a new key means three edits**: the parser, this table and
that one. The two tables list identical keys today.

| Key | Required | Meaning |
|-----|----------|---------|
| `Host` | Yes | Trino coordinator hostname |
| `Port` | Yes | Coordinator port |
| `User` | Yes¹ | Username (Basic Auth). ¹Optional under `ExternalAuthentication`, where the identity provider supplies it |
| `Password` | No | Password (Basic Auth) |
| `Protocol` | No | `https` (default) or `http` |
| `Catalog` | No | Default catalog |
| `Schema` | No | Default schema |
| `Source` | No | Query source Trino records and can route on. Default `stackable-odbc-trino/<version>` |
| `ClientTags` | No | Comma-separated Trino client tags, which select a resource group |
| `TlsVerify` | No | `true`/`full` (default), `ca`, or `false`/`none`. Alias: `SSLVerification` |
| `Certificate` | No | Path to a PEM CA certificate for server verification. Required by `ca` |
| `ClientCertificate` | No | Path to a PEM holding a client certificate chain and its PKCS#8 key, for mutual TLS |
| `AccessToken` | No | JWT bearer token. Alias: `Token` |
| `ExternalAuthentication` | No | `true` selects Trino's interactive OAuth 2.0 flow. Needs `https`, excludes `Password` and `AccessToken`, and is refused under `SQL_DRIVER_NOPROMPT` |
| `ExternalAuthenticationTimeout` | No | Budget for one interactive login, seconds, default 300. Not bounded by `SQL_ATTR_LOGIN_TIMEOUT` — see below |
| `QueryTimeout` | No | Per-request HTTP timeout in seconds (default 30). Alias: `LoginTimeout` |
| `SessionProperties` | No | Trino session properties, `name:value;name2:value2`. Needs `{braces}` — see below |
| `ExtraCredentials` | No | Connector-level credentials, same form. **Secret** — declared in `sensitive_connect_keywords` |
| `ResourceEstimates` | No | Scheduling hints, same form |
| `Path` | No | Default SQL path for resolving unqualified function names |
| `ClientInfo` | No | Free-form client metadata Trino records against the query |
| `TraceToken` | No | Correlation token Trino records against the query |
| `SessionUser` | No | User statements run as, while `User` still authenticates. JDBC's `sessionUser` |
| `Roles` | No | Authorisation role per catalog, `catalog:role;catalog2:ALL`. Needs `{braces}` |
| `TimeZone` | No | IANA session time zone (`Europe/Berlin`). Unset leaves the coordinator's |
| `ExtraHeaders` | No | Extra HTTP headers, same form. **Secret** — declared in `sensitive_connect_keywords` |
| `ClientCapabilities` | No | Comma-separated extra capabilities, on top of `PARAMETRIC_DATETIME` and `PATH` |
| `Proxy` | No | HTTP/HTTPS proxy URL for every request. Credentials in the URL are rejected |
| `ProxyUser` | No | Proxy Basic username. Requires `ProxyPassword` |
| `ProxyPassword` | No | Proxy Basic password. **Secret** — declared in `sensitive_connect_keywords` |
| `Locale` | No | Locale for locale-dependent formatting, sent as `X-Trino-Language` |
| `DisableCompression` | No | `true` or `false` (default) |
| `MaxAttempts` | No | Request retry budget. Unset leaves `trino-rust-client`'s own |
| `Encoding` | No | Trino's spooled query-data encoding: `json`, `json+zstd` or `json+lz4`. Unset returns every row inline. JDBC's `encoding` |

The five `name:value;name2:value2` keys take **JDBC's format verbatim**, so a
value copied out of a JDBC URL transfers unchanged. That format uses `;`, which
is also what separates one ODBC connection-string parameter from the next, so
the value must be wrapped in braces:

```text
SessionProperties={query_max_run_time:10m;example.foo:bar}
```

Unbraced, core's parser ends the value at the first `;` and discards the rest
as an unrecognised parameter — every pair but the first vanishes silently.
`session_properties_unbraced_keep_only_the_first_pair` pins that, so the
requirement is recorded as behaviour and not only here. Only the *first* `:`
splits a pair, so a value may contain one (`s3://bucket/path`, `10:00`).

A malformed pair fails the connection rather than being skipped: a dropped
session property changes how the query runs, and the result computed without it
is plausible enough that nobody would look.

`QueryTimeout` is the *default* for the per-request HTTP timeout, not the last
word: an application that sets `SQL_ATTR_CONNECTION_TIMEOUT` overrides it, and
`SQL_ATTR_LOGIN_TIMEOUT` separately bounds the login round trip. Neither is a
connection-string key — they reach `connect` as `ConnectParams` accessors. See
[Timeouts, liveness, and the hooks left defaulted](#timeouts-liveness-and-the-hooks-left-defaulted).

### TLS

`TlsVerify` has three modes, not two, and takes both vocabularies: `true` and
`full` verify the chain *and* the hostname, `ca` verifies the chain only, and
`false` and `none` verify nothing. `SSLVerification` is an alias, so a value
lifted from a JDBC URL transfers unchanged — and both keys accept both
vocabularies, because there is no sense in which one name owns one set of
words.

Setting both keys is an error unless they resolve to the same mode. They are
one setting, and silently preferring either would leave the other looking
honoured when it was not — for a value whose failure mode is an
unauthenticated connection.

**`ca` requires `Certificate`**, and `connect_params` rejects the combination
before the client sees it. rustls only permits skipping hostname verification
when the trust store is supplied explicitly, which excludes the platform's own
roots — so `ca` without a chain to verify against would trust nothing at all.
The client reports this too; catching it here names the connection-string keys
rather than the builder methods.

`ca` exists for a coordinator reached under a name its certificate does not
carry — an IP, or an internal DNS name. It is a much narrower compromise than
`none`, which is why it gets a quieter `warn!`: the certificate is still
verified, just not bound to a name.

`ClientCertificate` is mutual TLS, and is independent of the two above: either
may be set alone, and both feed one `Ssl`. **One PEM file holding the
certificate chain followed by a PKCS#8 private key** — the client builds
`reqwest` on rustls, which accepts neither PKCS#12 nor JKS, so JDBC's
`SSLKeyStorePath` / `SSLKeyStoreType` have no equivalent and the key is named
for what it actually takes rather than for JDBC parity it cannot deliver.

### Interactive OAuth 2.0

`ExternalAuthentication=true` selects Trino's external-authentication flow: the
coordinator answers with a login URL, a person visits it, and the client polls
for the bearer token. Three things about the path are load-bearing.

**Core decides whether a connect may prompt; this driver decides how.**
`SQLDriverConnect`'s *DriverCompletion* is the spec's control over interaction,
and only core sees it. `TrinoBackend::prompter` declares what the driver
*could* do — `BrowserPrompter`, in `src/backend/prompt.rs`, which logs the URL
and then opens a browser — and core hands it back through
`ConnectParams::prompter` only when the call permits prompting. `connect` reads
it from there and never calls `Backend::prompter` itself: under
`SQL_DRIVER_NOPROMPT` it receives `None` and there is nothing to call, so the
rule cannot be forgotten. `open` is a dependency of this crate and deliberately
not of core.

The log comes before the browser and happens unconditionally, because a Driver
Manager discards the driver's stderr — under `isql`, Power BI or Excel,
`ODBC_LOG_FILE` / `ODBC_LOG_LEVEL` are the only channel that survives. A failed
browser launch is therefore **not** an error: the flow can still be completed
from the logged URL, since the client polls rather than waiting on the handler.

**One login per identity per process.** The client caches the token in the
`Arc<OAuth2State>` behind an `Auth`, so clones share a login and a second
`Auth::new_oauth2` means a second browser. This driver builds a `Client` per
connection, so `OAUTH2_LOGINS` in `src/backend.rs` keys an `Auth` on
`(secure, host, port, user)` and hands out clones. Without it a pool warming
ten connections would open ten browsers. Expiry needs no handling: a stale
token yields a `401` and the client re-runs the flow behind the same `Arc`.

**`SQL_ATTR_LOGIN_TIMEOUT` does not bound the interactive wait**, and one
`warn!` says so when both are set. The flow fires on the first `401`, inside
`validate_connection` — the very round trip `login_deadline` bounds — but
applications set login timeouts assuming a machine round trip, and a tool
defaulting to 15s would abort every login while the user was still typing.
`ExternalAuthenticationTimeout` bounds it instead.

**`User` is optional under `ExternalAuthentication`, and `X-Trino-User` is then
left off entirely.**

Trino settles why in `HttpRequestSessionContextFactory`. The header is
*optional* whenever a request carries an authenticated identity — the user
falls back to the token's, and `"User must be set"` fires only when neither
exists:

```java
String user = trinoUser != null ? trinoUser : authenticatedIdentity.map(Identity::getUser).orElse(null);
assertRequest(user != null, "User must be set");
```

And a header that *disagrees* with the authenticated identity is read as an
impersonation request:

```java
if (!authenticatedIdentity.getUser().equals(originalIdentity.getUser())) {
    accessControl.checkCanImpersonateUser(authenticatedIdentity, originalIdentity.getUser());
}
```

So a `User` that does not match what the identity provider's user-mapping
produces would fail the connection with an impersonation denial — for typing
your own name in the wrong form — or, where the principal holds impersonation
rights, silently run the session as somebody else. Asking the operator to
invent one is therefore not a safe default, which is why `connect` calls
`ClientBuilder::without_user` when none is given.

A `User` supplied *alongside* `ExternalAuthentication` is still honoured: one
that matches the provider's mapping is harmless, and one that does not is the
application deliberately asking for impersonation, which Trino will judge on
its own rules. `SessionUser` is unaffected — naming somebody to run as while
authenticating as yourself is exactly what it is for.

This needed `trino-rust-client` to be able to omit the header at all;
`Session::user` is `Option<String>` and `ClientBuilder::without_user` exists
for this. Building with neither a user nor authentication is the client's
`Error::MissingUser`, so the case Trino would reject with `User must be set`
never reaches the wire.

## Testing

Everything lives under `integration-tests/`, split by kind: `scripts/` is the
bash, `stack/` the docker material, `suites/` the Python, `perf/` the
profiling tooling, `windows/` the VM harness, and `generated/` every produced
artefact. `generated/` is gitignored and safe to delete; `setup.sh` rebuilds
it.

### The stack is HTTPS only

The coordinator serves 8443 and nothing else: `http-server.http.enabled=false`,
and 8080 is neither published nor bound. OAuth 2.0 requires TLS, and this is
closer to a real deployment.

**The driver's `Protocol=http` connection-string value therefore has no
integration coverage.** Its parsing is unit-tested and nothing exercises the
connection. That is an accepted consequence of the above, not an oversight.

`internal-communication.https.required=true` is mandatory rather than tuning:
with no plaintext listener there is no HTTP internal URI, and without it Trino
fails to start with `NullPointerException: internalUri is null`.

### Certificates

`scripts/gen-certs.sh` builds one CA and signs the coordinator, client and
Keycloak leaves from it, into `generated/certs/`.

The truststore is built with **keytool, never `openssl pkcs12 -export
-nokeys`**. openssl writes the certificate into a certBag carrying no Oracle
trusted-certificate attribute, and Java reads the result as "0 entries": a
valid PKCS12 file that is empty as a trust store. It fails silently, and the
only symptoms are a client-certificate handshake dying with `tlsv1 alert
internal error` and 503s fetching internal memory info. `gen-certs.sh` asserts
the truststore holds a `trustedCertEntry`, because the file existing proves
nothing.

**Jetty selects the certificate on SNI, and serves Trino's internal
self-signed `CN=<node.environment>` certificate for anything it cannot match.**
So a name the coordinator's certificate does not carry yields a *different
certificate*, not a hostname mismatch, and connecting by IP address is worse:
TLS sends no SNI for an IP literal, so the fallback is served every time.
Measured:

| SNI sent | certificate served |
|---|---|
| `localhost` | `CN=localhost`, the CA-signed leaf |
| `trino` | `CN=localhost`, since `DNS:trino` is in the SAN |
| `nosuchname.example` | `CN=test`, Trino's internal certificate |
| none, connecting by IP | `CN=test` |

Two consequences. `suites/test_tls.py` cannot assert that `TlsVerify=ca`
ignores the hostname, and records that as a `NOTE` with the two ways out that
were tried and failed. And the Windows VM, which reaches the host by IP, maps
`trino` to the gateway in its own hosts file so SNI is sent and the
verified-TLS configurations stay meaningful.

### Profiles

Compose profiles make the heavier services opt-in. The unprofiled set is the
core stack.

| Profile | Services | Buys |
|---|---|---|
| *(none)* | `postgres`, `trino` | tpcds, postgresql and hive catalogs, HTTPS, PASSWORD and CERTIFICATE auth, transactional writes, a non-empty `SQLTablePrivileges` |
| `oauth` | `keycloak` | The OAuth 2.0 flow, through `suites/test_oauth.py` |
| `spooling` | `minio`, `minio-init` | The spooling protocol, through the `Encoding` key |

```bash
./integration-tests/setup.sh --profile oauth,spooling   # or PROFILES=all
```

Compose profiles select *services*; they cannot vary a mounted file's
contents, and Trino will not start when `config.properties` names an OAuth
issuer or an S3 endpoint that is not running. So `scripts/gen-trino-config.sh`
assembles `generated/trino/` from `stack/trino/` fragments driven by the same
profile list. A value that *changes* between profiles cannot be appended,
because a duplicate key is a Trino startup error; those are `@PLACEHOLDER@`
substitutions, and an unresolved one fails the assembly rather than reaching
Trino as a literal.

A profile change recreates the coordinator. Without that, compose would start
the new service and leave `trino` on its previously assembled config, so
enabling a profile would appear to do nothing.

### The hive catalog, and why it is not behind a profile

The `hive` catalog is in the base stack because it costs no container: a file
metastore on a path the coordinator can write needs neither a metastore service
nor object storage. It is what makes two things testable at all.

**It is the only connector that accepts a write outside autocommit.** Trino's
coordinator refuses the rest with `AUTOCOMMIT_WRITE_CONFLICT: Catalog only
supports writes using autocommit`, raised by
`InMemoryTransactionManager$TransactionMetadata` and gated on the SPI's
`Connector.isSingleStatementWritesOnly()`, whose default body is `iconst_1;
ireturn`:

| Plugin | Overrides it |
|---|---|
| `trino-hive` | yes, from `HiveConfig` (`hive.single-statement-writes`) |
| `trino-base-jdbc`, so postgresql | no, inherits `true` |
| `trino-iceberg` | no, inherits `true` |
| `trino-delta-lake` | no, inherits `true` |
| `trino-memory` | no, inherits `true` |

So a rollback cannot be demonstrated against `postgresql`, and **Iceberg is not
an alternative** — read from the shipped bytecode of Trino 483, not from
documentation. PostgreSQL's own transactionality is irrelevant, because the
coordinator refuses the write before any SQL reaches PostgreSQL.

**`hive.security=sql-standard` is what fills
`information_schema.table_privileges`**, which gives `SQLTablePrivileges` rows
to convert and exercises `metadata::table_privilege_row` end to end.

Two consequences that look like defects when met cold:

- **The warehouse is not a named volume.** Docker mounts one root-owned, and
  the coordinator runs as `trino`, so it could not write it at all. The
  warehouse therefore lives in the container's own writable layer under
  `/tmp/hive-warehouse`, which Trino creates on first use, and recreating the
  container starts from an empty metastore.
- **`CREATE SCHEMA` needs the `admin` role**, so an ordinary connection meets
  `Access Denied: Cannot create schema`. `scripts/seed-hive.sh` creates the
  schema with `X-Trino-Role: hive=ROLE{admin}` on every `setup.sh`, and that is
  the only statement that needs it: once the schema exists and `admin` owns it,
  an ordinary connection creates tables, writes, reads and grants without a
  role. The seed is idempotent, and it drains the statement's `nextUri` because
  Trino runs a statement as the client pages it.

A suite that needs an inactive profile is **skipped, naming the profile that
would enable it**. An unrun suite must never be printable as a passing one.

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
supplies the `SQLGetInfo` return-shape checks and
`info_group_inconsistencies`; `test_support` supplies
`attach_connection` / `detach_connection`, which put a network-free
`TrinoConnection` into a connection handle so the *connected* `SQLGetInfo` path
can be tested offline. Core's `handles` module is `pub(crate)`, so these are the
supported way to do that — do not look for a way to reach the handle directly.

`info_group_inconsistencies` checks the `SQLGetInfo` groups whose members
constrain each other — vendor terminology against `SQL_CATALOG_NAME`,
`SQL_TXN_CAPABLE` against the two isolation declarations — and returns one
message per violation. Core cannot police these at runtime, because
`TrinoBackend::get_info` runs first and is entitled to answer anything, so the
invariants live in the shared harness and each driver runs them against its own
backend. `get_info_groups_that_constrain_each_other_agree` is the call site
here; it is what would catch `txn_capable` reporting `SQL_TC_DML` while
`default_txn_isolation` and `txn_isolation_options` stayed at `0`.

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
uv run --with pyodbc python3 integration-tests/suites/test_sql_surface.py "$CONN" 2>&1 | tee run.log   # good
uv run --with pyodbc python3 integration-tests/suites/test_sql_surface.py "$CONN" > run.log 2>&1       # loses everything
```

Reproduced with `uv run --with pyodbc python3 -c "print('x')"` alone, so it is
neither the driver nor any suite (uv 0.11.21). `integration-tests/suites/test_c_abi.py` needs no
`uv` — standard library only — and redirects fine.

This is worth knowing because the failure looks like a hang: the run completes,
the output vanishes, and the only evidence left is a non-zero exit.

### SQL surface pen test

```bash
uv run --with pyodbc python3 integration-tests/suites/test_sql_surface.py "<connection-string>"
```

Walks the SQL a BI tool emits — join shapes, aggregates and the `GROUP BY`
extensions, window functions, subqueries and CTEs, set operations, parameters
in every clause that accepts one, the ODBC catalog functions, and the statement
forms whose result columns carry no declared length.

That last group is the one worth keeping: `DESCRIBE`, `SHOW` and `EXPLAIN`
return unbounded `varchar` columns, so the driver has to describe a column
whose size it cannot know, and an application sizes its buffers from what it
says.

### Folding contract test

```bash
uv run --with pyodbc python3 integration-tests/suites/test_folding_contract.py "<connection-string>"
```

The Power Query connector's SQL declarations, checked against the driver and
Trino. Nothing else loads the `.pq`: every other suite drives the driver
directly, and the only other check on folding is a human clicking "View Native
Query" in Power BI Desktop, one step at a time. A connector declaration can
therefore drift from what the driver reports or what Trino accepts with nothing
noticing — which is how both of the bugs this test was written to catch got in.

It parses the connector rather than transcribing it, so the two cannot drift:

- **Every `Constant` visitor field name is a driver `TYPE_NAME`.** Power Query
  looks each one up by `typeInfo[TYPE_NAME]` from `SQLGetTypeInfo`, so a name
  matching nothing can never fire — and a dead name hides the absence of the
  live one it should have been. Five were PostgreSQL names inherited from the
  reference connector.
- **Every CAST target is a type Trino has.** `NUMERIC` and `FLOAT` are not.
- **The row-limiting clause the `AstVisitor` builds is run**, including the
  order it concatenates `OFFSET` and `LIMIT` in. Trino's grammar is
  `OFFSET count LIMIT count` and rejects the reverse, so only a fold carrying
  both a skip and a take exposed it.
- **`SupportsDerivedTable` and `SupportsTop`** are checked against what Trino
  actually does.

A `NOTE` lists driver `TYPE_NAME`s with no visitor entry. That is not a
failure — Power Query evaluates such a constant locally instead of folding it —
but nothing in the connector lists the types it does not handle, so the gap is
otherwise invisible.

### Raw C ABI pen test

```bash
python3 integration-tests/suites/test_c_abi.py    # needs a running Trino; standard library only
```

`integration-tests/suites/test_c_abi.py` loads the `.so` with `ctypes` and calls the exported entry
points **with no Driver Manager in the loop**. unixODBC answers a large part of
the ODBC state machine itself, so the driver's own handling of out-of-order and
malformed calls is invisible to the pyodbc and `isql` suites. This is the only
place it is exercised.

It is also the only suite that reaches `SQLTablePrivilegesW` and
`SQLColumnPrivilegesW` at all: pyodbc exposes no `tablePrivileges()` or
`columnPrivileges()` method, so neither the integration nor the SQL-surface
suite can call them. `SQLColumnPrivileges`' `HY009` for a null `TableName` is
probed here for the same reason, and only for that function — it is the one of
the four privilege and procedure functions whose spec page states that
sentence without a **(DM)** marker, so the other three must not report it.

Every entry point called here needs its `argtypes` and `restype` declared in
`load()`. `SQLRETURN` is a 16-bit `SQLSMALLINT`, and an undeclared function
leaves ctypes reading the return register as a 32-bit `int`, where `SQL_ERROR`
arrives as `65535` and every comparison against `-1` silently fails.

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
than asserted so the suite stays green until the owning crate changes. Tighten
a `KNOWN` into a `check` as soon as its fix lands, or it becomes a permanent
blind spot.

None are open. Four have been raised from this suite and all four were fixed
in the crate that owned them, so each is now an assertion rather than a note:

- **Integer statement attributes were written four bytes wide.** Every
  non-pointer attribute on the `SQLSetStmtAttr` page is declared "An SQLULEN
  value" — not one is `SQLUINTEGER` — and `SQLULEN` is 64-bit on a 64-bit
  platform. `BufferLength` is ignored for a non-string value, so an application
  writing `SQLULEN v; SQLGetStmtAttr(s, SQL_ATTR_MAX_ROWS, &v, 0, NULL);` kept
  whatever was on its stack in the top half of `v` and read an enormous row
  limit rather than the `0` core reported. Now checked for six attributes here
  and all nineteen in
  `statement_attributes_are_written_at_the_full_sqlulen_width`.

  **Do not carry the rule across to connection attributes.** Only
  `SQL_ATTR_ASYNC_ENABLE` and `SQL_ATTR_ODBC_CURSORS` are `SQLULEN` there; the
  rest genuinely are `SQLUINTEGER`, and widening those would introduce the
  opposite bug — an eight-byte write into the four an application allocated.

- **`SQL_ATTR_CURRENT_CATALOG` was a write-only handle-local string.** It and
  `SQL_DATABASE_NAME` are one value under two names, but the attribute answered
  `""` where the info type answered the connected catalog, and setting it
  returned `SQL_SUCCESS` while switching nothing. Core grew
  `Backend::current_catalog` and `Backend::set_current_catalog`; this driver
  implements the first and deliberately leaves the second defaulted. See
  [Why the catalog cannot be set](#why-the-catalog-cannot-be-set).

The two that named core's parameter handling were fixed in core and are now
assertions, in the `bound parameter types` group. They are worth keeping in
that shape because each pins a defect an application sees rather than a call's
return code:

- **The declared SQL type survives.** `SQL_C_CHAR` + `SQL_NUMERIC` — what a
  client sends for a numeric delivered as text — reaches Trino as a `decimal`,
  so `WHERE decimal_col = ?` works. It previously arrived as a string and
  failed with `TYPE_MISMATCH: decimal(10,2) = varchar(5)`, an ordinary BI
  filter.
- **An unbound parameter marker is `07002`.** Core previously padded it with
  `NULL` and told the application nothing, which is why
  `SQLExecDirect("PREPARE p FROM ... ?")` registered a statement with no
  parameters — see `backend/describe_param.rs`.

### Type-transform fuzz

```bash
python3 integration-tests/suites/test_type_matrix.py    # needs a running Trino; standard library only
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
./integration-tests/setup.sh        # spin up Trino, build the driver, write ODBC config (~60s first run)
./integration-tests/run-tests.sh    # run Linux tests, then tear Trino down
```

`--skip-build` skips the cargo build; `--skip-delete` leaves Trino running.
Expected output is `XX passed, 0 failed` (pyodbc, once per config) then the same
for the FFI suite. What matters is `0 failed` — the totals move whenever tests
are added, so do not treat them as fixed.

The test instance has two catalogs: `tpcds` (TPC-DS benchmark data, read-only,
no constraints) and `postgresql` (PostgreSQL, whose test schema in
`integration-tests/stack/postgres/init.sql` provides primary keys, foreign keys, indexes and the
ODBC-relevant column types).

For interactive testing with `isql`, after `setup.sh`:

```bash
export ODBCSYSINI=$(pwd)/integration-tests/generated
export ODBCINI=$(pwd)/integration-tests/generated/odbc.ini
isql -3 trino_https -v

# DSNs in integration-tests/generated/odbc.ini: trino_https,
# trino_https_verify_false (TlsVerify=false), trino_postgresql, and
# trino_oauth (ExternalAuthentication, needs the oauth profile; opens a real
# browser, which will warn about the test CA)

docker compose -f integration-tests/stack/compose.yaml logs -f    # watch incoming requests
```

### The OAuth 2.0 flow, in `integration-tests/suites/test_oauth.py`

The `oauth` profile brings up Keycloak, and `suites/test_oauth.py` drives the
whole interactive flow against it. Do not replace any of it with a mock token
endpoint: that would exercise the driver's own plumbing and none of the
coordinator behaviour actually in doubt.

| What | Scenario |
|------|----------|
| The end-to-end flow: `401`, login URL, browser, poll, bearer token | `one_login_serves_many_connections` |
| That three connections on one identity open exactly **one** browser | same scenario, counted from the browser's own launch record |
| That omitting `X-Trino-User` works, and Trino resolves the user from the token | `the_token_supplies_the_user` |
| That a matching `User` is honoured, and a *disagreeing* one is refused | `a_matching_user_is_honoured`, `a_disagreeing_user_is_refused` |
| `28000` for a login the identity provider refuses, and for one nobody completes | `a_refused_login_reports_28000`, `an_abandoned_login_times_out` |
| `ExternalAuthenticationTimeout` firing, measured against the elapsed time | `an_abandoned_login_times_out` |
| Both sides of the *DriverCompletion* gate, including through unixODBC | `noprompt_is_refused`, `the_driver_manager_forwards_the_completion` |

**A `User` disagreeing with the token is refused, not ignored.** Trino's default
system access control denies `checkCanImpersonateUser`, so the connection fails
with `28000` and `Access Denied: User admin cannot impersonate user impostor`.
That is the measured behaviour and it is why `User` is optional under
`ExternalAuthentication` and the header is omitted entirely: an operator obliged
to invent one would have the connection refused for their own account.

It also constrains the suite. Every scenario expecting a *successful* connect has
to use the identity provider's own user or none at all, so a fresh
`OAUTH2_LOGINS` key cannot be obtained by naming a different `User`. Only
scenarios expecting failure can, because they never reach the impersonation
check.

**The suite cannot use pyodbc.** pyodbc calls `SQLDriverConnectW` with
`SQL_DRIVER_NOPROMPT` unconditionally, including for a `DSN=` string, and core
reads that as forbidding a prompt, so every `ExternalAuthentication` connection
made through pyodbc is refused. The suite loads the driver with `ctypes` and
passes `SQL_DRIVER_COMPLETE` itself. `isql` is unaffected, because `SQLConnect`
carries no *DriverCompletion* and core reads the absent argument as permitting a
prompt; the `trino_oauth` DSN exists for exactly that manual path.

**The browser is a `PATH`-shadowed `xdg-open`, not `$BROWSER`.** `open` 5.4.0
ignores `$BROWSER` and runs `xdg-open` first and unconditionally, and `xdg-open`
consults `$BROWSER` only in its `generic` desktop-environment branch: on a
machine with a session it dispatches to `gio`, which opens a real browser.
`suites/oauth_browser.py` therefore always **exits 0**, because a non-zero exit
sends `open::that` on to `gio open`, and it reports its outcome through a JSONL
record instead. That record is also how the suite counts browser launches, and
what turns a broken login into a diagnosis rather than a suite that waits out the
login budget with nothing to show.

### The spooling protocol

The `spooling` profile brings up MinIO and configures the coordinator to spool.
The driver reads a spooled result when the `Encoding` connection-string key
advertises an encoding, and returns every row inline when it does not.

Off by default because `protocol.spooling.retrieval-mode=storage` has the
*client* fetch segments straight from object storage: a workstation that cannot
reach the bucket would fail queries that succeed without the key, and the driver
cannot know that in advance. Trino's JDBC driver leaves its `encoding` property
unset for the same reason. A coordinator that does not support the requested
encoding **ignores the header and answers inline** — measured against the live
coordinator with `bogus` and with `json+snappy,json`, and end to end through the
driver by running `suites/test_spooling.py` against a stack with no spooling
manager — so setting the key can never fail a connection.

`Client::decode_page` is the decoder, at both page-decode sites in
`src/backend/execute.rs`: a direct page's rows arrive as they are, a spooled
page's segments are fetched, decoded and acknowledged. The catalog and metadata
functions need nothing, because `query_all_rows` goes through `Client::get_all`,
which pages on `QueryPager` and resolves segments itself.

`TrinoStatement::raw_columns` keeps Trino's own `Column` metadata for the result
set. A spooled segment carries values without names or types and is decoded
against that metadata, while Trino sends it on one page only, so the statement
holds it for every later page.

**What spools is bytes, not rows**, which is the trap for anyone extending
`suites/test_spooling.py`. Measured on this stack under `Encoding=json+zstd`:

| Query | Segments |
|---|---|
| `SELECT 1`, and 900 rows of `customer` | 1 inline, 0 spooled |
| 20,000 rows of `customer`, **all** columns | 2 inline, 25 spooled |
| the same 20,000 rows, four columns | inline only, 0 spooled |

A narrow projection never reaches object storage, so the suite's queries are
`SELECT *` deliberately. Rows that arrive spooled are byte-identical to rows that
arrive inline, so a row count proves nothing either: each scenario reads the
driver's log for `Successfully fetched remote spooled segment`, which the client
emits once per *remote* segment and never for an inline one. That log is opened
once per process — core pins its subscriber on the first connection — so the
suite sets `ODBC_LOG_FILE` once and reads the file in deltas.

The suite has **no required profile**. With `spooling` active it drives the
protocol; without it, it asserts the fallback above. Each stack state skips the
other's scenarios by name, so neither is a blind spot.

Abandoning a spooled result set leaves its remaining segments unacknowledged, so
they live until the coordinator's `fs.segment.ttl`, 12 hours by default.
`close_cursor` drains the remaining pages to keep the pooled socket clean and
discards their data; fetching those segments in order to acknowledge them would
download exactly the data the application abandoned.

Four settings in `stack/trino/spooling/` are load-bearing:

- **`retrieval-mode=coordinator_proxy`**, where the default is `storage`. It is
  the only mode that does not require the *client* to reach object storage, and
  the driver runs on the host, where `minio` does not resolve. Confirmed by the
  segment URIs, which name the coordinator's `/v1/spooled/download/...` rather
  than a pre-signed MinIO URI. Covering `storage` means publishing MinIO's port
  *and* adding `127.0.0.1 minio` to the host's `/etc/hosts`, so it belongs in an
  opt-in variant that skips with a reason rather than passing silently.
- **`fs.segment.encryption=false`**, where the default is `true` and means SSE-C.
  MinIO here serves plain HTTP with no key material, so a segment cannot be
  written at all with it on.
- **`initial-segment-size=16kB` and `max-segment-size=64kB`**, against defaults of
  8MB and 16MB. Without them a result would need tens of megabytes before a
  second segment appeared, and the retrieval loop is what needs exercising.
- **`protocol.spooling.inlining` is left at its default of enabled**, because
  that is what a real deployment does. The consequence is the test's to carry: the
  first 1000 rows, up to 128kB, arrive inline, so a query has to exceed that
  before anything is spooled.

A client is expected to acknowledge each segment, which deletes it. Segments from
a client that does not are left to `fs.segment.ttl`, 12 hours by default, so a
long-lived stack accumulates them.

### Windows VM tests

The same suites, driven through the Windows ODBC Driver Manager over WinRM. See
[`integration-tests/windows/WINDOWS.md`](integration-tests/windows/WINDOWS.md) for VM creation and the full
reference.

```bash
./integration-tests/run-tests.sh --windows                          # Linux + Windows
uv run --with pywinrm python3 integration-tests/windows/windows_test.py     # Windows only; Trino must be up
```

Four configurations, matching the Linux run: DSN and DSN-less crossed with
verified and unverified TLS. Each records its result rather than aborting the
run, so one failing configuration no longer hides the other three.

Three things the VM needs that the Linux run does not:

- **`harness.py` travels with `test_integration.py`.** The suite imports it,
  and the VM only receives the files this script deploys.
- **`ca.crt` is deployed too**, so the verified configurations can verify
  rather than only skip.
- **`trino` is mapped to the gateway in the VM's own hosts file.** The VM
  reaches the host by IP, TLS sends no SNI for an IP literal, and Jetty then
  serves Trino's internal certificate. Connecting by a name the coordinator's
  certificate carries is what makes verification possible at all. See
  [Certificates](#certificates).

**Do not diagnose a Windows failure without rebuilding the DLL first.**
`--skip-build` reuses whatever is in `target/x86_64-pc-windows-gnu/release/`,
which can predate the feature under test by days. A stale DLL produced four
consecutive `SQL_ATTR_QUERY_TIMEOUT` failures that looked exactly like a
Windows Driver Manager defect, and the driver's own log gave it away:
`SQL_ATTR_QUERY_TIMEOUT=2 not supported, substituting 0` is what core reports
for a backend with no `set_query_timeout`, which this driver has had since
2026-07-29.

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
- `stackable-odbc-trino-<version>-windows-x64.zip` — `.dll`, `.mez`, the `.bat`
  scripts and `configure-dsn.ps1`
- `StackableTrinoODBC-<version>.mez` — standalone Power BI asset

### The Windows DSN dialog

`packaging/windows/configure-dsn.ps1` is a WinForms dialog covering the whole
connection-string surface. It is **not** the ODBC Data Source Administrator's
"Add…" button: that button loads the driver's setup DLL and asks it for a
dialog, and this driver answers headlessly, so odbcad32 reports
`ODBC_ERROR_INVALID_KEYWORD_VALUE` and the message core posts. Giving that
button a dialog needs `ffi/setup.rs`'s `config_dsn_w` to become generic over
the backend, which is a core change.

Layout, the read path, the write path and validation are generated from one
`$Fields` table, and `dsn_keys_match_the_connection_string_parser` in
`src/lib.rs` fails `cargo test` if that table and the `PARAM_` constants ever
disagree in either direction.

The write goes through `SQLConfigDataSourceW`, so the driver's own `ConfigDSN`
stays in the loop. Two things about it are measured rather than assumed:

- **`SQLInstallerErrorW` returns a 16-bit `RETCODE`.** Declared as a 4-byte
  `bool` it silently yields no error record at all, which makes a failed write
  look like a write with no explanation.
- **The five `name:value;name2:value2` keys are written bare.** Braces belong
  to connection-string syntax, where `;` separates parameters. Measured against
  the live driver: a bare DSN value applies both session properties, a braced
  one fails the connection with `08001`.

Secrets are written only when their **Save** box is ticked, which is off by
default. A saved secret is stored unencrypted, and a System data source puts it
in HKLM where every local user can read it.

### The connector's advanced options

`StackableTrinoODBC.Contents` takes `optional options as record`, and
`Config_AdvancedOptions` is the list of keys it accepts.
`connector_options_are_connection_string_keys` in `src/lib.rs` checks that list
against the `PARAM_` constants, and against `StackableTrinoODBC.OptionsType` in
both directions: the list is what the connection string is built from, the type
is only what the Get Data dialog renders, and nothing in Power Query relates
them.

The four keys `Backend::sensitive_connect_keywords` declares are deliberately
absent — `AccessToken`, `ExtraCredentials`, `ExtraHeaders`, `ProxyPassword`. An
option set here is stored in the query text inside the `.pbix`, which is a file
people mail to each other.

**`SessionProperties`, `ResourceEstimates` and `Roles` are unverified through
Power Query.** All three carry `;`, and whether `Odbc.DataSource` escapes a
record value containing one is not established here: nothing in this repo
executes the `.pq`, since `suites/test_folding_contract.py` parses it and Power
BI is what runs it. They are passed unbraced, relying on Power Query's own
escaping. **Before a release, set `SessionProperties` to two pairs in Power BI
Desktop and confirm both apply**; if only the first does, brace those three in
the connector the way `Build-ConnectionString` does in `configure-dsn.ps1`.
`DirectQuery` needs no such check and no option of its own: it is a `Publish`
capability (`SupportsDirectQuery`), and Power BI draws the Import/DirectQuery
selector itself.

`connector/` holds the Power Query custom connector source; `connector/build.sh`
zips it into the `.mez`.

`connector/StackableTrinoODBC.pq` carries its own `[Version = "..."]`, which
Power BI reads to decide whether an installed `.mez` supersedes the one already
present. **It tracks the Cargo version**: `release.toml` rewrites it in the same
commit as the bump, exactly as it does `packaging/README.md`, and
`connector_version_matches_the_crate` in `src/lib.rs` fails `cargo test` if the
two ever part. Do not edit it by hand.

It was maintained separately until 2026-07-29 and had drifted to `1.0.0` against
a crate at `0.0.1`, so one release shipped a `.mez` and a `.so` naming different
versions — and a bug report quotes whichever the reporter installed.

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
