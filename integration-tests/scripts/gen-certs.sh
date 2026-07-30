#!/usr/bin/env bash
# One CA, and every leaf signed from it.
#
# The coordinator's SAN carries the names a client connects by, and Jetty
# selects the certificate on SNI. Anything it cannot match gets the self-signed
# certificate Trino generates for internal communication instead, so a name
# missing from this list does not yield a hostname mismatch against this
# certificate; it yields a different certificate. suites/test_tls.py documents
# what that rules out.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
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

echo "--- keycloak certificate ---"
gen_leaf keycloak "/CN=keycloak" "DNS:keycloak,DNS:localhost" serverAuth

echo "--- coordinator keystore ---"
openssl pkcs12 -export \
    -in "$CERT_DIR/trino.crt" -inkey "$CERT_DIR/trino.key" \
    -certfile "$CERT_DIR/ca.crt" -name trino \
    -out "$CERT_DIR/keystore.p12" -passout "pass:${KEYSTORE_PASSWORD}"

echo "--- truststore (the CA alone) ---"
# Serves two jobs: Trino's client-certificate truststore, and (because Trino
# derives its internal http clients' truststore from the same setting) the
# trust anchor for the HTTPS-only internal discovery loop.
#
# keytool, not `openssl pkcs12 -export -nokeys`. openssl writes the certificate
# into a certBag with no Oracle trusted-certificate attribute, and Java then
# reports "Your keystore contains 0 entries". That is an empty truststore
# failing silently: the coordinator still starts, but it validates no client
# certificate and logs 503s fetching its own memory info over TLS.
rm -f "$CERT_DIR/truststore.p12"
keytool -importcert -noprompt -trustcacerts \
    -alias ca -file "$CERT_DIR/ca.crt" \
    -keystore "$CERT_DIR/truststore.p12" -storetype PKCS12 \
    -storepass "$KEYSTORE_PASSWORD"

# Assert it is readable and non-empty. The failure above produced a valid
# PKCS12 file that Java saw as empty, so the file existing proves nothing.
entries="$(keytool -list -keystore "$CERT_DIR/truststore.p12" -storetype PKCS12 \
    -storepass "$KEYSTORE_PASSWORD" 2>/dev/null | grep -c 'trustedCertEntry' || true)"
if [[ "$entries" -lt 1 ]]; then
    echo "ERROR: truststore.p12 holds no trustedCertEntry; Java would read it as empty" >&2
    exit 1
fi
echo "truststore holds $entries trusted certificate(s)."

echo "--- client PEM for the driver's ClientCertificate key ---"
# One file: the certificate chain, then the PKCS#8 private key. The driver
# builds reqwest on rustls, which accepts neither PKCS#12 nor JKS, so this is
# the only form the ClientCertificate connection-string key takes.
openssl pkcs8 -topk8 -nocrypt -in "$CERT_DIR/client.key" -out "$CERT_DIR/client.pk8"
cat "$CERT_DIR/client.crt" "$CERT_DIR/ca.crt" "$CERT_DIR/client.pk8" > "$CERT_DIR/client.pem"
rm -f "$CERT_DIR/client.pk8"

echo "Certificates written to $CERT_DIR"
