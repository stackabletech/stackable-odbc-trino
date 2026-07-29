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
# Threading=2 asks unixODBC to serialise at the connection level rather than at
# its default environment level (3). It is required for correctness, not a
# tuning knob: SQLCancel called from one thread while another executes on the
# same statement is held behind that call at Threading=3, so it lands only once
# the statement it was meant to interrupt has already finished on its own.
# Measured on a query that runs ~24s: at Threading=3 the fetch raised HY010
# after 23.9s, at Threading=2 it raised HY008 after 2.0s.
#
# This does NOT affect SQL_ATTR_QUERY_TIMEOUT, which fires either way: core
# enforces that deadline from a timer thread calling Backend::cancel directly,
# inside the driver, so it never crosses the Driver Manager.
cat > "$TMP_INI" <<EOF
[stackable_odbc_trino]
Description=Stackable ODBC driver for Trino
Driver=$INSTALL_DIR/$DRIVER_LIB
Setup=$INSTALL_DIR/$DRIVER_LIB
FileUsage=1
Threading=2
EOF

odbcinst -i -d -f "$TMP_INI"

echo "Stackable Trino ODBC driver installed to $INSTALL_DIR."
echo "Verify with: odbcinst -q -d"
echo ""
echo "To create a DSN (optional), see README.md."
