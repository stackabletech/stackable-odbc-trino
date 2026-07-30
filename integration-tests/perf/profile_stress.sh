#!/usr/bin/env bash
# Profile the Trino REST client during stress tests.
#
# Usage:
#   ./test/profile_stress.sh "Driver=/path/to/driver.so;Host=localhost;Port=8080;User=admin;Protocol=http;Catalog=tpcds"
#   ./test/profile_stress.sh "DSN=trino_http"
#
# Output:
#   - Stress test results (PASS/FAIL per test with elapsed time)
#   - Profiling summary table (per-query breakdown of Trino vs client overhead)
#   - Raw log at /tmp/odbc_profile.log
#
# Requires: Trino running (./integration-tests/setup.sh), driver built (cargo build)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOG_FILE="${ODBC_PROFILE_LOG:-/tmp/odbc_profile.log}"

if [ $# -lt 1 ]; then
    echo "Usage: $0 <connection-string>"
    echo ""
    echo "Example:"
    echo "  $0 \"Driver=\$(pwd)/target/debug/libstackable_odbc_trino.so;Host=localhost;Port=8080;User=admin;Protocol=http;Catalog=tpcds\""
    exit 2
fi

CONN_STR="$1"

# Clear previous log
: > "$LOG_FILE"

echo "=== Trino REST Client Profiling ==="
echo "Log file: $LOG_FILE"
echo ""

# Set ODBC config if not already set (for DSN-less connections this is optional,
# but needed if using DSN names).
export ODBCSYSINI="${ODBCSYSINI:-$SCRIPT_DIR}"
export ODBCINI="${ODBCINI:-$SCRIPT_DIR/odbc.ini}"

# Enable profiling: info-level logging with span timing to file.
export ODBC_LOG_LEVEL=info
export ODBC_PROFILING=1
export ODBC_LOG_FILE="$LOG_FILE"

echo "--- Stress Test Output ---"
echo ""

# Run the stress test. Allow failure (exit code 1 = test failures) so we still
# parse the profile log.
uv run --with pyodbc python3 "$SCRIPT_DIR/test_stress.py" "$CONN_STR" || true

echo ""
echo "--- Profiling Summary ---"
echo ""

# Parse the profile log and produce the summary table.
python3 "$SCRIPT_DIR/parse_profile.py" "$LOG_FILE"
