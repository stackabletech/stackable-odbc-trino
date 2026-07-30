# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Interactive OAuth 2.0 authentication**, through the new
  `ExternalAuthentication` and `ExternalAuthenticationTimeout`
  connection-string keys. `ExternalAuthentication=true` selects Trino's
  external-authentication flow: the coordinator answers with a login URL, the
  driver presents it, and the client polls for the bearer token. It requires
  `Protocol=https` and cannot be combined with `Password` or `AccessToken`;
  either combination fails the connection as ambiguous rather than silently
  preferring one.

  The URL is logged before the browser is opened, and unconditionally. A Driver
  Manager discards whatever the driver writes to stderr, so under `isql`, Power
  BI or Excel the log is the only channel that survives — `ODBC_LOG_FILE` and
  `ODBC_LOG_LEVEL` are what make it reachable. A browser that will not launch is
  therefore not an error: there may be no display or no permission to start one,
  and the login can still be completed from the logged URL, because the client
  polls for the token rather than waiting on the handler.

  **One login serves every connection to the same coordinator**, for the life of
  the process, keyed on protocol, host, port and user. The client caches the
  token behind one `Auth` value, and this driver builds a client per connection,
  so without the cache a pool warming ten connections would open ten browsers.
  An expired token needs no handling: it yields a `401` and the flow re-runs.

  `SQL_DRIVER_NOPROMPT` is honoured. An application that passed it gets
  `SQL_ERROR` naming `AccessToken` as the non-interactive alternative, rather
  than a browser it asked not to see — which is what the matching
  `stackable-odbc-core` change exists for: core decides whether a connect may
  prompt, this driver decides how, and the browser dependency stays here.

  **`SQL_ATTR_LOGIN_TIMEOUT` does not bound the interactive wait.** It waits on
  a person, not on the data source, and applications set login timeouts
  assuming a machine round trip — a tool defaulting to 15s would abort every
  login while the user was still typing. `ExternalAuthenticationTimeout`
  (seconds, default 300) bounds it instead, and one `warn!` records the
  deviation when an application set both.

  **`User` becomes optional**, and `X-Trino-User` is then not sent at all, so
  Trino resolves the session user from the authenticated identity. It has to
  work this way: Trino reads a `X-Trino-User` that *disagrees* with the
  authenticated identity as an impersonation request rather than ignoring it,
  so a name the operator was obliged to invent would have the connection
  refused for their own account whenever it did not match the identity
  provider's user-mapping exactly. A `User` given anyway is still honoured, and
  `SessionUser` still expresses deliberate impersonation.

- **`SQLCopyDesc` is now advertised by `SQLGetFunctions`.** Core implements
  explicit descriptor handles, so the function works; it had been withheld
  because it once did not. `SQLGetFunctions` is what the Windows Driver Manager
  builds its dispatch table from, so reporting a working function unsupported
  means the DM answers `IM001` and the application never calls it.

- **Eight connection-string keys covering the Trino client options the driver
  never exposed**: `SessionProperties`, `ExtraCredentials`, `ResourceEstimates`,
  `Path`, `ClientInfo`, `TraceToken`, `DisableCompression` and `MaxAttempts`.
  Each maps to a `trino-rust-client` builder option that was simply never
  called, so the settings were unreachable through this driver even though the
  Trino JDBC driver exposes most of them.

  `SessionProperties` is the one most often wanted — it is how a connection sets
  `query_max_run_time`, `join_distribution_type` or any catalog property.
  `ExtraCredentials` carries secrets and is declared in
  `Backend::sensitive_connect_keywords`, so it is redacted from diagnostics and
  from the connection string echoed back by `SQLDriverConnect`.

  The three key-value keys take the Trino JDBC driver's format verbatim
  (`name:value;name2:value2`), so a value copied from a JDBC URL transfers
  unchanged. Because `;` also separates ODBC connection-string parameters, such
  a value must be wrapped in braces —
  `SessionProperties={query_max_run_time:10m;example.foo:bar}` — or everything
  after the first pair is silently discarded by the connection-string parser. A
  malformed pair fails the connection rather than being skipped.
- **`SQL_ATTR_QUERY_TIMEOUT` is now enforced.** Setting it previously returned
  `01S02` and substituted `0`, so an application that asked for a deadline was
  told, correctly but unhelpfully, that it had none. The driver now declares
  `QueryTimeout::CoreCancels`, so `SQLGetStmtAttr` reads back the value that was
  set, and `stackable-odbc-core` arms a timer that cancels the query — Trino's
  `DELETE /v1/query/{id}`, so the coordinator stops the work — and reports
  `HYT00`.

  The deadline covers `SQLFetch`, which is what makes it useful here: Trino
  answers with column metadata before it has computed a row, so `SQLExecDirect`
  returns in milliseconds and a slow query spends its time paging. Verified
  against a live coordinator under a 2-second deadline, both with and without a
  Driver Manager: `SQLFetch` on `SELECT count(*) FROM tpcds.sf10.store_sales`
  reports `HYT00` after 2.0s, where the query runs ~24s uncancelled. The
  connection remains usable afterwards.

  `SET SESSION query_max_run_time` was considered and rejected: it is a session
  property, so it would give every statement on the connection the most
  recently set value, and it would put a round trip inside `SQLSetStmtAttr`.

- **`SQL_ATTR_CONNECTION_DEAD` now reports a lost connection.** It previously
  always answered `SQL_CD_FALSE`, so a connection pool would hand out a
  connection whose link had failed, and the next caller's first query failed for
  no reason it could see. The driver now records a communication link failure
  wherever it is observed — including a `SQLFetch` page request — and reports it
  without a round trip. Only a link failure counts: a timeout, an auth rejection
  and a server-side query error all leave the connection usable.
- **`SQL_ATTR_LOGIN_TIMEOUT` and `SQL_ATTR_CONNECTION_TIMEOUT` now reach the
  connection.** Both are set through `SQLSetConnectAttr` rather than the
  connection string, and were previously discarded. `SQL_ATTR_CONNECTION_TIMEOUT`
  becomes the per-request HTTP timeout, overriding the `QueryTimeout`
  connection-string key when set; `SQL_ATTR_LOGIN_TIMEOUT` bounds the connect
  round trip and reports `HYT00` on expiry. `0` means "no timeout" for both, per
  the spec, and is not treated as unset.
- **`SQLCancel` now reports `HY008` for a cancel that lands between page
  requests.** The driver already reported it for one recognised from Trino's own
  `USER_CANCELED` code, which covers a cancel arriving while a request is in
  flight; `Backend::is_cancelled` covers the other half.
- Initial extraction of `stackable-odbc-trino` into its own repository, from the
  `stackable-odbc-rs` workspace that held the core framework and the SQLite
  driver alongside it. Provides the ODBC driver for Trino: the `Backend` and
  `StatementBackend` implementations, connection-string parsing, Trino-to-ODBC
  type conversion, ODBC escape-sequence translation, and the catalog and
  metadata functions, with the C ABI entry points generated by
  `stackable-odbc-core`'s `forward_ffi!` macro.
- `SQL_ATTR_METADATA_ID` is honoured by the catalog functions. Setting it to
  `SQL_TRUE` reclassifies most of their string arguments from pattern values to
  identifiers, which are matched case-folded per `SQL_IDENTIFIER_CASE`
  (`SQL_IC_LOWER` for Trino) and literally, so a `%` or `_` in a name is no
  longer a wildcard. `stackable-odbc-core` normalises the arguments from values
  this driver already declares, so nothing about which rows Trino is asked for
  changed except the interpretation the spec assigns them.

  One new diagnostic comes with it: the catalog functions return `HY009` when
  `SQL_ATTR_METADATA_ID` is `SQL_TRUE` and `CatalogName` is a null pointer,
  which the spec requires of a driver whose data source supports catalog names.
  Trino's do. `SQLColumnPrivileges` additionally returns `HY009` for a null
  `TableName` unconditionally; it is the only one of the ten whose spec page
  states that without a **(DM)** marker.
- `Source` and `ClientTags` connection-string keys. `Source` is what Trino
  records as the query's source and shows in `system.runtime.queries`, and it
  defaults to `stackable-odbc-trino/<version>`, carrying the driver's Cargo
  version so one build can be told from another after a rollout — queries
  previously reached the
  coordinator under `trino-rust-client`, the client library's name, which
  identified neither the driver nor the application. `ClientTags` takes a
  comma-separated list, which Trino matches when selecting a resource group,
  so an operator can queue this driver's traffic separately.
- `SQLDescribeParam` reports the type Trino infers for each parameter, read
  from `DESCRIBE INPUT` on the prepared statement. Every parameter of every
  statement was previously described as `VARCHAR(4000)` — a generic answer that
  makes a client sizing its buffers from it send a number as text and get a
  type error back from the coordinator.

  Parametric types carry their precision and scale, so a `char(20)` parameter
  reports a size of 20 and a `decimal(10,2)` reports 10 and 2. A statement
  Trino declines to prepare still falls back to the generic answer rather than
  failing the call.
- `SQLTablePrivileges` reports Trino's table-level privileges, read from the
  connected catalog's `information_schema.table_privileges`. It previously
  returned an empty result set for every data source.

  Rows appear only for connectors that implement permission management — Hive
  and Iceberg under `sql-standard` security, say. A connector without it, which
  includes every JDBC-backed connector, has no privileges to report and still
  answers with zero rows rather than an error.
- `SQLColumnPrivileges`, `SQLProcedures` and `SQLProcedureColumns` describe
  their result sets and return no rows, which is what they already did, now
  stated by the driver rather than left to a default. Trino grants privileges
  on tables and never on columns, and while it has callable procedures
  (`CALL system.runtime.kill_query(...)`) it publishes no metadata naming them
  — `system.jdbc.procedures` is a JDBC-compatibility view that is always empty.
  This matches the `SQL_ACCESSIBLE_PROCEDURES` of `"N"` the driver reports.
- **`SessionUser` connection-string key**, for running statements as another
  user while the connection authenticates as the principal in `User` — JDBC
  spells it `sessionUser`. Trino sends it as `X-Trino-User`, so the coordinator
  applies the impersonated user's permissions and records it against the query.
  Verified against a live coordinator: `SELECT current_user` answers `alice`
  with `SessionUser=alice` and `admin` without. One ODBC connection per
  impersonated user is the intended use, because session state accumulates per
  connection and would otherwise carry a `SET SESSION` made for one identity
  into another.
- **`Locale` connection-string key**, sent as `X-Trino-Language` for
  locale-dependent formatting.
- **`Proxy`, `ProxyUser` and `ProxyPassword` connection-string keys**, routing
  every request through an HTTP or HTTPS proxy, with optional Basic
  credentials. A `socks5://` URL is refused when the connection is built rather
  than failing later at connect time, because routing through SOCKS needs a
  `reqwest` feature `trino-rust-client` does not enable.

  Credentials written into the URL's userinfo (`http://bob:s3cret@proxy:3128`)
  are rejected, naming the two keys to use instead. The whole `Proxy` value is
  echoed back by `SQLDriverConnect`, so a password there is one the driver
  cannot redact — `ProxyPassword` is its own key precisely so it can be
  declared in `Backend::sensitive_connect_keywords`. An `@` outside the
  authority, in a path or query, is left alone. Setting one of the two
  credential keys without the other fails the connection instead of
  authenticating to nothing and meeting a `407` that names neither.
- **`ExtraHeaders` connection-string key**, arbitrary HTTP headers on every
  request in the same `name:value;name2:value2` form as the other key-value
  keys, for gateways and reverse proxies that require one. A name the client
  manages is rejected when the connection is built rather than sent alongside
  the client's own value, because `reqwest` appends and the request would
  otherwise carry two — verified against a live coordinator, where
  `ExtraHeaders={X-Trino-User:evil}` fails with `reserved header:
  X-Trino-User is managed by the client`.

  Declared in `Backend::sensitive_connect_keywords`. Unlike `AccessToken` and
  `ExtraCredentials` it is not a credential by definition, but a header a
  gateway demands is routinely an API key and nothing in the name says so.
- **`ClientCapabilities` connection-string key**, a comma-separated list added
  to the `PARAMETRIC_DATETIME` and `PATH` the client always sends. Those two
  cannot be dropped: its type decoder depends on both.
- **`TimeZone` connection-string key**, an IANA zone name sent as
  `X-Trino-Time-Zone`. Trino resolves `current_timestamp`, `TIMESTAMP WITH TIME
  ZONE` literals and every `AT TIME ZONE` against the session zone, so leaving
  it unset means those follow the coordinator's JVM — a property of the server
  rather than of the query. An unknown zone fails the connection instead of
  being ignored, because the alternative is timestamps that are silently hours
  out. Verified against a live coordinator: `current_timezone()` answers `UTC`
  unset, and `Europe/Berlin` or `Pacific/Auckland` when set.
- **`Roles` connection-string key**, an authorisation role per catalog in the
  same `catalog:role;catalog2:ALL` form as the other key-value keys, so it needs
  `{braces}` for the same reason. The value is a bare role name or the keyword
  `ALL`/`NONE`, matching JDBC's `roles` property; Trino's own `X-Trino-Role`
  spelling wraps a name as `ROLE{admin}`, and the driver renders that, because
  those braces would otherwise have to survive the connection-string parser.

  Roles are what Hive and Iceberg under `sql-standard` security check, and so
  are what decides whether `SQLTablePrivileges` returns a row — until now the
  client tracked `X-Trino-Set-Role` into a map that nothing could populate.
- `SQLGetFunctions` reports `SQLGetDescField`, `SQLSetDescField`,
  `SQLGetDescRec` and `SQLSetDescRec` as supported, which
  `stackable-odbc-core` implements against the implicit descriptors an
  application reaches through `SQLGetStmtAttr(SQL_ATTR_APP_ROW_DESC)` and its
  three siblings. The Windows Driver Manager builds its dispatch table from
  this bitmap, so a working function reported unsupported is one the
  application never calls — it answers `IM001` instead. `SQLCopyDesc` and
  `SQLAllocHandle(SQL_HANDLE_DESC)` are the explicit-descriptor half, which
  core does not implement, so `SQLCopyDesc` stays unadvertised.
- The Power Query connector folds `BIGINT`, `SMALLINT`, `TINYINT` and `BOOLEAN`
  constants, which previously had no `Constant` visitor entry and so were
  evaluated locally. `BIGINT` is the one that matters: it is what most Trino
  connectors report an integer column as, so a filter comparing one to a
  literal did not fold.

  The remaining types the driver reports stay unfolded on purpose, and the
  reasons are recorded beside the visitor. `CHAR` is the instructive one:
  `CAST('abc' AS CHAR)` is `char(1)` in Trino, so an entry would truncate the
  literal to `'a'` and fold an equality filter into one matching no rows.
  Trino also cannot cast a `varchar` to either interval type, so no CAST target
  exists to name for those.

### Changed

- **A fetch stopped by a cancellation now reports `HY008` instead of an empty
  result set.** `SQLFetch` returned `SQL_NO_DATA` when it found the statement
  already cancelled, which says "your result set ended" for rows that were in
  fact discarded. It also silently defeated `SQL_ATTR_QUERY_TIMEOUT`:
  `stackable-odbc-core` relabels a fetch *error* to `HYT00` when its timer
  fired and has nothing to relabel when the fetch succeeds, so a timeout whose
  cancel landed between page requests — rather than during one — surfaced as an
  empty result set with no diagnostic, indistinguishable from an empty table.
  Both paths now report `HY008`, and `HYT00` when a timeout caused it, so what
  the application is told no longer depends on the timing of the cancel.
- **The Linux installer now writes `Threading = 2`** into the driver's
  `odbcinst.ini` section. unixODBC's default of `3` serialises at the
  environment level and holds a cross-thread `SQLCancel` behind the call it was
  meant to interrupt — measured on a query running ~24s and cancelled after 2s,
  `SQLFetch` raised `HY010` after 23.9s at `Threading = 3` and `HY008` after
  2.0s at `Threading = 2`. An existing installation must re-run the installer or
  add the line by hand. This does not affect `SQL_ATTR_QUERY_TIMEOUT`, which
  fires under either setting.
- **`SQL_QUOTED_IDENTIFIER_CASE` reports `SQL_IC_LOWER`, not `SQL_IC_SENSITIVE`.**
  `stackable-odbc-core` used to hard-wire the latter on every driver's behalf;
  it is now `TrinoBackend::quoted_identifier_case`, and measurement against a
  live coordinator says Trino does not agree with the old answer. A column
  created in PostgreSQL as `"MixedCol"` and reached through the `postgresql`
  catalog resolves as `"MixedCol"`, `"mixedcol"` *and* `"MIXEDCOL"`, so quoted
  identifiers are case-insensitive, and `information_schema` reports the name
  as `mixedcol`, so the system catalog stores it folded. That is `SQL_IC_LOWER`
  by definition, and it matches what `SQL_IDENTIFIER_CASE` already said.

  An application generating SQL from `SQLColumns` or `SQLTables` output reads
  this to decide how to quote. Under `SQL_IC_SENSITIVE` it believes a quoted
  name must match the catalog's spelling exactly, a restriction Trino does not
  impose.

- **`SQL_CURSOR_SENSITIVITY` reports `SQL_UNSPECIFIED`, not `SQL_INSENSITIVE`.**
  Core owns this value and corrected it: insensitivity is a promise that no
  other cursor's changes become visible, and core's fetch streams rows from this
  backend as the application asks for them, so it can make no such promise about
  rows it has not read yet.

- **`SQL_DBMS_NAME`, `SQL_DBMS_VER`, `SQL_DRIVER_NAME`, `SQL_DRIVER_VER`,
  `SQL_TXN_CAPABLE`, `SQL_INTEGRITY`, `SQL_MULTIPLE_ACTIVE_TXN`,
  `SQL_SPECIAL_CHARACTERS` and `SQL_ACCESSIBLE_PROCEDURES` are now declarations
  rather than answers.** Each moved from an arm in `backend/info.rs`, or from a
  value core supplied on this driver's behalf, to a required `Backend` method.
  Every reported value is unchanged except `SQL_QUOTED_IDENTIFIER_CASE` above —
  what changed is that there is now one place each of them lives, and the
  compiler asks for it.

  `SQL_SPECIAL_CHARACTERS` stays `""` and is now a measurement rather than an
  understatement: `SELECT 1 AS a@b`, and the same with `:`, `$`, `#`, `-` and a
  space, each fail with `SYNTAX_ERROR` against a live coordinator, while `a_b`
  succeeds. Trino's identifier production admits nothing beyond the
  alphanumerics and underscore, which is exactly what this info type excludes.

- **An unsupported statement-attribute value is now answered rather than
  accepted silently.** `SQLSetStmtAttr` reports `01S02` and stores the value the
  driver will actually use for the eight attributes the spec's substitution row
  names, plus `SQL_ATTR_CURSOR_SCROLLABLE` and `SQL_ATTR_PARAMSET_SIZE`; and
  `HYC00` for a value with no substitution to offer —
  `SQL_ATTR_USE_BOOKMARKS` above `SQL_UB_OFF`, `SQL_ATTR_RETRIEVE_DATA =
  SQL_RD_OFF`, `SQL_ATTR_CURSOR_SENSITIVITY = SQL_SENSITIVE`,
  `SQL_ATTR_ENABLE_AUTO_IPD = SQL_TRUE` and `SQL_ATTR_ASYNC_ENABLE =
  SQL_ASYNC_ENABLE_ON`. An application that asks for a 30-second query timeout
  no longer reads `30` back from a driver that applies none. Owned by
  `stackable-odbc-core`; recorded here because it is what an application using
  this driver observes.

- **`SQLSetConnectAttr` enforces its driver-side rules.**
  `SQL_ATTR_PACKET_SIZE` reports `HY011` once the connection is open, which the
  spec states outright; `SQL_ATTR_ENLIST_IN_DTC` and `SQL_ATTR_ASYNC_ENABLE =
  SQL_ASYNC_ENABLE_ON` report `HYC00`, since this driver reports `SQL_AM_NONE`
  for `SQL_ASYNC_MODE` and enlists in no distributed transaction.

- **`SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` reports `HYC00` where it
  previously returned `SQL_SUCCESS`.** The old success was empty: the value was
  stored on the handle and nothing switched, so an application was told its
  unqualified names now resolved in a catalog the session was not using.

  Trino cannot switch a catalog without also switching the schema. `USE` is the
  only statement that moves the session catalog and its grammar requires one —
  `USE postgresql.public` works, `USE postgresql` is `NOT_FOUND`, parsed as a
  schema name — so honouring the call would mean inventing a schema and moving
  where unqualified names resolve, which is the same lie one level down. See
  `TrinoBackend::current_catalog` for the coordinator probes.

  This is a visible change for anything that sets the attribute during
  connection setup. Nothing in the tested paths does: all four
  `test/run-tests.sh` configurations, `isql`, and the Windows Driver Manager
  suite are unaffected, and the Power Query connector passes `Catalog` in the
  connection string rather than as an attribute.

- The Power Query connector binds parameters instead of inlining literals
  (`Config_UseParameterBindings`). It was off while the driver's parameter
  binding was incomplete, and turning it off also declared
  `SQL_API_SQLBINDPARAMETER = false` — contradicting the driver, which lists
  `BindParameter` among its supported functions.
- **`Protocol` now defaults to `https`** rather than `http`. A connection
  string that names no protocol is encrypted; an unencrypted connection has to
  be asked for with `Protocol=http`.

  This is the safe direction for an omitted value — plaintext should be a
  choice, not a silence — but it changes what an existing connection string
  means. Any application pointing at a plaintext coordinator without naming a
  protocol must add `Protocol=http`. The failure is a connection error at
  `SQLDriverConnect`, not a silent downgrade.

  The Power Query connector's optional `protocol` argument defaults to `https`
  to match, so both entry points agree.
- A server-side error's `SQLGetDiagRec` message no longer carries Trino's
  `failure_info`. The coordinator's Java stack was reaching the application
  through the diagnostic's causal chain, putting between 1,700 and 15,000
  characters into every error message — `DIVISION_BY_ZERO` was the worst,
  around 30 KB of UTF-16 across roughly 168 frames. The same errors now produce
  62 to 124 characters.

  Nothing an application can act on is lost. The message still names the Trino
  error and what went wrong (`query error [COLUMN_NOT_FOUND]: line 1:8: Column
  'nope' cannot be resolved`), and `SQLGetDiagRec` still reports Trino's own
  error code verbatim through `NativeErrorPtr`. The full `failure_info` is
  logged at `debug` instead, reachable through `ODBC_LOG_LEVEL` /
  `ODBC_LOG_FILE`.

  A transport failure's cause is unaffected and still carried whole; its
  message is a single line.
- `SQLCancel` can now interrupt a query from a thread other than the one
  executing it, which is the case the ODBC spec singles out and the one that
  matters to a BI tool with a cancel button. `stackable-odbc-core` replaced
  `Backend::cancel(&mut Self::Statement)` — a signature that could not be
  satisfied while another thread held the statement — with a `CancelToken` built
  once per statement from the connection. This driver's token carries the Trino
  client, the runtime and the query id, which `SQLExecDirect`/`SQLExecute`
  publish as soon as the coordinator accepts the query, so a cancel arriving
  while the query is still queued or planning now reaches it.

  A `SQLFetch` interrupted by a concurrent `SQLCancel` now reports `HY008`
  ("operation canceled"), which the spec defines for exactly this case — "the
  function was called, and before it completed execution, `SQLCancel` … was
  called on the StatementHandle from a different thread in a multithreaded
  application". It previously reported `HY000`, because Trino fails the
  in-flight page request with `USER_CANCELED` and nothing recognised that as a
  cancellation. The statement is left reporting a finished result set rather
  than `24000`: a cancellation the application asked for is not an abandoned
  cursor.

  The cancellation is recognised from Trino's own `USER_CANCELED` error code
  rather than from the driver's cancel flag, because the two race — the
  cancelling thread sets its flag only after its `DELETE` returns, by which
  time the coordinator may already have failed the in-flight request. Reading
  the server's verdict also covers a query killed by something else entirely,
  such as `CALL system.runtime.kill_query`.

  A statement cancelled between fetches reports `SQL_NO_DATA` from the next
  `SQLFetch`, as before. The six catalog functions remain uncancellable: four
  perform no I/O, and `SQLTables`/`SQLColumns` page inside the Trino client,
  which never surfaces the query id a cancel needs.
- `SQLGetInfo` called before `SQLDriverConnectW` no longer answers the info
  types derived from this driver's capability declarations, and returns
  `stackable-odbc-core`'s benign default for them instead. Those declarations
  now take a connection — `SQLGetInfo` is a per-connection call, so what a data
  source can do is a property of the connection — and core skips the ones that
  need one when there is no connection yet rather than answering from an
  invented value.

  Twenty-two of this driver's documented answers are affected:
  `SQL_SEARCH_PATTERN_ESCAPE`, `SQL_IDENTIFIER_QUOTE_CHAR`, `SQL_CATALOG_TERM`,
  `SQL_SCHEMA_TERM`, `SQL_CATALOG_NAME_SEPARATOR`, `SQL_COLUMN_ALIAS`,
  `SQL_ORDER_BY_COLUMNS_IN_SELECT`, `SQL_CATALOG_NAME`,
  `SQL_DATA_SOURCE_READ_ONLY`, `SQL_ACCESSIBLE_TABLES`, `SQL_GROUP_BY`,
  `SQL_CONCAT_NULL_BEHAVIOR`, `SQL_IDENTIFIER_CASE`, `SQL_NULL_COLLATION`,
  `SQL_SUBQUERIES`, `SQL_UNION`, `SQL_DEFAULT_TXN_ISOLATION`,
  `SQL_CONVERT_FUNCTIONS`, `SQL_TXN_ISOLATION_OPTION`, `SQL_ALTER_TABLE`,
  `SQL_OUTER_JOIN_CAPABILITIES` and `SQL_SQL_CONFORMANCE`.

  The connected answers are unchanged, and no call that previously succeeded
  now returns `SQL_ERROR` — core substitutes a default rather than failing, so
  an application that queries these before connecting reads a conservative
  value instead of Trino's.
- `SQLGetTypeInfo` now requires an open connection, inherited from core: the
  type list belongs to the data source.
- `SQLGetData` called repeatedly for one column now returns the value in parts,
  inherited from core. Each call delivers the next chunk with `01004`, the last
  returns `SQL_SUCCESS`, and a further call returns `SQL_NO_DATA`. Every call
  previously restarted at the beginning of the value, so the spec's documented
  read-in-a-loop pattern never terminated — an application reading a column
  larger than its buffer hung.
- `SQL_CURSOR_COMMIT_BEHAVIOR` now reports `SQL_CB_PRESERVE` instead of
  `SQL_CB_DELETE`. `stackable-odbc-core` derives it from
  `Backend::cursor_commit_behavior` rather than hard-coding it, and this driver
  keeps the `CursorBehavior::Preserve` default: it reports `SQL_TC_NONE`, so no
  transaction ever closes a cursor.

- `SQL_ALTER_TABLE` now reports `SQL_AT_ADD_COLUMN_SINGLE |
  SQL_AT_ADD_CONSTRAINT | SQL_AT_DROP_COLUMN` instead of `0`, which claimed
  Trino cannot `ALTER TABLE` at all. Each bit was confirmed against a live
  coordinator; `DEFAULT`, `CASCADE`, `RESTRICT`, `SET DEFAULT` and `ADD
  CONSTRAINT` are all rejected by Trino's grammar and stay unclaimed.
- `SQL_SQL_CONFORMANCE` now reports `0` instead of `SQL_SC_SQL92_ENTRY`. Trino's
  `CREATE TABLE` rejects `PRIMARY KEY`, `UNIQUE`, `CHECK` and `REFERENCES`, so
  the referential integrity entry level requires is absent, and the driver
  reports `SQL_TC_NONE` where entry level requires `COMMIT`/`ROLLBACK`.

- `SQL_STRING_FUNCTIONS`, `SQL_SYSTEM_FUNCTIONS` and `SQL_TIMEDATE_FUNCTIONS`
  now describe exactly the `{fn ...}` escapes the driver can translate, which
  is what the spec defines those bitmaps to mean. Twelve functions were
  previously advertised on the strength of Trino having an equivalent, while
  the escape itself reached the coordinator untranslated and failed there — a
  client that read the bitmap and emitted `{fn CURDATE()}` got
  `FUNCTION_NOT_FOUND`. All twelve now translate: `LOCATE(a, b)` becomes
  `position(a IN b)`, `CURDATE`/`CURTIME`/`CURRENT_DATE`/`CURRENT_TIME`/
  `CURRENT_TIMESTAMP`/`USERNAME`/`DBNAME` become the bare keywords Trino takes
  without parentheses, `TIMESTAMPADD`/`TIMESTAMPDIFF` become
  `date_add`/`date_diff` with the interval keyword re-quoted as a unit string,
  and `DAYOFWEEK` becomes an expression converting Trino's ISO day numbering
  to ODBC's — a rename alone would have returned a plausible, silently wrong
  day. Every advertised escape is executed against a real coordinator by
  `every_advertised_scalar_function_escape_runs_on_trino`.
- `SQL_STRING_FUNCTIONS` reports `SQL_FN_STR_LOCATE_2` rather than
  `SQL_FN_STR_LOCATE`. The spec splits the two-argument and three-argument
  forms across those flags, and only the two-argument one is supported: ODBC's
  third argument is a start offset where the third argument of Trino's
  `strpos()` is an occurrence index.
- `SQL_TIMEDATE_ADD_INTERVALS` and `SQL_TIMEDATE_DIFF_INTERVALS` report
  `SECOND | MINUTE | HOUR | DAY | WEEK | MONTH | QUARTER | YEAR` instead of
  `0`, matching the units `TIMESTAMPADD`/`TIMESTAMPDIFF` now accept.
  `SQL_FN_TSI_FRAC_SECOND` is not claimed: ODBC defines it as billionths of a
  second and Trino's finest unit is `millisecond`.
- `SQL_ACCESSIBLE_TABLES` now reports `"N"` instead of `"Y"`. `"Y"` guarantees
  the connected user has `SELECT` on every table `SQLTables` returns, which
  depends on the deployment's access control rather than on the driver.
- `SQL_DATABASE_NAME` now reports the catalog the connection was opened
  against, instead of the empty string.
- `SQL_KEYWORDS` now lists Trino's 22 reserved words that ODBC does not
  already reserve — `UNNEST`, `LISTAGG`, `ROLLUP`, the `JSON_*` family and the
  rest — instead of the empty string, which claimed Trino reserves nothing of
  its own. Applications read this to decide which identifiers need quoting, so
  an empty list can leave a generated identifier unquoted where it collides
  with a keyword. The list is static, from Trino's reserved-words
  documentation: unlike SQLite there is no API to enumerate them, and unlike
  the SQL-92 capability bitmaps it is deliberately not version-gated, because
  over-reporting a keyword only causes a needless quote while under-reporting
  causes a parse error.
- `SQLGetDiagRec` now reports Trino's own error code through `NativeErrorPtr`,
  and its message carries the whole causal chain, where every failure
  previously arrived as native error `0` with the client error flattened into
  a string. A server-side rejection now reaches the application as its Trino
  code — `SYNTAX_ERROR` as 1, `TABLE_NOT_FOUND` as 44, and so on — which is
  the only value in that field an application can act on. Transport failures
  keep `0`, the spec's "no native code", because Trino's taxonomy has no entry
  for them; so does `PERMISSION_DENIED`, whose code `trino-rust-client`
  discards before the driver sees it.
- `SQLDescribeCol` and `SQLColAttribute` now report `SQL_NULLABLE_UNKNOWN` for
  a result column's nullability, instead of `SQL_NULLABLE`. Trino's REST
  protocol describes a result column with a name and a type and nothing else,
  so the driver has no basis for either of the two definite answers, and the
  spec defines the third value for exactly that case. `SQL_NULLABLE` was a
  guess that is safe for a projection of a nullable base column and wrong for
  a `COUNT(*)`. An application that branches on nullability should treat the
  unknown as nullable, which is what it already had to do.

### Fixed

- **`SQL_ATTR_CURRENT_CATALOG` and `SQL_DATABASE_NAME` did not follow a
  `USE`.** Both reported the `Catalog` connection-string value for the life of
  the connection, so after `USE postgresql.public` a connection opened against
  `tpcds` still answered `tpcds` — naming a catalog the session had left, while
  the application's own unqualified table names resolved in the new one. They
  are now read from the client's session, which tracks the
  `X-Trino-Set-Catalog` the coordinator sends back, and the connection-string
  value is the fallback for the window before any response has been seen. The
  attribute is still not settable: `SQLSetConnectAttr` reports `HYC00`, because
  Trino's `USE` grammar cannot move a catalog without also inventing a schema.
- **`SQL_ATTR_CURRENT_CATALOG` was write-only, and disagreed with
  `SQL_DATABASE_NAME`.** The spec makes them one value under two names, but the
  attribute was a handle-local string nothing seeded from the connection: a
  connection opened against `tpcds` reported `"tpcds"` from
  `SQLGetInfo(SQL_DATABASE_NAME)` and `""` from
  `SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG)`. The driver now declares
  `Backend::current_catalog`, which core feeds to both readers, so there is one
  source and they cannot diverge.
- **Integer statement attributes were read back four bytes wide.** Every
  non-pointer attribute on the `SQLSetStmtAttr` page is declared an `SQLULEN`,
  which is 64-bit on a 64-bit platform, and `SQLGetStmtAttr` ignores
  `BufferLength` for a non-string value. An application writing
  `SQLULEN v; SQLGetStmtAttr(stmt, SQL_ATTR_MAX_ROWS, &v, 0, NULL);` therefore
  kept whatever was on its stack in the top half of `v` — reading an enormous
  row limit rather than the `0` the driver reported. Fixed in
  `stackable-odbc-core` for all nineteen; found from this driver's suite.
- **`SQL_ATTR_METADATA_ID` set on the *connection* never reached its
  statements.** It is one of exactly two attributes the spec allows an
  application to set at the connection level, and the value was stored and read
  back correctly while nothing acted on it: every catalog call on every
  statement of that connection kept pattern semantics. An application taking
  that route got `SQL_SUCCESS`, saw its value echoed, and then had
  `SQLColumns(TableName = "my_table")` match `my7table` as well, with no
  diagnostic to explain it. A statement now starts from its connection's value;
  per the ODBC 2.x rule the connection-level route inherits, that applies to
  statements allocated afterwards, and a later `SQLSetStmtAttr` still overrides
  it. Fixed in `stackable-odbc-core`; recorded here because the wrong result
  sets were this driver's.
- **`SQL_ATTR_PARAMS_PROCESSED_PTR` and `SQL_ATTR_PARAM_STATUS_PTR` were stored
  and never written.** `SQLExecDirect`, `SQLExecute` and the `SQLParamData`
  completion now report the processed count (`1`, since `SQL_ATTR_PARAMSET_SIZE`
  is pinned there) and set the first status element to `SQL_PARAM_SUCCESS` or
  `SQL_PARAM_ERROR`. An application that binds a status array to detect per-set
  errors previously read back its own initial buffer contents, which is
  indistinguishable from every set having succeeded.
- **Nine statement attributes `SQLSetStmtAttr` accepted could not be read
  back.** `SQL_ATTR_KEYSET_SIZE`, `SQL_ATTR_PARAM_BIND_TYPE`,
  `SQL_ATTR_PARAMS_PROCESSED_PTR`, `SQL_ATTR_PARAM_STATUS_PTR`,
  `SQL_ATTR_PARAM_BIND_OFFSET_PTR`, `SQL_ATTR_PARAM_OPERATION_PTR`,
  `SQL_ATTR_ROW_OPERATION_PTR`, `SQL_ATTR_FETCH_BOOKMARK_PTR` and
  `SQL_ATTR_ASYNC_STMT_EVENT` answered `HYC00` from `SQLGetStmtAttr` after
  being accepted by the setter.
- `SQLTables`, `SQLColumns` and `SQLTablePrivileges` reached only the connected
  catalog. Trino resolves a bare `information_schema` through the session
  catalog, and each catalog's copy describes only itself, so naming any other
  catalog matched nothing however the filter was written — an application that
  enumerated catalogs and then asked each one for its tables found every
  catalog but its own empty. The three now qualify the reference with the
  requested catalog. A catalog that does not exist is an empty result set
  rather than an error, which is what a filter naming something absent means.
- `SQLColumns` reported the *concise* type in `SQL_DATA_TYPE` for datetime
  columns, contradicting `SQLGetTypeInfo`, which has always reported the
  verbose one. A `DATE` column came back as `SQL_DATA_TYPE` 91 with a NULL
  `SQL_DATETIME_SUB`, where the spec has that column carry `SQL_DATETIME` (9)
  with the subcode in `SQL_DATETIME_SUB` — 1, 2 and 3 for date, time and
  timestamp. Trino's interval types are unaffected: they map to `SQL_WVARCHAR`
  rather than to an ODBC interval type, so they report no subcode.
- The Power Query connector emitted `LIMIT x OFFSET y`, which Trino rejects
  outright — its grammar is `OFFSET count LIMIT count`, in that order. Only a
  fold carrying both a skip and a take produced the pair, so take-only folding
  was unaffected and this went unnoticed.
- The connector's `Constant` visitor was keyed on PostgreSQL type names
  inherited from the reference connector. Power Query looks each field up by
  the driver's own `SQLGetTypeInfo` `TYPE_NAME`, so `TEXT`, `TIMESTAMPTZ` and
  `TIMETZ` never matched and string, timestamp and time constants folded
  through none of them; `NUMERIC` and `FLOAT` named types Trino does not have.
  Re-keyed to `VARCHAR`, `TIMESTAMP` and `TIME`, and the two unusable entries
  removed.
- A trailing statement terminator no longer fails the statement. Trino's REST
  API takes one statement per request and its grammar has no terminator, so
  `SELECT 1;` was rejected with `SYNTAX_ERROR` at the semicolon — every query
  from a tool that appends one, `isql` included, failed. The driver now strips
  the trailing run before submitting.

  Only the trailing run, after trailing whitespace: a statement whose last token
  is a string literal or quoted identifier ends with the closing quote, so a
  semicolon inside one is never the final character and is left intact. An
  embedded semicolon is also left alone, and Trino still rejects it — correctly,
  since it accepts only one statement per request. A comment after the
  terminator (`SELECT 1; -- done`) is not recognised.
- `NaN`, `Infinity` and `-Infinity` in a `DOUBLE` or `REAL` column are now
  readable. JSON has no literal for the IEEE specials, so Trino sends them as
  strings; the conversion had no arm for a string-valued float column, so they
  fell through as text and core then refused `String -> Double` with `22018`.
  An application reading the column as text fared no better: the value arrived
  as `"NaN"` with the JSON quote characters still attached.

  The quoting was not specific to floats. Every fallback in
  `json_to_column_value` rendered its value with `Value::to_string()`, which
  re-encodes a string as JSON, so any value that failed to convert reached the
  application with two quote marks it never sent. Those paths now yield the
  text Trino actually sent.
- `{fn CONVERT(value, SQL_type)}` is now translated to `CAST(value AS type)`.
  `SQL_CONVERT_FUNCTIONS` has always reported `SQL_FN_CVT_CAST`, advertising the
  escape, but nothing translated it: the ODBC type keyword reached Trino as a
  bare identifier, so `SELECT {fn CONVERT('1', SQL_INTEGER)}` failed with
  `COLUMN_NOT_FOUND` on `sql_integer`. All 26 ODBC type keywords with a Trino
  equivalent are mapped and exercised against a live coordinator.

  `SQL_CHAR` maps to `VARCHAR` rather than to Trino's `CHAR`, which is `CHAR(1)`
  when written without a length — `CAST('hello world' AS CHAR)` returns `"h"`,
  and `{fn CONVERT}` carries no length to give `CHAR(n)` instead. The
  `SQL_INTERVAL_*` keywords stay untranslated, since no bare `CAST` reaches
  Trino's interval types; the escape is left alone rather than cast to something
  the application did not ask for.

- The Power Query connector no longer overrides `SQL_SQL92_PREDICATES` and
  `SQL_SQL92_RELATIONAL_JOIN_OPERATORS` with every bit set. The driver gates
  both on the coordinator's version — `MATCH` and `UNIQUE` arrived in Trino
  482, `OVERLAPS` in 483, `CORRESPONDING` in 475 — and the flat overrides
  re-asserted all of them on every server, so Power BI could fold a predicate
  the coordinator then rejected. The join override also claimed `NATURAL JOIN`,
  which Trino rejects with `NOT_SUPPORTED`, and `UNION JOIN`, which has no
  production in its grammar at any version. `SQL_AGGREGATE_FUNCTIONS`,
  `SQL_SQL92_VALUE_EXPRESSIONS` and `SQL_IDENTIFIER_QUOTE_CHAR` are no longer
  overridden either; the driver reports exactly those values, and an override
  wins over the driver silently, so keeping them only invited drift.
  `SQL_SQL_CONFORMANCE` is still overridden to `SQL_SC_SQL92_FULL`, which is
  Microsoft's documented guidance for Power Query connectors and is what
  `SupportsDerivedTable` keys off.
- `SQL_MULT_RESULT_SETS`, `SQL_NEED_LONG_DATA_LEN` and
  `SQL_MAX_ROW_SIZE_INCLUDES_LONG` now report `"N"` instead of the empty
  string, which is not one of the two values the spec defines for any of them.
- `SQL_NULL_COLLATION` now reports `SQL_NC_END` instead of `SQL_NC_HIGH`.
  Trino's default null ordering is `NULLS LAST` regardless of the ordering
  direction, which is what `SQL_NC_END` means; `SQL_NC_HIGH` told applications
  the position follows `ASC`/`DESC`.
- `SQL_GROUP_BY` now reports `SQL_GB_GROUP_BY_CONTAINS_SELECT` instead of
  `SQL_GB_NO_RELATION`. Trino requires every non-aggregated column in the
  select list to appear in `GROUP BY`, and `SQL_GB_NO_RELATION` told
  applications it did not.
- `SQL_DEFAULT_TXN_ISOLATION` now reports `0` instead of
  `SQL_TXN_READ_COMMITTED`. The spec defines `0` as the value for a data source
  that does not support transactions, and this driver reports `SQL_TC_NONE`;
  the previous value also named a level absent from the driver's own
  `SQL_TXN_ISOLATION_OPTION` of `0`.
- `SQL_CORRELATION_NAME` now reports `SQL_CN_ANY` and `SQL_NON_NULLABLE_COLUMNS`
  reports `SQL_NNC_NON_NULL`, instead of the `0` (`SQL_CN_NONE` /
  `SQL_NNC_NULL`) they defaulted to. Trino accepts `FROM ... AS x(a)` and
  `ADD COLUMN f integer NOT NULL`.
- `SQL_EXPRESSIONS_IN_ORDERBY` now reports `"Y"` instead of the empty string,
  which is not one of its two spec-defined values. Trino accepts
  `ORDER BY lower(s)`.
- `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` now rejects every isolation level
  with `HY024` rather than accepting and silently discarding it, and
  `SQLGetConnectAttr` reports the same `0` as `SQL_DEFAULT_TXN_ISOLATION`
  instead of a hard-coded `SQL_TXN_READ_COMMITTED`.
- `windows/WINDOWS.md` documented the connection string as accepting neither
  `AccessToken` / `Token` nor `QueryTimeout` / `LoginTimeout`, all four of which
  the driver has always accepted.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-trino/commits/HEAD
