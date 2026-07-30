#!/usr/bin/env bash
# Assembles generated/keycloak/ from stack/keycloak/, and makes Keycloak's
# private key readable inside its container.
#
# The realm names the client secret, the realm name, the user and the Trino
# callback port, and so does the Trino config fragment. Substituting both from
# lib.sh is what stops the two disagreeing. Everything mounted into a container
# comes from generated/, never from the checkout.
#
# Three fields in realm-trino.json are load-bearing, and JSON cannot say so
# itself:
#
#   directAccessGrantsEnabled  enables the password grant, which is how a token's
#                              claims are read with no browser in the loop.
#   the oidc-audience-mapper   puts the client id into `aud`. Keycloak's default
#                              access-token audience is `account`, and Trino
#                              requires its own client id to be present.
#   requiredActions: []        with a non-temporary credential, so Keycloak does
#                              not interpose an "update password" page that the
#                              test browser would not fill in.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

OUT="$GENERATED/keycloak"
# Created even when the profile is off, so the compose mount always resolves.
mkdir -p "$OUT"

if [[ ",$PROFILES," != *",oauth,"* ]]; then
    echo "oauth profile inactive; skipping Keycloak config."
    exit 0
fi

# Keycloak reads the key file directly and its container user need not be
# uid 1000, while openssl writes 0600. Done on every run rather than in
# gen-certs.sh, which exits early when the certificates already exist.
chmod 0644 "$CERT_DIR/keycloak.key"

sed -e "s|@KEYCLOAK_REALM@|$KEYCLOAK_REALM|g" \
    -e "s|@KEYCLOAK_USER@|$KEYCLOAK_USER|g" \
    -e "s|@KEYCLOAK_PASSWORD@|$KEYCLOAK_PASSWORD|g" \
    -e "s|@OAUTH_CLIENT_ID@|$OAUTH_CLIENT_ID|g" \
    -e "s|@OAUTH_CLIENT_SECRET@|$OAUTH_CLIENT_SECRET|g" \
    -e "s|@TRINO_HTTPS_PORT@|$TRINO_HTTPS_PORT|g" \
    "$STACK_DIR/keycloak/realm-trino.json" > "$OUT/realm-trino.json"

# An unresolved placeholder would reach Keycloak as a literal client secret,
# which fails later as an opaque `invalid_client`.
if grep -qE '@[A-Z_]+@' "$OUT/realm-trino.json"; then
    echo "ERROR: unresolved placeholder in the assembled realm:" >&2
    grep -nE '@[A-Z_]+@' "$OUT/realm-trino.json" >&2
    exit 2
fi

echo "Assembled the Keycloak realm into $OUT"
