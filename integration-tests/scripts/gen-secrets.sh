#!/usr/bin/env bash
# Generates the secrets the stack needs. Idempotent.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

PASSFILE="$GENERATED/password.db"

if [[ -f "$PASSFILE" ]]; then
    echo "password.db already exists, skipping."
    exit 0
fi

if command -v htpasswd &>/dev/null; then
    # Trino requires bcrypt cost >= 8; htpasswd's -B defaults to 5, so -C 10.
    htpasswd -c -B -C 10 -b "$PASSFILE" "$TRINO_USER" "$TRINO_PASSWORD"
else
    python3 -c "
import bcrypt, sys
h = bcrypt.hashpw(sys.argv[2].encode(), bcrypt.gensalt(rounds=10)).decode()
print(f'{sys.argv[1]}:{h}')
" "$TRINO_USER" "$TRINO_PASSWORD" > "$PASSFILE"
fi
echo "password.db created."
