#!/usr/bin/env bash
# Stops the stack and removes its volumes.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

# Every profile, so a teardown removes services this invocation did not start.
PROFILES="$(parse_profiles all)"
export PROFILES

compose down -v
