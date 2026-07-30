#!/usr/bin/env bash
# Brings up the test stack: generates certificates, secrets and ODBC config,
# starts Docker Compose, waits for readiness, and builds the driver.
#
# The stack keeps running after this exits. Use run-tests.sh to run the suites,
# or scripts/teardown.sh to stop.
#
# Usage:
#   ./integration-tests/setup.sh
#   ./integration-tests/setup.sh --profile oauth,hive
#   PROFILES=all ./integration-tests/setup.sh
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

PROFILES_ARG="${PROFILES:-}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile) PROFILES_ARG="$2"; shift 2 ;;
        --profile=*) PROFILES_ARG="${1#*=}"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
PROFILES="$(parse_profiles "$PROFILES_ARG")"
export PROFILES

# --- Dependency checks (fail fast) ---
missing=()
docker compose version &>/dev/null || missing+=("'docker compose' v2 plugin — sudo apt install docker-compose-plugin  OR  https://docs.docker.com/compose/install/")
command -v cargo &>/dev/null || missing+=("'cargo' — https://rustup.rs")
command -v curl &>/dev/null || missing+=("'curl' — sudo apt install curl")
command -v openssl &>/dev/null || missing+=("'openssl' — sudo apt install openssl")
# keytool builds the truststore: openssl cannot write a PKCS12 that Java reads
# as a trust anchor. See the comment in gen-certs.sh.
command -v keytool &>/dev/null || missing+=("'keytool' — sudo apt install default-jdk-headless")
if [[ ! -f "$GENERATED/password.db" ]] && ! command -v htpasswd &>/dev/null && ! python3 -c "import bcrypt" 2>/dev/null; then
    missing+=("'htpasswd' or 'python3-bcrypt' — sudo apt install apache2-utils  OR  pip install bcrypt")
fi
if [[ ${#missing[@]} -gt 0 ]]; then
    echo "ERROR: Missing required dependencies:" >&2
    printf '  - %s\n' "${missing[@]}" >&2
    exit 1
fi

echo "=== Profiles: ${PROFILES:-<core only>} ==="

echo "=== Generating secrets ==="
"$SCRIPT_DIR/gen-secrets.sh"

echo "=== Generating TLS material ==="
"$SCRIPT_DIR/gen-certs.sh"

echo "=== Assembling the Keycloak realm ==="
"$SCRIPT_DIR/gen-keycloak-config.sh"

echo "=== Assembling Trino config ==="
"$SCRIPT_DIR/gen-trino-config.sh"

echo "=== Starting the stack ==="
# A profile change must recreate the coordinator. Compose would start the new
# service and leave trino running on its previously assembled config, so
# enabling a profile would appear to do nothing at all.
PROFILE_STAMP="$GENERATED/.profiles"
RECREATE=()
if [[ -f "$PROFILE_STAMP" && "$(cat "$PROFILE_STAMP")" != "$PROFILES" ]]; then
    was="$(cat "$PROFILE_STAMP")"
    echo "Profiles changed (${was:-<core only>} -> ${PROFILES:-<core only>}); recreating Trino"
    RECREATE=(--force-recreate)
fi
printf '%s' "$PROFILES" > "$PROFILE_STAMP"

compose up -d "${RECREATE[@]+"${RECREATE[@]}"}"

# Readiness probes are shell functions, not `bash -c` strings: wait_for runs
# "$@" in this shell, so a function works directly and the nested quoting a
# curl-plus-grep pipeline inside a -c argument would need goes away entirely.
trino_ready() {
    # "starting":false means the coordinator finished its startup sequence and
    # can schedule work. HTTP 200 alone is not enough: /v1/info answers while
    # the node is still initialising, which surfaces later as
    # NO_NODES_AVAILABLE on a data query.
    curl -sf --cacert "$CERT_DIR/ca.crt" \
        "https://$TRINO_HOST:$TRINO_HTTPS_PORT/v1/info" | grep -q '"starting":false'
}

postgresql_catalog_ready() {
    # -u rather than X-Trino-User: PASSWORD authentication is mandatory now
    # that there is no allow-insecure-over-http path to slip through, and the
    # authenticated identity supplies the user.
    curl -sf --cacert "$CERT_DIR/ca.crt" -u "$TRINO_USER:$TRINO_PASSWORD" \
        -X POST "https://$TRINO_HOST:$TRINO_HTTPS_PORT/v1/statement" \
        -H "X-Trino-Catalog: postgresql" \
        -H "X-Trino-Schema: public" \
        -d "SELECT 1 FROM postgresql.public.customers LIMIT 1" | grep -q '"stats"'
}

echo "=== Waiting for Trino ==="
wait_for trino "Trino" 600 trino_ready

echo "=== Waiting for the postgresql catalog ==="
wait_for trino "postgresql catalog" 60 postgresql_catalog_ready

if [[ ",$PROFILES," == *",oauth,"* ]]; then
    keycloak_ready() {
        # The discovery document rather than /health/ready: this is the endpoint
        # whose absence actually breaks the flow, and it proves the realm import
        # finished, which a health probe does not.
        curl -sf --cacert "$CERT_DIR/ca.crt" \
            "$KEYCLOAK_ISSUER/.well-known/openid-configuration" |
            grep -q '"token_endpoint"'
    }
    echo "=== Waiting for Keycloak ==="
    wait_for keycloak "Keycloak" 180 keycloak_ready
fi

echo "=== Building the driver ==="
(cd "$PROJECT_DIR" && cargo build)

echo "=== Writing ODBC config ==="
"$SCRIPT_DIR/gen-odbc-config.sh"

cat <<EOF

=== Setup complete ===

Run tests:    ./integration-tests/run-tests.sh [--windows]
Tear down:    ./integration-tests/scripts/teardown.sh

Interactive:
  export ODBCSYSINI=$GENERATED
  export ODBCINI=$GENERATED/odbc.ini
  isql -3 trino_https -v
EOF
