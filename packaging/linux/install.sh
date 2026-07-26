#!/usr/bin/env bash
# Install the Stackable Trino ODBC driver on Linux.
# Must be run as root (or via sudo).
# INSTALL_DIR environment variable overrides the default install path.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB_DIR="$SCRIPT_DIR"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/lib/stackable-odbc}"
DRIVER_LIB="libstackable_odbc_trino.so"

if [ "$EUID" -ne 0 ]; then
  echo "This script must be run as root (or via sudo)." >&2
  exit 1
fi

if [ ! -f "$LIB_DIR/$DRIVER_LIB" ]; then
  echo "ERROR: $DRIVER_LIB not found next to install.sh at $LIB_DIR" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
cp "$LIB_DIR/$DRIVER_LIB" "$INSTALL_DIR/"

TMP_INI="$(mktemp)"
trap 'rm -f "$TMP_INI"' EXIT
cat > "$TMP_INI" <<EOF
[stackable_odbc_trino]
Description=Stackable ODBC driver for Trino
Driver=$INSTALL_DIR/$DRIVER_LIB
Setup=$INSTALL_DIR/$DRIVER_LIB
FileUsage=1
EOF

odbcinst -i -d -f "$TMP_INI"

echo "Stackable Trino ODBC driver installed to $INSTALL_DIR."
echo "Verify with: odbcinst -q -d"
echo ""
echo "To create a DSN (optional), see README.md."
