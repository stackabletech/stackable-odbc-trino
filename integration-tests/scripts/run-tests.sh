#!/usr/bin/env bash
# Runs Trino integration tests (Linux) and optionally Windows VM tests.
# Calls setup.sh automatically if Trino is not already running.
# Tears down the Docker Compose stack on exit (unless --skip-delete is passed).
#
# Usage:
#   ./integration-tests/run-tests.sh                         # Linux tests only
#   ./test/run-tests.sh --windows               # Linux + Windows VM tests
#   ./test/run-tests.sh --skip-build            # skip the cargo build (also passed to windows_test.py)
#   ./test/run-tests.sh --skip-delete           # keep Trino running after tests
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_DIR="$(cd "$TEST_DIR/.." && pwd)"

COMPOSE_FILE="$TEST_DIR/stack/compose.yaml"
GENERATED="$TEST_DIR/generated"
DRIVER_PATH="$PROJECT_DIR/target/debug/libstackable_odbc_trino.so"

RUN_WINDOWS=false
SKIP_DELETE=false
SKIP_BUILD=false
WINDOWS_EXTRA_ARGS=()

for arg in "$@"; do
    case "$arg" in
        --windows) RUN_WINDOWS=true ;;
        --skip-delete) SKIP_DELETE=true ;;
        --skip-build) SKIP_BUILD=true; WINDOWS_EXTRA_ARGS+=("$arg") ;;
        *) WINDOWS_EXTRA_ARGS+=("$arg") ;;
    esac
done

if [[ "$SKIP_DELETE" == false ]]; then
    trap 'echo "=== Tearing down Trino ===" && docker compose -f "$COMPOSE_FILE" down' EXIT
fi

# --- Start Trino if not already running ---
if ! docker compose -f "$COMPOSE_FILE" ps --status running 2>/dev/null | grep -q trino; then
    echo "=== Trino not running — calling setup.sh ==="
    "$SCRIPT_DIR/setup.sh"
fi

# --- Rebuild the driver ---
# setup.sh also builds, but it is skipped when Trino is already running. Without
# this, editing driver source and re-running would silently test the previous
# .so. cargo is incremental, so this is a no-op when nothing changed.
if [[ "$SKIP_BUILD" == false ]]; then
    echo "=== Building stackable-odbc-trino ==="
    (cd "$PROJECT_DIR" && cargo build)
fi

# --- Linux: pyodbc integration tests (4 configs, matching Windows) ---
export ODBCSYSINI="$GENERATED"
export ODBCINI="$GENERATED/odbc.ini"

echo "=== Running Linux pyodbc integration tests (DSN-less, HTTP) ==="
uv run --with pyodbc python3 "$TEST_DIR/suites/test_integration.py" \
    "Driver=$DRIVER_PATH;Host=localhost;Port=8080;User=admin;Password=admin;Protocol=http;Catalog=tpcds"

echo "=== Running Linux pyodbc integration tests (DSN-less, HTTPS) ==="
uv run --with pyodbc python3 "$TEST_DIR/suites/test_integration.py" \
    "Driver=$DRIVER_PATH;Host=localhost;Port=8443;User=admin;Password=admin;Protocol=https;TlsVerify=false;Catalog=tpcds"

echo "=== Running Linux pyodbc integration tests (DSN, HTTP) ==="
uv run --with pyodbc python3 "$TEST_DIR/suites/test_integration.py" "DSN=trino_http"

echo "=== Running Linux pyodbc integration tests (DSN, HTTPS) ==="
uv run --with pyodbc python3 "$TEST_DIR/suites/test_integration.py" "DSN=trino_https_verify_false"

# --- Linux: SQL surface pen test ---
# The SQL a BI tool actually emits: every join shape, the GROUP BY extensions,
# window functions, CTEs, set operations, parameters in each clause that takes
# one, the catalog functions, and the statement forms whose result columns have
# no declared length (DESCRIBE, SHOW, EXPLAIN).
echo "=== Running SQL surface pen test ==="
uv run --with pyodbc python3 "$TEST_DIR/suites/test_sql_surface.py" \
    "Driver=$DRIVER_PATH;Host=localhost;Port=8080;User=admin;Password=admin;Protocol=http;Catalog=tpcds"

# --- Linux: Power Query folding contract ---
# The connector's SQL declarations against the driver and Trino. Nothing else
# loads the .pq, and the only other check on folding is a human clicking
# "View Native Query" one step at a time.
echo "=== Running folding contract test ==="
uv run --with pyodbc python3 "$TEST_DIR/suites/test_folding_contract.py" \
    "Driver=$DRIVER_PATH;Host=localhost;Port=8080;User=admin;Password=admin;Protocol=http;Catalog=tpcds"

# --- Linux: raw C ABI pen test (no Driver Manager) ---
# Calls the driver's exported entry points directly with ctypes. unixODBC
# answers a large part of the ODBC state machine itself, so the driver's own
# handling of out-of-order and malformed calls is invisible to every suite
# above this one. Standard library only -- no uv, no pyodbc.
echo "=== Running raw C ABI pen test ==="
python3 "$TEST_DIR/suites/test_c_abi.py" "$DRIVER_PATH"

# --- Linux: type-transform fuzz (no Driver Manager) ---
# Every (Trino value, C data type) pair through SQLGetData, checked against
# invariants rather than a transcribed conversion matrix. Covers the integer
# boundary values, the IEEE specials, NULL per target type, and the statement
# terminator forms. Standard library only.
echo "=== Running type-transform fuzz ==="
python3 "$TEST_DIR/suites/test_type_matrix.py" "$DRIVER_PATH"

# --- Linux: Rust FFI integration tests ---
# Only FFI tests are run here. The backend::tests integration tests use a
# separate TrinoConnection (with its own reqwest pool), and running both
# groups against the same Trino coordinator causes intermittent connection
# pool corruption. Run backend tests in isolation with:
#   cargo test -- --ignored backend
echo "=== Running Trino FFI integration tests ==="
cd "$PROJECT_DIR"
cargo test -- --ignored ffi_integration_tests

# --- Windows VM tests (optional) ---
if [[ "$RUN_WINDOWS" == true ]]; then
    echo "=== Running Windows VM integration tests ==="
    uv run --with pywinrm python3 "$TEST_DIR/windows/windows_test.py" "${WINDOWS_EXTRA_ARGS[@]+"${WINDOWS_EXTRA_ARGS[@]}"}"
fi
