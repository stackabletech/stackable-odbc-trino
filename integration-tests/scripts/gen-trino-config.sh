#!/usr/bin/env bash
# Assembles generated/trino/ from stack/trino/ fragments, per active profile.
#
# Compose profiles select *services*; they cannot vary a mounted file's
# contents. And Trino refuses to start when config.properties names an OAuth
# issuer, an S3 endpoint or a metastore that is not running, so a superset
# configuration is not available either. Hence assembly.
#
# A fragment directory may contain:
#   *.properties            copied in whole (spooling-manager.properties, say)
#   catalog/*.properties    copied into catalog/
#   config.properties.d/*   appended onto config.properties
#
# Appending is why a value that *changes* between profiles cannot be a
# fragment: a duplicate key is a Trino startup error. Those are @PLACEHOLDER@
# substitutions instead, resolved at the end.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

OUT="$GENERATED/trino"
rm -rf "$OUT"
mkdir -p "$OUT/catalog"

cp -r "$STACK_DIR/trino/base/." "$OUT/"

for profile in ${PROFILES//,/ }; do
    src="$STACK_DIR/trino/$profile"
    if [[ ! -d "$src" ]]; then
        echo "ERROR: no config fragment directory for profile '$profile'" >&2
        exit 2
    fi

    shopt -s nullglob
    for f in "$src"/*.properties; do cp "$f" "$OUT/"; done
    for f in "$src"/catalog/*.properties; do cp "$f" "$OUT/catalog/"; done
    for f in "$src"/config.properties.d/*; do
        printf '\n# --- from profile: %s ---\n' "$profile" >> "$OUT/config.properties"
        cat "$f" >> "$OUT/config.properties"
    done
    shopt -u nullglob
done

# --- computed values ---
# CERTIFICATE first, so a connection presenting a client certificate is
# authenticated by it and one that presents none falls through to PASSWORD.
#
# OAUTH2 goes last, and that ordering is load-bearing rather than cosmetic:
# Trino emits one `WWW-Authenticate` header per configured type in this order, so
# `Basic realm="Trino"` precedes the Bearer challenge. That is the arrangement
# the client's header scan has to survive, and a stack that emitted the Bearer
# challenge first would let a client reading only the first header pass.
AUTH_TYPES="CERTIFICATE,PASSWORD"
if [[ ",$PROFILES," == *",oauth,"* ]]; then
    AUTH_TYPES="$AUTH_TYPES,OAUTH2"
fi

sed -i "s|@AUTH_TYPES@|$AUTH_TYPES|g" "$OUT/config.properties"

# Stated in lib.sh and used by both the Trino config and the Keycloak realm.
sed -i -e "s|@OAUTH_CLIENT_ID@|$OAUTH_CLIENT_ID|g" \
       -e "s|@OAUTH_CLIENT_SECRET@|$OAUTH_CLIENT_SECRET|g" \
       "$OUT/config.properties"

# An unresolved placeholder reaches Trino as a literal, which is why these are
# @NAME@ rather than an empty default: a fragment added later that introduces
# one without teaching this script about it fails here instead.
if grep -qE '@[A-Z_]+@' "$OUT"/*.properties; then
    echo "ERROR: unresolved placeholder in the assembled config:" >&2
    grep -nE '@[A-Z_]+@' "$OUT"/*.properties >&2
    exit 2
fi

echo "Assembled Trino config for profiles '${PROFILES:-<core only>}' into $OUT"
