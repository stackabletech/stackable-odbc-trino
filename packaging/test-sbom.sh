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
check "component count" "$(jq '.components | length' "$SBOM")" "168"

# Expects 1, not 0. The one unlicensed component is syft's type:"file" entry for
# the artifact itself, which is not a cargo package and so is not in the lookup.
# The finalize stage removes it and this tightens to 0.
check "every cargo component is licensed" \
  "$(jq '[.components[] | select((.licenses // []) | length == 0)] | length' "$SBOM")" "1"

check "no bare pkg:cargo purl on a git or path dep" \
  "$(jq '[.components[] | select(.purl != null)
         | select(.name == "trino-rust-client" or .name == "stackable-odbc-core")
         | select(.purl | test("^pkg:cargo/[^?]*$"))] | length' "$SBOM")" "0"

check "the fork purl names an immutable commit" \
  "$(jq -r '.components[] | select(.name == "trino-rust-client") | .purl' "$SBOM")" \
  "pkg:cargo/trino-rust-client@0.11.0?vcs_url=git+https://github.com/stackabletech/trino-rust-client.git@4a835ccfe4d8332b495cbd74ee1ba48971cbc024"

check "syft cpe23 noise is stripped" \
  "$(jq '[.components[].properties[]? | select(.name | startswith("syft:cpe23"))] | length' "$SBOM")" "0"

check "dev-dependencies are absent" \
  "$(jq '[.components[] | select(.name | test("^(criterion|proptest|serial_test)$"))] | length' "$SBOM")" "0"

check "the native component is merged in" \
  "$(jq '[.components[] | select(.name == "unixodbc")] | length' "$SBOM")" "1"

check "the native component keeps its soname" \
  "$(jq -r '.components[] | select(.name == "unixodbc")
            | .properties[] | select(.name == "stackable:soname") | .value' "$SBOM")" \
  "libodbcinst.so.2"

check "the Windows runtime is not merged into a Linux SBOM" \
  "$(jq '[.components[] | select(.name == "libgcc" or .name == "mingw-w64-runtime")] | length' "$SBOM")" "0"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "All checks passed."
else
  echo "$FAILURES check(s) failed."
  exit 1
fi
