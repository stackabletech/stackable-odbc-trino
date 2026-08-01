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
| `StackableTrinoODBC*.png` | Yes | Icons, the Stackable mark at eight sizes. See below |
| `StackableTrinoODBC.query.pq` | No | Test query for the Power Query SDK debugger |
| `build.sh` | No | Builds the .mez on Linux |
| `.vscode/settings.json` | No | VS Code workspace config for the Power Query SDK |

### Icons

The eight PNGs are the Stackable mark, rendered from
`.readme/static/borrowed/Icon_Stackable.svg`. Power BI shows them in the
**Get Data** list, and the `.pq` groups them into `Icon16` = {16, 20, 24, 32}
and `Icon32` = {32, 40, 48, 64}, from which Power BI picks by display scaling.
So the 16 is only ever seen at 100% DPI.

16, 20 and 24 are rendered edge to edge and the rest with 9% padding: the mark
has six horizontal bands, which at 16px is under three pixels each, so the small
sizes need every pixel they can get. They are soft at 16 regardless, which is
accepted rather than solved with a simplified variant.

**The Stackable mark rather than Trino's.** Naming the connector "Stackable
Trino" is nominative use of a trademark and is fine; shipping the Trino Software
Foundation's logo as this product's tile would imply an endorsement nobody gave.

To regenerate after a brand asset changes (needs no system packages, `uv`
fetches the renderer):

```bash
uv run --with cairosvg python3 -c '
import cairosvg
src = open(".readme/static/borrowed/Icon_Stackable.svg").read()
body = src.split(">", 1)[1].rsplit("</svg>", 1)[0]
for size in (16, 20, 24, 32, 40, 48, 64, 80):
    pad = 0.0 if size <= 24 else 0.09
    inner = size * (1 - 2 * pad)
    scale = min(inner / 507.97, inner / 517.33)
    dx, dy = (size - 507.97 * scale) / 2, (size - 517.33 * scale) / 2
    cairosvg.svg2png(
        bytestring=(
            f"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" "
            f"height=\"{size}\" viewBox=\"0 0 {size} {size}\">"
            f"<g transform=\"translate({dx},{dy}) scale({scale})\">{body}</g></svg>"
        ).encode(),
        write_to=f"connector/StackableTrinoODBC{size}.png",
        output_width=size, output_height=size,
    )
'
```

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
| `Config_AdvancedOptions` | see the file | Connection-string keys `StackableTrinoODBC.Contents` accepts in its `options` record |
| `Config_SqlConformance` | `SQL_SC_SQL92_FULL` (8) | Overrides driver's reported conformance |
| `Config_LimitClauseKind` | `LimitClauseKind.LimitOffset` | SQL syntax for row limits |
| `Config_UseParameterBindings` | `true` | Bind parameters rather than inlining literals |
| `Config_StringLiteralEscapeCharacters` | `{ "'" }` | How a literal quote is escaped in generated SQL |
| `Config_UseCastInsteadOfConvert` | `true` | Trino uses CAST, not CONVERT |
| `Config_EnableDirectQuery` | `true` | Enables DirectQuery mode in PowerBI |

`Config_AdvancedOptions` deliberately omits the four keys the driver declares
sensitive (`AccessToken`, `ExtraCredentials`, `ExtraHeaders`, `ProxyPassword`):
an option set here is stored in the query text inside the `.pbix`, which is a
file people mail to each other. `connector_options_are_connection_string_keys`
in `src/lib.rs` checks the list against the driver's own `PARAM_` constants, and
`connector_version_matches_the_crate` checks the `[Version = "..."]` at the top
of the `.pq` against `Cargo.toml`, so neither can drift. Do not edit the version
by hand; `release.toml` rewrites it during a release.

### Verifying query folding

Automatically, on Linux, against a running Trino:

```bash
uv run --with pyodbc python3 integration-tests/suites/test_folding_contract.py "<connection-string>"
```

This is the only thing that reads the `.pq` outside Power BI. It parses the
connector rather than transcribing it, and checks that every `Constant` visitor
field name is a real driver `TYPE_NAME`, that every CAST target is a type Trino
has, and that the row-limiting clause the `AstVisitor` builds actually runs
(Trino's grammar is `OFFSET count LIMIT count` and rejects the reverse). Without
it, a connector declaration can drift from what the driver reports or what Trino
accepts with nothing noticing.

By hand, in PowerBI Desktop, after connecting:

1. Open the Power Query Editor (Transform Data)
2. Add a filter, sort, or aggregation step
3. Right-click the step → **View Native Query**
4. If greyed out, the step didn't fold — check the connector configuration

You can also enable ODBC tracing on Windows to see the exact SQL sent to
the driver.
