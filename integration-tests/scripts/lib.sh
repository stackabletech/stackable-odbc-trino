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
# The schema scripts/seed-hive.sh creates in the hive catalog, and the one every
# suite that needs a writable, transactional table works in.
HIVE_SCHEMA="tx"
KEYSTORE_PASSWORD="changeit"

# Keycloak, for the `oauth` profile. 8444 on the host because Trino holds 8443;
# inside its own container Keycloak listens on 8443.
KEYCLOAK_HOST="localhost"
KEYCLOAK_HTTPS_PORT=8444
KEYCLOAK_REALM="trino"
# The browser-facing base URL. This is also the `iss` claim Keycloak stamps into
# its tokens, because every generated URL derives from KC_HOSTNAME, so it is
# what Trino's `issuer` property has to be set to.
KEYCLOAK_ISSUER="https://$KEYCLOAK_HOST:$KEYCLOAK_HTTPS_PORT/realms/$KEYCLOAK_REALM"
# The same principal password.db and the client certificate resolve to, so the
# Trino session user is `admin` however the connection authenticated and
# `current_user` is comparable across all three.
KEYCLOAK_USER="$TRINO_USER"
KEYCLOAK_PASSWORD="$TRINO_PASSWORD"
OAUTH_CLIENT_ID="trino"
OAUTH_CLIENT_SECRET="trino-test-client-secret"

# MinIO and the spooling protocol, for the `spooling` profile.
# MinIO's port is not published. Under
# protocol.spooling.retrieval-mode=coordinator_proxy the client never reaches
# object storage; only the coordinator does, over the compose network.
#
# Exported, because compose.yaml interpolates these three rather than repeating
# them: Trino gets them substituted into spooling-manager.properties and MinIO
# gets them from its environment, and a mismatch would surface only when a query
# tried to spool. The defaults in compose.yaml exist so `docker compose logs`
# still parses the file outside the harness.
export MINIO_ACCESS_KEY="minioadmin"
export MINIO_SECRET_KEY="minioadmin"
export SPOOLING_BUCKET="spooling"
# A 256-bit base64 key, fixed rather than generated per setup: the stack is
# reproducible and nothing here is a real secret.
SPOOLING_SECRET="W0PUdc6us24Z5Ki2Oi92/iaLd8Oksfxge59U2EHmKwo="

# Every profile this stack knows about. `--profile all` expands to this.
ALL_PROFILES="oauth spooling"

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
            # shellcheck disable=SC2206  # word splitting is intended here
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

# service_running <service>. True while its container exists and is running.
service_running() {
    local id
    id="$(compose ps -q "$1" 2>/dev/null)" || return 1
    [[ -n "$id" ]] || return 1
    [[ "$(docker inspect -f '{{.State.Running}}' "$id" 2>/dev/null)" == "true" ]]
}

# wait_for <service> <description> <seconds> <command...>
#
# The command is run in this shell, so a shell function works and the nested
# quoting a `bash -c "curl ... | grep ..."` would need does not arise.
#
# The wait gives up the moment <service>'s container stops running, and prints
# its log either way. A coordinator that rejects its own configuration exits
# within seconds and stays exited, so spending the whole budget on it wastes
# minutes and then reports a timeout, while the log says exactly which property
# is wrong.
wait_for() {
    local service="$1" what="$2" limit="$3"; shift 3
    local waited=0
    while ! "$@" &>/dev/null; do
        if ! service_running "$service"; then
            echo "ERROR: the $service container is not running. Its last log lines:" >&2
            compose logs --tail 40 "$service" >&2
            return 1
        fi
        if (( waited >= limit )); then
            echo "ERROR: $what did not become ready in ${limit}s. Last log lines:" >&2
            compose logs --tail 40 "$service" >&2
            return 1
        fi
        sleep 5
        waited=$(( waited + 5 ))
        echo "  Waiting for $what... (${waited}s)"
    done
    echo "$what is ready."
}

# wait_for_init <service> <seconds>
#
# The counterpart of wait_for for a one-shot container, whose success *is*
# exiting. `docker compose wait` cannot serve here: it considers only running
# containers and answers "no containers for project" once the container has
# finished, which for an init container is the usual case by the time anything
# asks.
wait_for_init() {
    local service="$1" limit="$2" waited=0 id state
    while true; do
        id="$(compose ps -aq "$service" 2>/dev/null || true)"
        if [[ -n "$id" ]]; then
            state="$(docker inspect -f '{{.State.Status}}:{{.State.ExitCode}}' "$id" 2>/dev/null || true)"
            case "$state" in
                exited:0)
                    echo "$service completed."
                    return 0
                    ;;
                exited:*)
                    echo "ERROR: $service exited with code ${state#exited:}. Its log:" >&2
                    compose logs --tail 40 "$service" >&2
                    return 1
                    ;;
            esac
        fi
        if (( waited >= limit )); then
            echo "ERROR: $service did not complete in ${limit}s. Its log:" >&2
            compose logs --tail 40 "$service" >&2
            return 1
        fi
        sleep 2
        waited=$(( waited + 2 ))
        echo "  Waiting for $service... (${waited}s)"
    done
}
