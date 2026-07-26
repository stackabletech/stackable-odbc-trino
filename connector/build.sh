#!/usr/bin/env bash
set -euo pipefail

# Build the StackableTrinoODBC.mez Power Query custom connector.
# A .mez is just a ZIP archive containing the .pq, .pqm, .resx, and .png files.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="$SCRIPT_DIR/bin"
MEZ_FILE="$OUT_DIR/StackableTrinoODBC.mez"

mkdir -p "$OUT_DIR"
rm -f "$MEZ_FILE"

cd "$SCRIPT_DIR"
zip -j "$MEZ_FILE" \
    StackableTrinoODBC.pq \
    Diagnostics.pqm \
    OdbcConstants.pqm \
    resources.resx \
    StackableTrinoODBC*.png

echo "Built: $MEZ_FILE ($(du -h "$MEZ_FILE" | cut -f1))"
echo ""
echo "To install in PowerBI Desktop:"
echo "  1. Copy $MEZ_FILE to: %USERPROFILE%\\Documents\\Power BI Desktop\\Custom Connectors\\"
echo "  2. Enable custom connectors: File > Options > Security > Allow any extension"
echo "  3. Restart PowerBI Desktop"
echo "  4. Get Data > More > Database > Trino (ODBC)"
