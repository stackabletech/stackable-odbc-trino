#!/usr/bin/env bash
# Wrapper. The logic lives in scripts/run-tests.sh.
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/scripts/run-tests.sh" "$@"
