# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

A `TIME WITH TIME ZONE` value whose offset or hour field lies far outside a real
clock is now kept as text instead of being converted with wrapped arithmetic.
Both fields arrive as free-form text, so a number that no zone or clock could
hold still parses as an `i32`, and the conversion to minutes overflowed. Release
builds carry no overflow checks, so the driver reported a different time rather
than declining the value.

### Added

Fuzz targets for this driver's own parsers, in `fuzz/`: the JSON-to-value read
path, the Trino type-signature parsers, ODBC escape translation under the Trino
dialect, and the connection-string value parsing. They run as an
AddressSanitizer smoke test in CI. See `fuzz/README.md`.

## [0.1.0] — 2026-08-04

First release, so this section describes what the driver offers rather than
what changed.

### Added

**Querying.** An ODBC 3.80 driver for [Trino](https://trino.io) on Linux and
Windows. Queries, result sets fetched a row or an array at a time, bound
parameters singly and in batches, and the ODBC escape sequences `{fn ...}`,
`{d ...}` and `{oj ...}` translated into Trino SQL. Trino's types map to their
SQL equivalents, the parametric decimal, timestamp and interval types included.

**Metadata.** Catalogs, schemas, tables, columns, table types and table
privileges, so a tool can browse the data source instead of asking you to type
table names. `SQLDescribeParam` answers from Trino's `DESCRIBE INPUT` instead of
guessing, so a filter on a `decimal` column keeps its type.

**Authentication.** Username and password, a bearer token, a client certificate
for mutual TLS, and Trino's interactive OAuth 2.0 flow, which opens a browser
and picks up the token once the login completes. One login is shared across the
process, so an application opening ten connections opens one browser tab.
`SessionUser` runs statements as another user while you authenticate as
yourself, and `Roles` selects the authorisation role per catalog.

**TLS.** Three verification modes: full, certificate chain only, and none. The
middle one verifies against your CA while skipping the hostname check, which is
what a coordinator reached under an internal name needs.

**Transactions.** Turn autocommit off and the driver opens a Trino transaction
on the next statement, then commits or rolls back on request. A statement that
fails aborts the transaction, and the driver rolls back and reports that the
commit did not happen.

**Cancellation and timeouts.** `SQLCancel` from another thread asks the
coordinator to kill the query, so it stops consuming cluster time.
`SQL_ATTR_QUERY_TIMEOUT` covers fetching as well as execution, which is where a
Trino query spends its time.

**Large results.** Setting `Encoding` turns on Trino's spooling protocol. It is
off by default, and a coordinator that does not support it answers normally, so
enabling it cannot break a connection that already worked.

**Connection options.** 34 connection-string keys covering session properties,
resource estimates, extra credentials, client tags, proxies, time zone and
locale. Keys shared with Trino's JDBC driver take the same format, so a value
copied out of a JDBC URL works unchanged.

**Packaging.** Installers for Linux and Windows, a Windows dialog for creating a
data source, and a Power Query custom connector for Power BI that folds filters,
joins, grouping and row limits into the SQL it sends. Every release artifact
ships with a CycloneDX SBOM, is published alongside an SPDX document, and is
covered by `sha256sums.txt`.

### Known limitations

- Trino publishes no primary keys, foreign keys, indexes or stored procedures,
  so those lookups return no rows.
- The current catalog cannot be changed after connecting. Set `Catalog` in the
  connection string instead.
- `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_MAX_LENGTH` are reported as unsupported
  rather than emulated.
- Only the `READ UNCOMMITTED` isolation level is offered, because it is the one
  every Trino catalog accepts.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-trino/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/stackabletech/stackable-odbc-trino/releases/tag/v0.1.0
