# Developing the Power Query connector

Maintainer notes for `connector/`. To install or build the `.mez`, see
[`README.md`](README.md).

## Project structure

| File | Included in .mez | Purpose |
|------|------------------|---------|
| `StackableTrinoODBC.pq` | Yes | Main connector logic (M language) |
| `Diagnostics.pqm` | Yes | Trace logging helper (from Microsoft sample) |
| `OdbcConstants.pqm` | Yes | ODBC constants translated to M (from Microsoft sample) |
| `resources.resx` | Yes | UI strings (button text, labels) |
| `StackableTrinoODBC*.png` | Yes | Icons, the Stackable mark at seven sizes. See below |
| `StackableTrinoODBC.query.pq` | No | Test query for the Power Query SDK debugger |
| `build.sh` | No | Builds the .mez on Linux |

## Configuration

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
| `Config_EnableDirectQuery` | `true` | Enables DirectQuery mode in Power BI |

`Config_AdvancedOptions` omits the four keys the driver declares sensitive:
`AccessToken`, `ExtraCredentials`, `ExtraHeaders` and `ProxyPassword`. An option
set here is stored in the query text inside the `.pbix`, which is a file people
mail to each other.

Two `cargo test` checks keep the connector and the driver in step.
`connector_options_are_connection_string_keys` in `src/lib.rs` checks
`Config_AdvancedOptions` against the driver's own `PARAM_` constants.
`connector_version_matches_the_crate` checks the `[Version = "..."]` at the top
of the `.pq` against `Cargo.toml`. Do not edit that version by hand;
`release.toml` rewrites it during a release.

## Icons

The seven PNGs are the Stackable mark, rendered from
`.readme/static/borrowed/Icon_Stackable.svg`. Power BI shows them in the
**Get Data** list. The `.pq` groups them into `Icon16` = {16, 20, 24, 32} and
`Icon32` = {32, 40, 48, 64}, and Power BI picks a group by display scaling, so
the 16 is only ever seen at 100% DPI.

Those seven sizes are the whole set Power Query defines, and every connector in
Microsoft's `DataConnectors` samples ships exactly them. A size outside the two
groups is loaded by nothing: the icons are read with `Extension.Contents` from
inside the `.mez`, so they reach Power BI and nothing else. The ODBC Driver
Manager never sees them, and lists the driver from the `VERSIONINFO` resource
`build.rs` embeds in the DLL.

16, 20 and 24 are rendered edge to edge, the rest with 9% padding. The mark has
six horizontal bands, which at 16px is under three pixels each, so the small
sizes need every pixel they can get. They stay soft at 16, and there is no
simplified variant for the small sizes.

The tile is the Stackable mark rather than Trino's. Naming the connector
"Stackable Trino" is nominative use of a trademark and is fine. Shipping the
Trino Software Foundation's logo as this product's tile would imply an
endorsement nobody gave.

To regenerate after a brand asset changes (needs no system packages, `uv`
fetches the renderer):

```bash
uv run --with cairosvg python3 -c '
import cairosvg
src = open(".readme/static/borrowed/Icon_Stackable.svg").read()
body = src.split(">", 1)[1].rsplit("</svg>", 1)[0]
for size in (16, 20, 24, 32, 40, 48, 64):
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

## Testing with the Power Query SDK

1. Open `connector/` in VS Code with the Power Query SDK extension
2. Open `StackableTrinoODBC.query.pq`
3. Ctrl+Shift+Alt+E to evaluate. This runs the test query against a local Trino
   instance without installing the `.mez`.

## Verifying query folding

Automatically, on Linux, against a running Trino:

```bash
uv run --with pyodbc python3 integration-tests/suites/test_folding_contract.py "<connection-string>"
```

This is the only thing that reads the `.pq` outside Power BI, so it is the only
guard against a connector declaration drifting from what the driver reports or
what Trino accepts. It parses the connector rather than transcribing it, and
checks three things:

- every `Constant` visitor field name is a real driver `TYPE_NAME`
- every CAST target is a type Trino has
- the row-limiting clause the `AstVisitor` builds runs, including the order it
  concatenates the two clauses in. Trino's grammar is
  `OFFSET count LIMIT count` and rejects the reverse.

By hand, in Power BI Desktop, after connecting:

1. Open the Power Query Editor (Transform Data)
2. Add a filter, sort, or aggregation step
3. Right-click the step → **View Native Query**
4. If it is greyed out, the step did not fold. Check the connector
   configuration.

ODBC tracing on Windows shows the exact SQL sent to the driver.
