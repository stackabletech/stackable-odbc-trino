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
#   ./integration-tests/run-tests.sh --suite tls    # only suites whose name matches
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

RUN_WINDOWS=false
SKIP_DELETE=false
SKIP_BUILD=false
WINDOWS_EXTRA_ARGS=()

SUITE_FILTER=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --windows) RUN_WINDOWS=true; shift ;;
        --skip-delete) SKIP_DELETE=true; shift ;;
        --skip-build) SKIP_BUILD=true; WINDOWS_EXTRA_ARGS+=("$1"); shift ;;
        --suite) SUITE_FILTER="$2"; shift 2 ;;
        --suite=*) SUITE_FILTER="${1#*=}"; shift ;;
        *) WINDOWS_EXTRA_ARGS+=("$1"); shift ;;
    esac
done

if [[ "$SKIP_DELETE" == false ]]; then
    trap 'echo "=== Tearing down the stack ===" && "$SCRIPT_DIR/teardown.sh"' EXIT
fi

# --- Set the stack up if it has not been ---
# Two conditions, and neither implies the other. stack.env is what the suites
# read, so a running coordinator without it is unusable. But `generated/`
# survives a teardown, so the file on its own says only that the stack was
# configured once, possibly in a previous session.
#
# Checking the file alone let a whole run execute against a coordinator that was
# not there. That reports as a plausible mix rather than as an error: every
# check needing the server fails with `unable to reach Trino server`, while the
# ones asserting a *refusal* pass, because a refusal is what they wanted and a
# dead port supplies one. A suite must not be able to pass for that reason.
if [[ ! -f "$STACK_ENV" ]]; then
    echo "=== Stack not set up, calling setup.sh ==="
    "$SCRIPT_DIR/setup.sh"
elif ! service_running trino; then
    echo "=== Stack configured but not running, calling setup.sh ==="
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
# Built from stack.env rather than written out here, so a port, a credential
# or a certificate path is stated in exactly one place. The four configurations
# are DSN and DSN-less crossed with verified and unverified TLS. The
# HTTP/HTTPS axis went with the plaintext listener.
stack_conn() {
    python3 - "$@" <<'PYEOF'
import os, sys
sys.path.insert(0, os.path.join(os.environ["TEST_DIR"], "suites"))
from harness import Stack
overrides = dict(a.split("=", 1) for a in sys.argv[1:])
for k, v in overrides.items():
    if v == "":
        overrides[k] = None
print(Stack.load(os.environ["STACK_ENV"]).conn_str(**overrides))
PYEOF
}
export TEST_DIR STACK_ENV

CONN_HTTPS="$(stack_conn)"
CONN_HTTPS_NOVERIFY="$(stack_conn TlsVerify=false Certificate=)"

# --- the suite registry ---
# Each entry is  name|required profile|command. An empty profile means the core
# stack. A suite whose profile is not active is SKIPPED and says which profile
# would enable it: an unrun suite must never be printable as a passing one.
SUITES=(
  "integration (DSN-less, verified TLS)||uv run --with pyodbc python3 $TEST_DIR/suites/test_integration.py '$CONN_HTTPS'"
  "integration (DSN-less, TlsVerify=false)||uv run --with pyodbc python3 $TEST_DIR/suites/test_integration.py '$CONN_HTTPS_NOVERIFY'"
  "integration (DSN, verified TLS)||uv run --with pyodbc python3 $TEST_DIR/suites/test_integration.py 'DSN=trino_https'"
  "integration (DSN, TlsVerify=false)||uv run --with pyodbc python3 $TEST_DIR/suites/test_integration.py 'DSN=trino_https_verify_false'"
  "harness unit tests||python3 $TEST_DIR/suites/test_harness.py"
  "sql surface||uv run --with pyodbc python3 $TEST_DIR/suites/test_sql_surface.py '$CONN_HTTPS'"
  "folding contract||uv run --with pyodbc python3 $TEST_DIR/suites/test_folding_contract.py '$CONN_HTTPS'"
  "tls||uv run --with pyodbc python3 $TEST_DIR/suites/test_tls.py"
  "raw C ABI||python3 $TEST_DIR/suites/test_c_abi.py '$DRIVER_PATH' '$CONN_HTTPS'"
  "type matrix||python3 $TEST_DIR/suites/test_type_matrix.py '$DRIVER_PATH' '$CONN_HTTPS'"
  # No required profile: with `spooling` active it drives the spooled protocol,
  # and without it asserts the fallback a coordinator with no spooling manager
  # produces. Both are real assertions, so neither stack state is a blind spot.
  "spooling||uv run --with pyodbc python3 $TEST_DIR/suites/test_spooling.py"
  # No required profile either: the hive catalog it writes to is in the base
  # stack, because a file metastore costs no container.
  "transactions||uv run --with pyodbc python3 $TEST_DIR/suites/test_transactions.py"
  # ctypes rather than pyodbc, and no connection string: pyodbc passes
  # SQL_DRIVER_NOPROMPT unconditionally, which core reads as forbidding the
  # prompt an interactive login needs, so this suite builds its own.
  "oauth|oauth|python3 $TEST_DIR/suites/test_oauth.py '$DRIVER_PATH'"
)

failed_suites=()
skipped_suites=()

for entry in "${SUITES[@]}"; do
    name="${entry%%|*}"; rest="${entry#*|}"
    need="${rest%%|*}"; cmd="${rest#*|}"

    if [[ -n "$SUITE_FILTER" && "$name" != *"$SUITE_FILTER"* ]]; then
        continue
    fi
    if [[ -n "$need" && ",${PROFILES:-}," != *",$need,"* ]]; then
        echo "SKIP  $name: profile '$need' is not active (setup.sh --profile $need)"
        skipped_suites+=("$name")
        continue
    fi

    echo "=== Running $name ==="
    if ! eval "$cmd"; then
        failed_suites+=("$name")
    fi
done

# --- Linux: Rust FFI integration tests ---
# Outside the registry because it is cargo, not a Python suite, and its output
# convention is different. It still honours --suite and still reports into the
# same summary, so a failure here cannot abort the run before the totals print.
#
# Only FFI tests are run. The backend::tests integration tests use a separate
# TrinoConnection with its own reqwest pool, and running both groups against
# the same coordinator causes intermittent connection pool corruption. Run
# those in isolation with:
#   cargo test -- --ignored backend
if [[ -z "$SUITE_FILTER" || "ffi" == *"$SUITE_FILTER"* ]]; then
    echo "=== Running ffi (cargo) ==="
    if ! (cd "$PROJECT_DIR" && cargo test -- --ignored ffi_integration_tests); then
        failed_suites+=("ffi (cargo)")
    fi
fi

# --- Windows VM tests (optional) ---
# Before the summary, so its result is part of the single verdict rather than
# unreachable behind a Linux failure.
if [[ "$RUN_WINDOWS" == true ]]; then
    echo "=== Running windows (VM) ==="
    if ! uv run --with pywinrm python3 "$TEST_DIR/windows/windows_test.py" "${WINDOWS_EXTRA_ARGS[@]+"${WINDOWS_EXTRA_ARGS[@]}"}"; then
        failed_suites+=("windows (VM)")
    fi
fi

# --- suite summary ---
echo ""
echo "=== Suite summary ==="
# Guarded: `printf '%s\n' "${empty[@]}"` still prints one empty line, which in
# a summary reads as an unnamed skipped suite.
if [[ ${#skipped_suites[@]} -gt 0 ]]; then
    printf '  SKIP  %s\n' "${skipped_suites[@]}"
fi
if [[ ${#failed_suites[@]} -gt 0 ]]; then
    printf '  FAIL  %s\n' "${failed_suites[@]}"
    echo "${#failed_suites[@]} suite(s) failed"
    exit 1
fi
echo "all selected suites passed"
