# Stackable Trino, Power Query Custom Connector

A Power Query custom connector (`.mez`) that lets Power BI Desktop query Trino
through the Stackable ODBC driver, DirectQuery included.

## What it does

Power BI can already reach any ODBC driver through its generic ODBC source.
This connector gives Trino its own entry in the **Get Data** dialog and tells
Power BI how to generate SQL that Trino accepts:

- **LIMIT/OFFSET** instead of TOP N
- **CAST** instead of CONVERT
- **Double-quote identifiers** instead of square brackets
- Reports full SQL-92 conformance, so Power BI folds filters, sorts,
  aggregations and joins into the query it sends rather than computing them
  locally

## Prerequisites

- Power BI Desktop, which is Windows only
- The `stackable_odbc_trino` ODBC driver registered on the same machine. See
  [`packaging/README.md`](../packaging/README.md#windows-x86_64).

## Installing

The `.mez` ships in the Windows release archive and is also published on its
own. For the install steps see
[`packaging/README.md`](../packaging/README.md#power-bi-custom-connector-windows-only).

## Building

On Linux, or anywhere with `zip`:

```bash
./connector/build.sh
```

Output: `connector/bin/StackableTrinoODBC.mez`

On Windows with the Power Query SDK (a VS Code extension):

1. Open the `connector/` folder in VS Code
2. Install the "Power Query SDK" extension
3. Ctrl+Shift+B → "MakePQX"

## Developing

[`DEVELOPING.md`](DEVELOPING.md) covers the project layout, the connector's
configuration settings, regenerating the icons, and how to verify query
folding.
