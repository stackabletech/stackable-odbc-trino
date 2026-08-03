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
[![ODBC 3.80](https://img.shields.io/badge/ODBC-3.80-blue)](#compatibility)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#quick-start)
[![Trino](https://img.shields.io/badge/Trino-compatible-blue)](https://trino.io)
[![Power BI](https://img.shields.io/badge/Power%20BI-connector%20included-blue)](#power-bi)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

## What is this?

[Trino](https://trino.io) runs SQL across many systems at once, so one query can
join a table in PostgreSQL against files in S3 and a Kafka topic. Most desktop
analytics tools cannot talk to Trino directly, but nearly all of them speak
ODBC.

This is the ODBC driver for Trino. Install it, and Power BI, Excel, Tableau,
DBeaver, `isql` and Python's `pyodbc` can query Trino like any other database.
Linux and Windows are both first-class targets.

## Quick start

Download an archive from the
[releases page](https://github.com/stackabletech/stackable-odbc-trino/releases).

### Windows

1. Unzip `stackable-odbc-trino-<version>-windows-x64.zip`.
2. Right-click `install.bat` and choose **Run as administrator**. This registers
   the driver with Windows.
3. Open **ODBC Data Sources (64-bit)** from the Start menu, click **Add**, and
   pick `stackable_odbc_trino` from the list. Fill in your coordinator's
   hostname, port and login, then click **OK**.

Step 3 creates a *DSN*: a saved connection with a name. Once it exists, every
tool on the machine can pick it from a list instead of asking you to type a
connection string.

The archive also ships `configure-dsn.ps1`, a standalone dialog covering every
option below, if you would rather script the setup or see the full surface in
one window.

### Linux

You need unixODBC (the `unixodbc` package). Installing the driver registers it
system-wide, so it needs root.

```bash
mkdir /tmp/trino-odbc
tar xzf stackable-odbc-trino-<version>-linux-x64.tar.gz -C /tmp/trino-odbc
cd /tmp/trino-odbc
sudo ./install.sh
```

Check it worked with `odbcinst -q -d`, which should list
`[stackable_odbc_trino]`.

### Power BI

Both archives contain `StackableTrinoODBC.mez`, a Power Query custom connector,
which is also published on its own. It gives Trino a proper entry in the
**Get Data** dialog instead of the generic ODBC one.

1. Copy the `.mez` into `%USERPROFILE%\Documents\Power BI Desktop\Custom Connectors\`.
2. In **File > Options > Security**, allow any extension to load.
3. Restart Power BI Desktop. **Stackable Trino** now appears under **Get Data**.

### Your first query

```python
import pyodbc

conn = pyodbc.connect(
    "Driver=stackable_odbc_trino;Host=trino.example.com;Port=8443;"
    "User=me;Password=secret"
)
for row in conn.cursor().execute("SELECT name FROM tpch.tiny.nation LIMIT 5"):
    print(row.name)
```

For the full install and uninstall reference, see
[`packaging/README.md`](packaging/README.md).

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
| `User` | Yes¹ | Username. Alias: `UID`. ¹Optional under `ExternalAuthentication`, where the login supplies it |
| `Password` | No | Password. Alias: `PWD` |
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
| `User` | Yes¹ | Username (Basic Auth). Alias: `UID`. ¹Optional under `ExternalAuthentication`, where the identity provider supplies it |
| `Password` | No | Password (Basic Auth). Alias: `PWD` |
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

### Values that contain a semicolon

Five keys take a list of pairs: `SessionProperties`, `ResourceEstimates`,
`ExtraCredentials`, `Roles` and `ExtraHeaders`. They use JDBC's format exactly,
so a value copied out of a JDBC URL works unchanged. That format separates pairs
with `;`, which is also what separates one connection-string key from the next,
so wrap those values in braces:

```text
SessionProperties={query_max_run_time:10m;example.foo:bar};Encoding=json+zstd
```

Without the braces, the connection string ends the value at the first `;` and
silently discards every pair but the first.

In a DSN it is the other way round. Braces are connection-string syntax, so the
value is stored bare:

```text
SessionProperties=query_max_run_time:10m;example.foo:bar
```

Braces in a DSN fail the connection outright, so the mistake is at least loud in
that direction. The Windows dialog handles both cases for you.

## What you get

- **Sign in the way your company already does.** Username and password, a bearer
  token, a client certificate, or a browser login through Trino's OAuth 2.0
  flow. For the browser login the driver shows you a URL, you sign in with your
  normal account, and it picks up the token when you are done. That login is
  shared across the whole application, so a tool opening ten connections opens
  one browser tab.

- **Run queries as somebody else, on purpose.** `SessionUser` authenticates as
  you but runs the SQL under another user's name, which is how a shared service
  keeps per-user permissions. `Roles` picks the authorisation role per catalog,
  which Hive and Iceberg need before they will let you write anything.

- **Encryption has a middle setting.** Most drivers offer full verification or
  none, so one coordinator reached under an internal hostname ends up with
  checking switched off everywhere. `TlsVerify=ca` still verifies the
  certificate against your CA and only skips the hostname match.

- **The stop button stops the query.** Cancelling from your tool tells the
  coordinator to kill the query, so it stops consuming cluster time. Query
  timeouts work the same way, and cover the time spent receiving rows rather
  than only the time spent starting the query.

- **Your tool can browse the data.** Catalogs, schemas, tables, columns, types
  and privileges all show up in the object browser, so you can click through
  what is there instead of guessing table names.

- **Real transactions.** Turn autocommit off and the driver opens a Trino
  transaction on your next statement, then commits or rolls back when you say
  so. If a statement inside the transaction fails, Trino abandons the whole
  thing, and the driver rolls back and tells you the commit did not happen
  rather than reporting a success that threw your writes away.

- **Power BI does the work in Trino, not on your laptop.** The bundled connector
  pushes filters, joins, grouping and row limits down into the SQL it sends, so
  a report over a billion-row table asks Trino for the answer instead of
  dragging the table across the network first. DirectQuery is supported.

- **Big results can skip the coordinator.** Setting `Encoding=json+zstd` turns
  on Trino's spooling protocol, where large results travel through object
  storage instead of streaming through the coordinator a page at a time. It is
  off by default because not every machine can reach that storage, and a
  coordinator that does not support it answers normally, so switching it on
  cannot break a connection that already worked.

## Limits

Each of these is reported to your tool as unsupported rather than quietly
ignored, so the tool can react instead of trusting a wrong answer.

- **No primary keys, foreign keys, indexes or stored procedures.** Trino
  publishes no metadata for any of them, so those lookups return nothing.
- **The catalog cannot be changed after connecting.** Trino's `USE` moves the
  catalog and the schema together, so honouring "switch to catalog X" would mean
  inventing a schema, and your unqualified table names would start resolving
  somewhere you never asked for. Set `Catalog` when you connect.
- **Row and field size limits are not faked.** Trino can only limit a result set
  through `LIMIT` in the SQL you wrote.
- **One isolation level.** Trino catalogs disagree about which levels they
  accept, so the driver offers the one they all support and refuses the rest up
  front, rather than letting a query fail later for a reason nobody can see.

## Compatibility

| | |
|---|---|
| ODBC | 3.80 |
| Platforms | Linux x86-64, Windows x86-64 |
| Driver Managers | unixODBC, and the Windows Driver Manager |
| Trino | tested against 483 |
| Tested with | Power BI Desktop, `pyodbc`, `isql`, DBeaver |

Older Trino versions are likely to work, since the driver uses the stable REST
protocol, but 483 is what the test suite runs against.

## Troubleshooting

**Turn on logging first.** The driver logs to a file when you ask it to, and
that is usually enough to see what a tool is really sending:

```bash
export ODBC_LOG_LEVEL=debug
export ODBC_LOG_FILE=/tmp/trino-odbc.log
```

On Windows, set the same two as environment variables. The log may contain your
SQL, so check it before sharing.

**The driver does not appear in the list.** On Linux, run `odbcinst -q -d`; if
`[stackable_odbc_trino]` is missing, the install did not complete. On Windows,
make sure you opened **ODBC Data Sources (64-bit)**: a 64-bit driver is invisible
to the 32-bit Administrator, and both are in the Start menu under similar names.

**TLS errors.** The default verifies the certificate chain and the hostname. If
your coordinator's certificate does not carry the name you are connecting under,
use `TlsVerify=ca` with `Certificate` pointing at your CA's PEM file. That still
verifies the certificate and only relaxes the name check.

**Only the first session property applies.** Wrap the value in braces. See
[Values that contain a semicolon](#values-that-contain-a-semicolon).

**The browser login never opens.** Some tools, `pyodbc` among them, tell the
driver it may not display anything. The driver reports this rather than hanging.
Use `AccessToken` with those tools, or connect through one that allows a prompt.

## Getting help

- [GitHub Discussions](https://github.com/orgs/stackabletech/discussions) for
  questions
- [Discord](https://discord.gg/7kZ3BNnCAF) to talk to us
- [Issues](https://github.com/stackabletech/stackable-odbc-trino/issues) for
  bugs, and [SECURITY.md](SECURITY.md) for anything security-related

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for building from source, running the
tests, and how the repository is laid out. [CHANGELOG.md](CHANGELOG.md) records
what changed in each release.

## License

Apache-2.0
