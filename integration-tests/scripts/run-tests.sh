#!/usr/bin/env bash
# Runs Trino integration tests (Linux) and optionally Windows VM tests.
# Calls setup.sh automatically if Trino is not already running.
# Tears down the Docker Compose stack on exit (unless --skip-delete is passed).
#
# Usage:
#   ./integration-tests/run-tests.sh                # Linux tests only
#   ./integration-tests/run-tests.sh --windows      # Linux + Windows VM tests
#   ./integration-tests/run-tests.sh --skip-build   # skip the cargo build (also passed to windows_test.py)
#   ./integration-tests/run-tests.sh --skip-delete  # keep the stack running afterwards
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

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
    trap 'echo "=== Tearing down the stack ===" && "$SCRIPT_DIR/teardown.sh"' EXIT
fi

# --- Set the stack up if it has not been ---
# stack.env rather than `compose ps`: a running container is not the same as a
# configured stack, and the suites read stack.env, not docker.
if [[ ! -f "$STACK_ENV" ]]; then
    echo "=== Stack not set up — calling setup.sh ==="
    "$SCRIPT_DIR/setup.sh"
fi

# One description of the stack, shared with the suites. Carries ODBCSYSINI and
# ODBCINI, so the Driver Manager and the suites cannot disagree about which
# config they are reading.
set -a
# shellcheck source=/dev/null
source "$STACK_ENV"
set +a

# --- Rebuild the driver ---
# setup.sh also builds, but it is skipped when Trino is already running. Without
# this, editing driver source and re-running would silently test the previous
# .so. cargo is incremental, so this is a no-op when nothing changed.
if [[ "$SKIP_BUILD" == false ]]; then
    echo "=== Building stackable-odbc-trino ==="
    (cd "$PROJECT_DIR" && cargo build)
fi

# --- Linux: pyodbc integration tests (4 configs, matching Windows) ---
CONN_HTTP="Driver=$DRIVER_PATH;Host=localhost;Port=8080;User=admin;Password=admin;Protocol=http;Catalog=tpcds"

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
python3 "$TEST_DIR/suites/test_c_abi.py" "$DRIVER_PATH" "$CONN_HTTP"

# --- Linux: type-transform fuzz (no Driver Manager) ---
# Every (Trino value, C data type) pair through SQLGetData, checked against
# invariants rather than a transcribed conversion matrix. Covers the integer
# boundary values, the IEEE specials, NULL per target type, and the statement
# terminator forms. Standard library only.
echo "=== Running type-transform fuzz ==="
python3 "$TEST_DIR/suites/test_type_matrix.py" "$DRIVER_PATH" "$CONN_HTTP"

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
