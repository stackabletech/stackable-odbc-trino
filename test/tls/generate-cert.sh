#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The host-only network gateway IP. Override with ODBC_TEST_HOST_GATEWAY for
# non-default libvirt subnets.
HOST_GATEWAY="${ODBC_TEST_HOST_GATEWAY:-192.168.197.1}"

# Skip if cert already exists (idempotent)
if [[ -f "$SCRIPT_DIR/server.crt" && -f "$SCRIPT_DIR/server.key" && -f "$SCRIPT_DIR/keystore.p12" ]]; then
    echo "TLS cert already exists, skipping generation."
    exit 0
fi

echo "=== Generating self-signed TLS certificate ==="

openssl req -x509 -newkey rsa:2048 \
    -keyout "$SCRIPT_DIR/server.key" \
    -out "$SCRIPT_DIR/server.crt" \
    -days 3650 \
    -nodes \
    -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:${HOST_GATEWAY}" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
    -addext "extendedKeyUsage=serverAuth"

openssl pkcs12 -export \
    -in "$SCRIPT_DIR/server.crt" \
    -inkey "$SCRIPT_DIR/server.key" \
    -out "$SCRIPT_DIR/keystore.p12" \
    -name trino \
    -passout pass:changeit

echo "TLS cert and PKCS12 keystore generated."
