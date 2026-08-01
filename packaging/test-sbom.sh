#!/usr/bin/env bash
# Assertions for packaging/sbom.sh, run against the real release artifact.
#
# Needs the release .so, syft and cargo-auditable. Builds the .so if absent.
# Run from anywhere: ./packaging/test-sbom.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SO="$REPO_ROOT/target/release/libstackable_odbc_trino.so"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
check() {
  local label="$1" actual="$2" expected="$3"
  if [ "$actual" = "$expected" ]; then
    echo "PASS  $label"
  else
    echo "FAIL  $label: expected '$expected', got '$actual'"
    FAILURES=$((FAILURES + 1))
  fi
}

if [ ! -f "$SO" ]; then
  echo "Building the release artifact with cargo auditable..."
  (cd "$REPO_ROOT" && cargo auditable build --release)
fi

"$REPO_ROOT/packaging/sbom.sh" "$SO" "$WORK"
SBOM="$WORK/libstackable_odbc_trino.so.cdx.json"

check "SBOM file is written" "$([ -f "$SBOM" ] && echo yes || echo no)" "yes"
check "component count" "$(jq '.components | length' "$SBOM")" "167"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "All checks passed."
else
  echo "$FAILURES check(s) failed."
  exit 1
fi
