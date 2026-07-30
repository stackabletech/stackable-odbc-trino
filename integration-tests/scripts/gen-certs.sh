#!/usr/bin/env bash
# One CA, and every leaf signed from it.
#
# The coordinator's SAN deliberately omits IP:127.0.0.1 while carrying
# DNS:localhost. That single omission is what makes TlsVerify=ca testable
# against one certificate and one listener: connecting as Host=localhost
# verifies fully, connecting as Host=127.0.0.1 fails hostname verification
# under `full` and succeeds under `ca`, which verifies the chain but not the
# name. Do not add 127.0.0.1 -- test_tls.py asserts its absence.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

# The host-only network gateway, for the Windows VM. Override for a non-default
# libvirt subnet.
HOST_GATEWAY="${ODBC_TEST_HOST_GATEWAY:-192.168.197.1}"

mkdir -p "$CERT_DIR"

if [[ -f "$CERT_DIR/ca.crt" && -f "$CERT_DIR/keystore.p12" && -f "$CERT_DIR/client.pem" ]]; then
    echo "Certificates already exist, skipping generation."
    exit 0
fi

echo "--- certificate authority ---"
openssl req -x509 -newkey rsa:4096 -nodes -days 3650 -sha256 \
    -keyout "$CERT_DIR/ca.key" -out "$CERT_DIR/ca.crt" \
    -subj "/CN=stackable-odbc-trino-test-ca" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign"

# gen_leaf <name> <subject> <subjectAltName> <extendedKeyUsage>
gen_leaf() {
    local name="$1" subj="$2" san="$3" eku="$4"
    openssl req -newkey rsa:2048 -nodes -sha256 \
        -keyout "$CERT_DIR/$name.key" -out "$CERT_DIR/$name.csr" -subj "$subj"
    openssl x509 -req -in "$CERT_DIR/$name.csr" -days 3650 -sha256 \
        -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" -CAcreateserial \
        -out "$CERT_DIR/$name.crt" \
        -extfile <(printf 'subjectAltName=%s\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=%s\n' "$san" "$eku")
    rm -f "$CERT_DIR/$name.csr"
}

echo "--- coordinator certificate ---"
gen_leaf trino "/CN=localhost" \
    "DNS:localhost,DNS:trino,IP:${HOST_GATEWAY}" serverAuth

echo "--- client certificate (mutual TLS) ---"
# CN is the Trino username: http-server.authentication.certificate maps the
# subject to a principal, and the user-mapping pattern extracts this CN.
gen_leaf client "/CN=${TRINO_USER}" "DNS:${TRINO_USER}" clientAuth

echo "--- keycloak certificate (phase 5) ---"
gen_leaf keycloak "/CN=keycloak" "DNS:keycloak,DNS:localhost" serverAuth

echo "--- coordinator keystore ---"
openssl pkcs12 -export \
    -in "$CERT_DIR/trino.crt" -inkey "$CERT_DIR/trino.key" \
    -certfile "$CERT_DIR/ca.crt" -name trino \
    -out "$CERT_DIR/keystore.p12" -passout "pass:${KEYSTORE_PASSWORD}"

echo "--- truststore (the CA alone) ---"
# Serves two jobs: Trino's client-certificate truststore, and the JVM
# truststore that lets the coordinator trust its own certificate over the
# HTTPS-only internal discovery loop.
openssl pkcs12 -export -nokeys \
    -in "$CERT_DIR/ca.crt" -name ca \
    -out "$CERT_DIR/truststore.p12" -passout "pass:${KEYSTORE_PASSWORD}"

echo "--- client PEM for the driver's ClientCertificate key ---"
# One file: the certificate chain, then the PKCS#8 private key. The driver
# builds reqwest on rustls, which accepts neither PKCS#12 nor JKS, so this is
# the only form the ClientCertificate connection-string key takes.
openssl pkcs8 -topk8 -nocrypt -in "$CERT_DIR/client.key" -out "$CERT_DIR/client.pk8"
cat "$CERT_DIR/client.crt" "$CERT_DIR/ca.crt" "$CERT_DIR/client.pk8" > "$CERT_DIR/client.pem"
rm -f "$CERT_DIR/client.pk8"

echo "Certificates written to $CERT_DIR"
