#!/usr/bin/env bash
# Sets up the Trino test environment: generates TLS cert and password.db,
# starts Trino via Docker Compose, waits for it to be healthy, builds the
# driver, and writes the ODBC config files.
#
# Trino keeps running after this script exits. Use run-tests.sh to execute
# tests and tear down, or manage the compose stack manually:
#   docker compose -f test/docker-compose.yml down
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"
PASSFILE="$SCRIPT_DIR/trino-config/password.db"

# --- Dependency checks (fail fast) ---
missing=()
if ! docker compose version &>/dev/null; then
    missing+=("'docker compose' v2 plugin — sudo apt install docker-compose-plugin  OR  https://docs.docker.com/compose/install/")
fi
if ! command -v cargo &>/dev/null; then
    missing+=("'cargo' — https://rustup.rs")
fi
if ! command -v curl &>/dev/null; then
    missing+=("'curl' — sudo apt install curl")
fi
if [[ ! -f "$PASSFILE" ]] && ! command -v htpasswd &>/dev/null && ! python3 -c "import bcrypt" 2>/dev/null; then
    missing+=("'htpasswd' or 'python3-bcrypt' — sudo apt install apache2-utils  OR  pip install bcrypt")
fi
if [[ ${#missing[@]} -gt 0 ]]; then
    echo "ERROR: Missing required dependencies:" >&2
    for dep in "${missing[@]}"; do
        printf "  - %s\n" "$dep" >&2
    done
    exit 1
fi

echo "=== Generating TLS certificate ==="
bash "$SCRIPT_DIR/tls/generate-cert.sh"

echo "=== Generating password.db (admin/admin) ==="
if [[ ! -f "$PASSFILE" ]]; then
    if command -v htpasswd &>/dev/null; then
        # Trino requires bcrypt cost >= 8; htpasswd defaults to cost 5 with -B, use -C 10
        htpasswd -c -B -C 10 -b "$PASSFILE" admin admin
    else
        python3 -c "
import bcrypt
h = bcrypt.hashpw(b'admin', bcrypt.gensalt(rounds=10)).decode()
print(f'admin:{h}')
" > "$PASSFILE"
    fi
    echo "password.db created."
else
    echo "password.db already exists, skipping."
fi

echo "=== Starting Trino via Docker Compose ==="
cd "$SCRIPT_DIR"
docker compose up -d

echo "=== Waiting for Trino to be healthy (up to 600s) ==="
for i in $(seq 1 120); do
    # Wait for "starting":false — this means the coordinator has completed its
    # startup sequence and worker nodes are ready to run queries. Checking only
    # for HTTP 200 is insufficient: /v1/info responds early while the node is
    # still initialising, causing NO_NODES_AVAILABLE on data queries.
    if curl -sf http://localhost:8080/v1/info 2>/dev/null | grep -q '"starting":false'; then
        echo "Trino is ready."
        break
    fi
    if [[ $i -eq 120 ]]; then
        echo "ERROR: Trino did not become healthy in 600 seconds." >&2
        echo "Run: docker compose logs trino" >&2
        exit 1
    fi
    echo "  Waiting... ($((i * 5))s)"
    sleep 5
done

echo "=== Waiting for PostgreSQL catalog in Trino (up to 60s) ==="
for i in $(seq 1 12); do
    if curl -sf -X POST http://localhost:8080/v1/statement \
        -H "X-Trino-User: admin" \
        -H "X-Trino-Catalog: postgresql" \
        -H "X-Trino-Schema: public" \
        -d "SELECT 1 FROM postgresql.public.customers LIMIT 1" 2>/dev/null | grep -q '"stats"'; then
        echo "PostgreSQL catalog is ready."
        break
    fi
    if [[ $i -eq 12 ]]; then
        echo "WARNING: PostgreSQL catalog not yet available — PK/FK tests may fail." >&2
    fi
    echo "  Waiting... ($((i * 5))s)"
    sleep 5
done

echo "=== Building stackable-odbc-trino ==="
cd "$PROJECT_DIR"
cargo build

DRIVER_PATH="$PROJECT_DIR/target/debug/libstackable_odbc_trino.so"

echo "=== Writing ODBC install config ==="
cat > "$SCRIPT_DIR/odbcinst.ini" << EOF
[stackable_odbc_trino]
Driver = $DRIVER_PATH
EOF

echo "=== Writing ODBC driver config ==="
cat > "$SCRIPT_DIR/odbc.ini" << EOF
[trino_https]
Driver = stackable_odbc_trino
Host = localhost
Port = 8443
User = admin
Password = admin
Protocol = https
Catalog = tpcds
Certificate = $SCRIPT_DIR/tls/server.crt

[trino_https_verify_false]
Driver = stackable_odbc_trino
Host = localhost
Port = 8443
User = admin
Password = admin
Protocol = https
Catalog = tpcds
TlsVerify = false

[trino_http]
Driver = stackable_odbc_trino
Host = localhost
Port = 8080
User = admin
Password = admin
Protocol = http
Catalog = tpcds

[trino_postgresql]
Driver = stackable_odbc_trino
Host = localhost
Port = 8080
User = admin
Protocol = http
Catalog = postgresql
EOF

echo ""
echo "=== Setup complete — Trino is running ==="
echo ""
echo "Run tests:    ./test/run-tests.sh [--windows]"
echo "Tear down:    docker compose -f $COMPOSE_FILE down"
echo ""
echo "To test interactively:"
echo "  export ODBCSYSINI=$SCRIPT_DIR"
echo "  export ODBCINI=$SCRIPT_DIR/odbc.ini"
echo "  isql -3 trino_https -v"
echo "  isql -3 trino_https_verify_false -v"
echo "  isql -3 trino_http -v"
