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

# The purl sbom.sh should emit for a git-sourced package, built from what
# Cargo.lock actually resolved.
#
# Derived rather than written out as a literal, because both git dependencies
# move: the fork tracks a branch, and core's tag is bumped per release. A
# hardcoded commit turns every routine bump into a failure of this suite, which
# is what happened. The fork's rev moved and the literal here did not, so the
# check reported a defect that was only a stale expectation.
#
# This still asserts the real property. The commit comes from the lockfile, not
# from the SBOM, so a purl naming a branch name, a truncated rev or the wrong
# package fails exactly as before.
locked_git_purl() {
  awk -v want="$1" '
    /^\[\[package\]\]/ { pkg = ""; ver = ""; next }
    /^name = /    { pkg = $3; gsub(/"/, "", pkg); next }
    /^version = / { ver = $3; gsub(/"/, "", ver); next }
    /^source = "git\+/ {
      if (pkg != want) next
      src = $0; sub(/^source = "/, "", src); sub(/"$/, "", src)
      repo = src; sub(/^git\+/, "", repo); sub(/[?#].*$/, "", repo)
      rev = src; sub(/^.*#/, "", rev)
      printf "pkg:cargo/%s@%s?vcs_url=git+%s@%s\n", pkg, ver, repo, rev
      exit
    }
  ' "$REPO_ROOT/Cargo.lock"
}

FORK_PURL="$(locked_git_purl trino-rust-client)"
CORE_PURL="$(locked_git_purl stackable-odbc-core)"

# Aborts rather than running on. An empty expectation compares equal to nothing
# the SBOM emits, so the checks below would still fail, but they would blame
# the SBOM for a lockfile the helper could not read. Once either dependency
# moves to crates.io, delete its checks rather than letting this fire.
for named in "trino-rust-client=$FORK_PURL" "stackable-odbc-core=$CORE_PURL"; do
  if [ -z "${named#*=}" ]; then
    echo "ERROR: Cargo.lock records no git source for ${named%%=*}." >&2
    exit 1
  fi
done

if [ ! -f "$SO" ]; then
  echo "Building the release artifact with cargo auditable..."
  (cd "$REPO_ROOT" && cargo auditable build --locked --release)
fi

"$REPO_ROOT/packaging/sbom.sh" "$SO" "$WORK"
SBOM="$WORK/libstackable_odbc_trino.so.cdx.json"

SPDX="$WORK/libstackable_odbc_trino.so.spdx.json"

check "SBOM file is written" "$([ -f "$SBOM" ] && echo yes || echo no)" "yes"
check "SPDX file is written" "$([ -f "$SPDX" ] && echo yes || echo no)" "yes"
check "component count" "$(jq '.components | length' "$SBOM")" "167"

check "every component is licensed" \
  "$(jq '[.components[] | select((.licenses // []) | length == 0)] | length' "$SBOM")" "0"

check "no bare pkg:cargo purl on a git or path dep" \
  "$(jq '[.components[] | select(.purl != null)
         | select(.name == "trino-rust-client" or .name == "stackable-odbc-core")
         | select(.purl | test("^pkg:cargo/[^?]*$"))] | length' "$SBOM")" "0"

check "the fork purl names an immutable commit" \
  "$(jq -r '.components[] | select(.name == "trino-rust-client") | .purl' "$SBOM")" \
  "$FORK_PURL"

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

check "the artifact is the SBOM subject" \
  "$(jq -r '.metadata.component.name' "$SBOM")" "libstackable_odbc_trino.so"

check "the subject carries a sha256" \
  "$(jq -r '.metadata.component.hashes[]? | select(.alg == "SHA-256") | .content' "$SBOM" | tr -d '\n' | wc -c)" "64"

check "no absolute build path leaks" \
  "$(jq -r '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$SBOM")" "0"

check "the rust toolchain is recorded" \
  "$(jq '[.metadata.properties[]? | select(.name == "stackable:rustc-version")] | length' "$SBOM")" "1"

# One: stackable-odbc-trino, the root package, which is path-local permanently.
# The gate is not "zero path components": it is that the only path-sourced
# component is the root package, which is what catches a developer's local
# `[patch]` override shipping in a release artifact.
check "path-sourced components" \
  "$(jq '[.components[].properties[]? | select(.name == "stackable:cargo-source" and .value == "path")] | length' "$SBOM")" "1"

check "the only path-sourced component is the root package" \
  "$(jq -r '[.components[]
             | select(.properties[]? | select(.name == "stackable:cargo-source" and .value == "path"))
             | .name] | join(",")' "$SBOM")" \
  "stackable-odbc-trino"

# Core is pinned by tag, and the purl must still name the commit that tag
# resolved to rather than the tag itself: a tag can be moved, so a purl carrying
# `@v0.1.0` would not identify the source the artifact was built from.
check "core's purl names an immutable commit" \
  "$(jq -r '.components[] | select(.name == "stackable-odbc-core") | .purl' "$SBOM")" \
  "$CORE_PURL"

# --- SPDX ------------------------------------------------------------------
# SPDX is converted from the enriched CycloneDX rather than generated afresh, so
# the enrichment reaches both formats from one implementation. These assert the
# conversion carries it across.

check "SPDX carries the enriched licenses" \
  "$(jq '[.packages[] | select((.licenseDeclared // "NOASSERTION") == "NOASSERTION")] | length' "$SPDX")" "2"

check "SPDX carries the fork's vcs_url purl" \
  "$(jq -r '[.packages[] | select(.name == "trino-rust-client") | .externalRefs[]? | select(.referenceType == "purl") | .referenceLocator] | first' "$SPDX")" \
  "$FORK_PURL"

check "SPDX carries the native component" \
  "$(jq '[.packages[] | select(.name == "unixodbc")] | length' "$SPDX")" "1"

check "SPDX leaks no build path" \
  "$(jq '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$SPDX")" "0"

# --- --check-native --------------------------------------------------------

check "--check-native passes on the current fragment" \
  "$("$REPO_ROOT/packaging/sbom.sh" --check-native "$SO" >/dev/null 2>&1 && echo ok || echo failed)" "ok"

# Drift must be detected, not tolerated. Feed it a fragment with the entry
# removed and require a non-zero exit.
jq 'del(.linux[0])' "$REPO_ROOT/packaging/sbom-native.json" > "$WORK/drifted.json"
check "--check-native detects a missing entry" \
  "$(SBOM_NATIVE="$WORK/drifted.json" "$REPO_ROOT/packaging/sbom.sh" --check-native "$SO" >/dev/null 2>&1 && echo ok || echo failed)" "failed"

# The wrong soname must be caught too, which is the mistake of naming libodbc
# where the artifact links libodbcinst.
jq '.linux[0].properties |= map(if .name == "stackable:soname" then .value = "libodbc.so.2" else . end)' \
  "$REPO_ROOT/packaging/sbom-native.json" > "$WORK/wrong-soname.json"
check "--check-native detects a wrong soname" \
  "$(SBOM_NATIVE="$WORK/wrong-soname.json" "$REPO_ROOT/packaging/sbom.sh" --check-native "$SO" >/dev/null 2>&1 && echo ok || echo failed)" "failed"

# The Windows branch asserts a different invariant: the mingw runtime must stay
# statically linked, because the archive ships no runtime DLL alongside it.
DLL="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll"
if [ -f "$DLL" ]; then
  check "--check-native passes on the Windows DLL" \
    "$("$REPO_ROOT/packaging/sbom.sh" --check-native "$DLL" >/dev/null 2>&1 && echo ok || echo failed)" "ok"

  # --- the Windows SBOM ----------------------------------------------------
  # Generated as well as checked, because the two artifact formats take
  # different branches through augment and finalize. Syft emits a second
  # self-entry of type "application" for the PE artifact, which the Linux run
  # never exercises.
  "$REPO_ROOT/packaging/sbom.sh" "$DLL" "$WORK" >/dev/null
  WSBOM="$WORK/stackable_odbc_trino.dll.cdx.json"

  check "Windows: every component is licensed" \
    "$(jq '[.components[] | select((.licenses // []) | length == 0)] | length' "$WSBOM")" "0"

  check "Windows: no self-entry survives" \
    "$(jq '[.components[] | select((.purl // "") == "")] | length' "$WSBOM")" "0"

  check "Windows: the toolchain runtime is declared" \
    "$(jq -r '[.components[] | select(.name == "mingw-w64-runtime" or .name == "libgcc") | .name] | sort | join(",")' "$WSBOM")" \
    "libgcc,mingw-w64-runtime"

  check "Windows: unixODBC is not merged in" \
    "$(jq '[.components[] | select(.name == "unixodbc")] | length' "$WSBOM")" "0"

  check "Windows: no absolute build path leaks" \
    "$(jq '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$WSBOM")" "0"

  check "Windows: the artifact is the SBOM subject" \
    "$(jq -r '.metadata.component.name' "$WSBOM")" "stackable_odbc_trino.dll"
else
  echo "SKIP  Windows checks: DLL not built (cargo auditable build --locked --release --target x86_64-pc-windows-gnu)"
fi

# --- the .mez ---------------------------------------------------------------
# The Power Query connector is M source in a zip, so syft finds nothing in it
# and there is no cargo graph to enrich. Its SBOM is built directly: the
# connector is the subject, and it has no dependencies.
MEZ="$REPO_ROOT/connector/bin/StackableTrinoODBC.mez"
if [ ! -f "$MEZ" ]; then
  (cd "$REPO_ROOT/connector" && ./build.sh >/dev/null 2>&1) || true
fi

if [ -f "$MEZ" ]; then
  "$REPO_ROOT/packaging/sbom.sh" "$MEZ" "$WORK" >/dev/null
  MSBOM="$WORK/StackableTrinoODBC.mez.cdx.json"
  MSPDX="$WORK/StackableTrinoODBC.mez.spdx.json"

  check "mez: CycloneDX is written" "$([ -f "$MSBOM" ] && echo yes || echo no)" "yes"
  check "mez: SPDX is written" "$([ -f "$MSPDX" ] && echo yes || echo no)" "yes"

  check "mez: the connector is the subject" \
    "$(jq -r '.metadata.component.name' "$MSBOM")" "StackableTrinoODBC.mez"

  check "mez: the subject carries a sha256" \
    "$(jq -r '.metadata.component.hashes[]? | select(.alg == "SHA-256") | .content' "$MSBOM" | tr -d '\n' | wc -c)" "64"

  # The version is read from the connector's own [Version = "..."], which
  # release.toml keeps in step with the crate version.
  check "mez: the subject version matches the .pq" \
    "$(jq -r '.metadata.component.version' "$MSBOM")" \
    "$(grep -oE '\[Version = "[^"]+"\]' "$REPO_ROOT/connector/StackableTrinoODBC.pq" | head -1 | sed -E 's/.*"(.*)".*/\1/')"

  # Zero is the honest answer, not a gap: the connector is pure M with no
  # third-party dependencies.
  check "mez: no components" "$(jq '.components | length' "$MSBOM")" "0"

  check "mez: no build path leaks" \
    "$(jq '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$MSBOM")" "0"
else
  echo "SKIP  mez checks: connector/bin/StackableTrinoODBC.mez could not be built"
fi

# An artifact built without cargo auditable must be refused, not silently turned
# into a near-empty SBOM. Strip the section to prove the guard fires.
cp "$SO" "$WORK/no-audit.so"
objcopy --remove-section=.dep-v0 "$WORK/no-audit.so" 2>/dev/null || true
check "an artifact without .dep-v0 is refused" \
  "$("$REPO_ROOT/packaging/sbom.sh" "$WORK/no-audit.so" "$WORK/refused" >/dev/null 2>&1 && echo ok || echo refused)" "refused"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "All checks passed."
else
  echo "$FAILURES check(s) failed."
  exit 1
fi
