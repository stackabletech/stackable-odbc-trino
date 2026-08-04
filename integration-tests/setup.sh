#!/usr/bin/env bash
# Wrapper. The logic lives in scripts/setup.sh.
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/scripts/setup.sh" "$@"
