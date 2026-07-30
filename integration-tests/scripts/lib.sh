#!/usr/bin/env bash
# Shared paths, profile handling and helpers. Sourced, never executed.
#
# SC2034: every variable below is consumed by a script that sources this file,
# which shellcheck cannot see from here.
# shellcheck disable=SC2034

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_DIR="$(cd "$TEST_DIR/.." && pwd)"

STACK_DIR="$TEST_DIR/stack"
GENERATED="$TEST_DIR/generated"
CERT_DIR="$GENERATED/certs"
COMPOSE_FILE="$STACK_DIR/compose.yaml"
STACK_ENV="$GENERATED/stack.env"

DRIVER_PATH="$PROJECT_DIR/target/debug/libstackable_odbc_trino.so"

TRINO_HOST="localhost"
TRINO_HTTPS_PORT=8443
TRINO_USER="admin"
TRINO_PASSWORD="admin"
TRINO_CATALOG="tpcds"
KEYSTORE_PASSWORD="changeit"

# Every profile this stack knows about. `--profile all` expands to this.
ALL_PROFILES="oauth spooling hive"

mkdir -p "$GENERATED"

# `docker compose` with the right file, from the right directory: compose
# resolves relative volume paths against the compose file's own directory.
compose() {
    (cd "$STACK_DIR" && COMPOSE_PROFILES="${PROFILES:-}" docker compose -f "$COMPOSE_FILE" "$@")
}

# parse_profiles <arg>. Normalises a comma or space separated list, expands
# `all`, and rejects an unknown name rather than starting a stack that silently
# lacks what was asked for.
parse_profiles() {
    local raw="${1:-}" out=() p
    raw="${raw//,/ }"
    for p in $raw; do
        if [[ "$p" == "all" ]]; then
            # shellcheck disable=SC2206  # deliberate word splitting
            out=($ALL_PROFILES)
            break
        fi
        if [[ " $ALL_PROFILES " != *" $p "* ]]; then
            echo "ERROR: unknown profile '$p'. Known: $ALL_PROFILES all" >&2
            exit 2
        fi
        out+=("$p")
    done
    local IFS=,
    echo "${out[*]:-}"
}

# wait_for <description> <seconds> <command...>
#
# The command is run in this shell, so a shell function works and the nested
# quoting a `bash -c "curl ... | grep ..."` would need does not arise.
wait_for() {
    local what="$1" limit="$2"; shift 2
    local waited=0
    while ! "$@" &>/dev/null; do
        if (( waited >= limit )); then
            echo "ERROR: $what did not become ready in ${limit}s" >&2
            return 1
        fi
        sleep 5
        waited=$(( waited + 5 ))
        echo "  Waiting for $what... (${waited}s)"
    done
    echo "$what is ready."
}
