<!-- markdownlint-disable MD041 MD033 -->

<p align="center">
  <img width="150" src="./.readme/static/borrowed/Icon_Stackable.svg" alt="Stackable Logo"/>
</p>

<h1 align="center">Stackable ODBC Driver for Trino</h1>

<p align="center"><em>Plug Power BI, Excel, Tableau or Python straight into Trino.</em></p>

[![Build and Test](https://github.com/stackabletech/stackable-odbc-trino/actions/workflows/build.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-trino/actions/workflows/build.yaml)
[![Security Audit](https://github.com/stackabletech/stackable-odbc-trino/actions/workflows/security_audit.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-trino/actions/workflows/security_audit.yaml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/stackabletech/stackable-odbc-trino/badge)](https://scorecard.dev/viewer/?uri=github.com/stackabletech/stackable-odbc-trino)
[![Apache License 2.0](https://img.shields.io/badge/license-Apache--2.0-green)](./LICENSE)
[![ODBC 3.80](https://img.shields.io/badge/ODBC-3.80-blue)](#what-it-deliberately-does-not-do)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#quick-start)
[![Trino](https://img.shields.io/badge/Trino-compatible-blue)](https://trino.io)
[![Power BI](https://img.shields.io/badge/Power%20BI-connector%20included-blue)](#power-bi)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

## What is this?

[Trino](https://trino.io) is a query engine that runs SQL across many different
systems at once. One query can join a table in PostgreSQL against files in S3
and a Kafka topic, and Trino makes all of it look like one database.

Most of the tools people actually build dashboards in do not know how to talk to
Trino. They know how to talk to **ODBC**: a standard, decades old, that every
desktop analytics tool speaks. A tool loads a small library called a *driver*,
calls the standard functions, and the driver turns them into whatever that
particular database understands.

This repository is that driver for Trino. Install it, and Power BI, Excel,
Tableau, DBeaver, `isql` and Python's `pyodbc` can all query Trino as if it were
any ordinary database. Linux and Windows are both first-class targets.

## Quick start

Grab an archive from the
[releases page](https://github.com/stackabletech/stackable-odbc-trino/releases).

### Windows

1. Unzip `stackable-odbc-trino-<version>-windows-x64.zip`.
2. Right-click `install.bat` and choose **Run as administrator**. This tells
   Windows the driver exists.
3. Open **ODBC Data Sources (64-bit)** from the Start menu, click **Add**, and
   pick **Stackable Trino ODBC**. Fill in your coordinator's hostname, port and
   login, then click **OK**.

Step 3 creates a *DSN*, which is just a saved connection with a name, like a
browser bookmark. Once it exists, every tool on the machine can pick it from a
list instead of asking you to type a connection string.

The archive also ships `configure-dsn.ps1`, a standalone dialog covering every
option below, if you would rather script it or want the full surface in one
window.

### Linux

You need unixODBC (the `unixodbc` package), and root, because the driver has to
be registered system-wide.

```bash
mkdir /tmp/trino-odbc
tar xzf stackable-odbc-trino-<version>-linux-x64.tar.gz -C /tmp/trino-odbc
cd /tmp/trino-odbc
sudo ./install.sh
```

Check it worked with `odbcinst -q -d`. You should see `[stackable_odbc_trino]`
in the output.

### Power BI

Both archives contain `StackableTrinoODBC.mez`, a Power Query custom connector,
which is also published on its own. It gives Trino a proper entry in the
**Get Data** dialog instead of the generic ODBC one.

1. Copy the `.mez` into `%USERPROFILE%\Documents\Power BI Desktop\Custom Connectors\`.
2. In **File > Options > Security**, allow any extension to load.
3. Restart Power BI Desktop. Trino now appears under **Get Data**.

### Then use it

```python
import pyodbc

conn = pyodbc.connect(
    "Driver=stackable_odbc_trino;Host=trino.example.com;Port=8443;"
    "User=me;Password=secret"
)
for row in conn.cursor().execute("SELECT name FROM tpch.tiny.nation LIMIT 5"):
    print(row.name)
```

For the full install and uninstall reference see
[`packaging/README.md`](packaging/README.md), and for managing drivers and DSNs
by hand on Windows see
[`integration-tests/windows/WINDOWS.md`](integration-tests/windows/WINDOWS.md).

## Highlights

- **Sign in the way your company already does.** Username and password, a bearer
  token, a client certificate, or a real browser login through Trino's OAuth 2.0
  flow: the driver shows you a URL, you log in with your normal company account,
  and it picks up the token when you are done. That login is shared across the
  whole application, so a tool opening ten connections at startup opens one
  browser tab, not ten.

- **Run queries as somebody else, on purpose.** `SessionUser` authenticates as
  you but runs the SQL under another user's name, which is how a shared service
  keeps per-user permissions. `Roles` picks the authorisation role per catalog,
  which Hive and Iceberg need before they will let you write anything.

- **Encryption has a middle setting, not just on and off.** Most drivers make
  you choose between full verification and none, so one coordinator reached
  under an internal hostname ends up with checking switched off everywhere.
  Here `TlsVerify=ca` still verifies the certificate against your CA and only
  skips the hostname match. Turning verification off entirely stays possible,
  and stays a deliberate choice.

- **The stop button actually stops the query.** Cancelling from your tool tells
  the coordinator to kill the query, so it stops burning cluster time rather
  than running to completion while nobody is listening. The query timeout is
  just as literal: Trino sends the column names almost immediately and the rows
  much later, so a timer that only covers sending the query would never fire.
  This one keeps running while the rows arrive.

- **Big results can skip the coordinator.** Setting `Encoding=json+zstd` turns
  on Trino's spooling protocol, where large results travel through object
  storage instead of being streamed through the coordinator one page at a time.
  It is off by default because not every laptop can reach that storage, and a
  coordinator that does not support it just answers normally, so switching it on
  can never break a connection that otherwise worked.

- **Your tool can browse the data.** Catalogs, schemas, tables, columns, types
  and privileges all show up in the object browser, so you can click through
  what is there instead of guessing table names. When a query has parameters in
  it, the driver asks Trino what type each one is with `DESCRIBE INPUT` rather
  than guessing, which is why a filter on a `decimal` column keeps working.

- **Real transactions.** Turn autocommit off and the driver opens a Trino
  transaction on your next statement, then commits or rolls back when you say
  so. If a statement inside the transaction fails, Trino abandons the whole
  thing, and the driver rolls back and tells you the commit did not happen
  instead of reporting a success that threw your writes away.

- **Power BI does the work in Trino, not on your laptop.** The bundled connector
  folds filters, joins, grouping and row limits down into the SQL it sends, so a
  report over a billion-row table asks Trino for the answer instead of dragging
  the table across the network first. DirectQuery is supported, and a contract
  test checks the connector's declarations against what this driver reports and
  what Trino really accepts.

- **Windows is a first-class target.** It gets its own installer and a proper
  dialog for setting up a saved connection, and the test suite is run through
  the Windows Driver Manager in a VM before every release, not only through
  Linux's. Windows' Driver Manager is much stricter than unixODBC and tends to
  fail silently, so this is measured rather than assumed.

## Connecting

Connection strings are `Key=Value` pairs joined by `;`. Keys are
case-insensitive.

```text
Driver=stackable_odbc_trino;Host=trino.example.com;Port=8443;User=admin;Password=secret;Catalog=hive;Schema=default
```

The keys most people need:

| Key | Required | Meaning |
|-----|----------|---------|
| `Host` | Yes | Trino coordinator hostname |
| `Port` | Yes | Coordinator port |
| `User` | Yes¹ | Username. ¹Optional under `ExternalAuthentication`, where the login supplies it |
| `Password` | No | Password |
| `Catalog` | No | Catalog to start in |
| `Schema` | No | Schema to start in |
| `TlsVerify` | No | `true`/`full` (default), `ca`, or `false`/`none` |
| `ExternalAuthentication` | No | `true` for the browser login |

<details>
<summary><strong>All connection options</strong> (click to expand)</summary>

The authoritative list is `src/backend/types/connect_params.rs`.

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
| `ExternalAuthentication` | No | `true` selects Trino's interactive OAuth 2.0 flow. Needs `https`, and excludes `Password` and `AccessToken` |
| `ExternalAuthenticationTimeout` | No | Budget for one interactive login, in seconds. Default 300 |
| `QueryTimeout` | No | Per-request HTTP timeout in seconds (default 30). Alias: `LoginTimeout` |
| `Encoding` | No | Trino's spooled query-data encoding: `json`, `json+zstd` or `json+lz4`. Unset returns every row inline. JDBC's `encoding` |
| `SessionProperties` | No | Trino session properties, `{name:value;name2:value2}` |
| `ResourceEstimates` | No | Scheduling hints, same form |
| `ExtraCredentials` | No | Connector-level credentials, same form |
| `Roles` | No | Authorisation role per catalog, `{catalog:role;catalog2:ALL}` |
| `SessionUser` | No | User statements run as, while `User` still authenticates. JDBC's `sessionUser` |
| `Path` | No | Default SQL path for resolving unqualified function names |
| `TimeZone` | No | IANA session time zone (`Europe/Berlin`). Unset leaves the coordinator's |
| `Locale` | No | Locale for locale-dependent formatting, sent as `X-Trino-Language` |
| `ClientInfo` | No | Free-form client metadata Trino records against the query |
| `TraceToken` | No | Correlation token Trino records against the query |
| `ExtraHeaders` | No | Extra HTTP headers, `{name:value;name2:value2}` |
| `ClientCapabilities` | No | Comma-separated extra capabilities, on top of `PARAMETRIC_DATETIME` and `PATH` |
| `Proxy` | No | HTTP/HTTPS proxy URL for every request. Credentials in the URL are rejected |
| `ProxyUser` | No | Proxy Basic username. Requires `ProxyPassword` |
| `ProxyPassword` | No | Proxy Basic password |
| `DisableCompression` | No | `true` or `false` (default) |
| `MaxAttempts` | No | Request retry budget. Unset leaves the client's own |

</details>

### One gotcha worth knowing

Five keys take a list of pairs: `SessionProperties`, `ResourceEstimates`,
`ExtraCredentials`, `Roles` and `ExtraHeaders`. They use JDBC's format exactly,
so a value copied out of a JDBC URL works unchanged. That format separates pairs
with `;`, which is also what separates one connection-string key from the next,
so those values have to be wrapped in braces:

```text
SessionProperties={query_max_run_time:10m;example.foo:bar};Encoding=json+zstd
```

Leave the braces off and the connection string ends the value at the first `;`,
silently throwing away every pair but the first.

In a DSN it is the other way round: braces are connection-string syntax, so the
value is stored bare. The Windows dialog handles that for you.

## What it deliberately does not do

Everything here is reported to the application as unsupported rather than
quietly ignored, so a tool can react instead of trusting a wrong answer.

- **No primary keys, foreign keys, indexes or stored procedures.** Trino
  publishes no metadata for any of them, so those lookups return nothing.
- **The current catalog cannot be changed after connecting.** Trino's `USE`
  moves the catalog and the schema together, so honouring "switch to catalog X"
  would mean inventing a schema and leaving your unqualified table names
  resolving somewhere you never asked for. Set `Catalog` when connecting.
- **Row and field size limits are not faked.** Trino can only limit a result set
  through `LIMIT` in the SQL you wrote, and the standard forbids a driver from
  emulating those settings by throwing rows away after they arrive.
- **One isolation level.** Catalogs disagree about which levels they accept, so
  the driver advertises only the one every catalog supports and refuses the rest
  up front rather than letting the query fail later for a reason nobody can see.

## Building from source

Building or testing needs the unixODBC development libraries, because the ODBC
bindings link against them. You do not need a running Trino, or any ODBC setup:

```bash
sudo apt-get install unixodbc-dev   # Debian/Ubuntu
```

Everything generic about being an ODBC driver lives in
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core),
which this repository builds on. It is not published yet, so clone it as a
sibling directory:

```bash
git clone https://github.com/stackabletech/stackable-odbc-core
git clone https://github.com/stackabletech/stackable-odbc-trino
cd stackable-odbc-trino
cargo build --release
```

Output: `target/release/libstackable_odbc_trino.so`

For Windows, cross-compile with MinGW (`gcc-mingw-w64-x86-64`):

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll`

To assemble the release archives, see [`packaging/README.md`](packaging/README.md).

## Testing

```bash
cargo test    # unit and FFI tests; needs no running Trino
cargo bench   # Criterion fetch-throughput benchmark; needs TRINO_BENCH_URL
```

The integration suite runs against a real Trino in Docker. It is **not** run in
CI, because the Trino and Postgres compose stack is bigger than a standard
GitHub runner, so run it locally before a release:

```bash
./integration-tests/setup.sh       # spin up Trino, build the driver, write ODBC config
./integration-tests/run-tests.sh   # run the tests, then tear Trino down
```

The Windows tests run the same suites through the Windows Driver Manager in a
VM; see
[`integration-tests/windows/WINDOWS.md`](integration-tests/windows/WINDOWS.md).

For architecture, conventions and the full testing reference, see
[AGENTS.md](AGENTS.md).

## License

Apache-2.0
