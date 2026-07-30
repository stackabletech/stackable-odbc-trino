# Trino ODBC — Power Query Custom Connector

A Power Query custom connector (.mez) that enables PowerBI Desktop to
connect to Trino via the Stackable ODBC driver, including DirectQuery
support.

## What it does

The connector tells PowerBI how to generate Trino-compatible SQL:

- **LIMIT/OFFSET** instead of TOP N
- **CAST** instead of CONVERT
- **Double-quote identifiers** instead of square brackets
- Reports full SQL-92 conformance so PowerBI enables query folding for
  filters, sorts, aggregations, and joins

## Prerequisites

- The `stackable_odbc_trino` ODBC driver must be registered on the
  Windows machine (see `integration-tests/windows/WINDOWS.md` for driver registration)
- PowerBI Desktop (Windows only)

## Building

On Linux (or anywhere with `zip`):

```bash
./connector/build.sh
```

Output: `connector/bin/StackableTrinoODBC.mez`

On Windows with the Power Query SDK (VS Code extension):

1. Open the `connector/` folder in VS Code
2. Install the "Power Query SDK" extension
3. Ctrl+Shift+B → "MakePQX"

## Installing in PowerBI Desktop

1. Copy `StackableTrinoODBC.mez` to
   `%USERPROFILE%\Documents\Power BI Desktop\Custom Connectors\`
   (create the folder if it doesn't exist)
2. Open PowerBI Desktop → File → Options → Security → Data Extensions →
   select "Allow any extension to load without validation or warning"
3. Restart PowerBI Desktop
4. Get Data → More → Database → **Stackable Trino**

## Developing

### Project structure

| File | Included in .mez | Purpose |
|------|------------------|---------|
| `StackableTrinoODBC.pq` | Yes | Main connector logic (M language) |
| `Diagnostics.pqm` | Yes | Trace logging helper (from Microsoft sample) |
| `OdbcConstants.pqm` | Yes | ODBC constants translated to M (from Microsoft sample) |
| `resources.resx` | Yes | UI strings (button text, labels) |
| `StackableTrinoODBC*.png` | Yes | Icons — currently placeholders, replace with Trino branding |
| `StackableTrinoODBC.query.pq` | No | Test query for the Power Query SDK debugger |
| `build.sh` | No | Builds the .mez on Linux |
| `.vscode/settings.json` | No | VS Code workspace config for the Power Query SDK |

### Testing with the Power Query SDK

1. Open `connector/` in VS Code with the Power Query SDK extension
2. Open `StackableTrinoODBC.query.pq`
3. Ctrl+Shift+Alt+E to evaluate — this runs the test query against a
   local Trino instance without needing to install the .mez

### Configuration

Key settings in `StackableTrinoODBC.pq` (top of file):

| Setting | Default | Purpose |
|---------|---------|---------|
| `Config_DriverName` | `stackable_odbc_trino` | ODBC driver name as registered |
| `Config_SqlConformance` | `SQL_SC_SQL92_FULL` (8) | Overrides driver's reported conformance |
| `Config_LimitClauseKind` | `LimitClauseKind.LimitOffset` | SQL syntax for row limits |
| `Config_UseParameterBindings` | `true` | Bind parameters rather than inlining literals |
| `Config_UseCastInsteadOfConvert` | `true` | Trino uses CAST, not CONVERT |
| `Config_EnableDirectQuery` | `true` | Enables DirectQuery mode in PowerBI |

### Verifying query folding

In PowerBI Desktop, after connecting:

1. Open the Power Query Editor (Transform Data)
2. Add a filter, sort, or aggregation step
3. Right-click the step → **View Native Query**
4. If greyed out, the step didn't fold — check the connector configuration

You can also enable ODBC tracing on Windows to see the exact SQL sent to
the driver.
