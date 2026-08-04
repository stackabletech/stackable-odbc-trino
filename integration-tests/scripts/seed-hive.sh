#!/usr/bin/env bash
# Creates the hive catalog's schema, which is the one thing sql-standard
# security will not let an ordinary connection do.
#
# Under hive.security=sql-standard, CREATE SCHEMA requires the admin role, so a
# suite connecting normally gets `Access Denied: Cannot create schema`. Once the
# schema exists and admin owns it, everything else an ordinary connection needs
# works without a role: creating tables, writing, reading and granting. So this
# seeds exactly the schema and stops.
#
# Idempotent, and run on every setup: the file metastore lives in the
# coordinator's writable layer, so recreating the container starts it empty.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

BASE="https://$TRINO_HOST:$TRINO_HTTPS_PORT"

# Submit a statement and follow nextUri to the end.
#
# The drain is not optional: Trino runs a statement as the client pages it, so a
# POST whose response is never followed leaves the DDL unexecuted.
trino_run() {
    local sql="$1" role="${2:-}"
    local curl_args=(
        -sf --cacert "$CERT_DIR/ca.crt" -u "$TRINO_USER:$TRINO_PASSWORD"
        -H "X-Trino-Catalog: hive" -H "X-Trino-Schema: $HIVE_SCHEMA"
    )
    if [[ -n "$role" ]]; then
        curl_args+=(-H "X-Trino-Role: hive=ROLE{$role}")
    fi

    local response next
    response="$(curl "${curl_args[@]}" -X POST -d "$sql" "$BASE/v1/statement")"
    while :; do
        if [[ "$response" == *'"failureInfo"'* ]]; then
            echo "ERROR: seeding the hive catalog failed on: $sql" >&2
            echo "$response" >&2
            return 1
        fi
        # The response is JSON and this is a single flat field, so a sed match
        # is enough; the stack scripts carry no jq dependency.
        next="$(sed -n 's/.*"nextUri":"\([^"]*\)".*/\1/p' <<<"$response")"
        if [[ -z "$next" ]]; then
            return 0
        fi
        response="$(curl "${curl_args[@]}" "$next")"
    done
}

trino_run "CREATE SCHEMA IF NOT EXISTS hive.$HIVE_SCHEMA" admin
echo "Seeded hive.$HIVE_SCHEMA"
