#!/usr/bin/env bash
# Generate a CycloneDX SBOM for one release artifact.
#
# Usage:
#   sbom.sh <artifact> <outdir>     write <outdir>/<basename>.cdx.json
#
# The artifact must be built with `cargo auditable`, which embeds a .dep-v0
# section holding the crates that were linked in. Syft reads that section, so the
# component list describes what shipped rather than what Cargo.toml asked for,
# and dev-dependencies are excluded by construction.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Overridable so the tests can feed a drifted fragment on purpose.
SBOM_NATIVE="${SBOM_NATIVE:-$REPO_ROOT/packaging/sbom-native.json}"

usage() {
  cat >&2 <<'EOF'
usage: sbom.sh <artifact> <outdir>
           write <outdir>/<basename>.cdx.json

       sbom.sh --check-native <artifact>
           verify sbom-native.json against what the artifact actually links
EOF
  exit 2
}

# Libraries supplied by the toolchain and libc are the platform, not components,
# so they are excluded the same way the Windows branch excludes the operating
# system's own DLLs. Everything else the ELF object needs at load time must be
# declared in the fragment.
IGNORED_SONAMES='^(libc\.so\.|libm\.so\.|libpthread\.so\.|libdl\.so\.|librt\.so\.|libgcc_s\.so\.|ld-linux)'

# The Windows artifact declares no load-time component at all, because it
# imports only the operating system's libraries. What must hold instead is that
# the toolchain runtime stays *statically* linked: the release archive ships no
# runtime DLL, so an artifact importing one would fail to load on a machine
# without mingw installed.
FORBIDDEN_WINDOWS_IMPORTS='^(libgcc_s_seh-1|libgcc_s_dw2-1|libwinpthread-1|libstdc\+\+-6)\.dll$'

check_native_elf() {
  local artifact="$1" needed declared
  needed="$(readelf -d "$artifact" \
    | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' \
    | grep -Ev "$IGNORED_SONAMES" \
    | sort)"
  declared="$(jq -r '.linux[].properties[]? | select(.name == "stackable:soname") | .value' \
    "$SBOM_NATIVE" | sort)"

  if [ "$needed" = "$declared" ]; then
    echo "PASS: sbom-native.json matches the artifact's DT_NEEDED set"
    return 0
  fi

  echo "FAIL: sbom-native.json has drifted from $artifact" >&2
  echo "  linked but undeclared:" >&2
  comm -23 <(echo "$needed") <(echo "$declared") | sed 's/^/    /' >&2
  echo "  declared but not linked:" >&2
  comm -13 <(echo "$needed") <(echo "$declared") | sed 's/^/    /' >&2
  return 1
}

check_native_pe() {
  local artifact="$1" dynamic
  dynamic="$(objdump -p "$artifact" \
    | sed -n 's/^\tDLL Name: //p' \
    | tr '[:upper:]' '[:lower:]' \
    | sort -u \
    | grep -E "$FORBIDDEN_WINDOWS_IMPORTS" || true)"

  if [ -z "$dynamic" ]; then
    echo "PASS: the toolchain runtime is statically linked into the artifact"
    return 0
  fi

  echo "FAIL: $artifact imports the toolchain runtime dynamically" >&2
  echo "$dynamic" | sed 's/^/    /' >&2
  echo "  The release archive ships no runtime DLL, so this artifact would fail" >&2
  echo "  to load on a machine without mingw installed. Either restore static" >&2
  echo "  linking or ship the runtime and declare it in sbom-native.json." >&2
  return 1
}

if [ "${1:-}" = "--check-native" ]; then
  [ "$#" -eq 2 ] || usage
  [ -f "$2" ] || { echo "ERROR: artifact not found: $2" >&2; exit 1; }
  case "$2" in
    *.so) check_native_elf "$2" ;;
    *.dll) check_native_pe "$2" ;;
    *) echo "ERROR: cannot check native links of $2" >&2; exit 1 ;;
  esac
  exit $?
fi

[ "$#" -eq 2 ] || usage

ARTIFACT="$1"
OUTDIR="$2"

[ -f "$ARTIFACT" ] || { echo "ERROR: artifact not found: $ARTIFACT" >&2; exit 1; }
mkdir -p "$OUTDIR"

BASENAME="$(basename "$ARTIFACT")"
OUT="$OUTDIR/$BASENAME.cdx.json"
OUT_SPDX="$OUTDIR/$BASENAME.spdx.json"

# An artifact built with plain `cargo build` carries no .dep-v0 section, and
# syft then reports a handful of components rather than the whole graph. That
# failure is silent and the result looks like a valid SBOM, so refuse it here
# rather than shipping a document that understates what is in the binary.
require_audit_section() {
  local artifact="$1" found=0
  case "$artifact" in
    *.so) found="$(readelf -S -W "$artifact" 2>/dev/null | grep -c '\.dep-v0' || true)" ;;
    *.dll) found="$(objdump -h "$artifact" 2>/dev/null | grep -c '\.dep-v0' || true)" ;;
    *) return 0 ;;
  esac

  if [ "${found:-0}" -eq 0 ]; then
    echo "ERROR: $artifact carries no .dep-v0 section." >&2
    echo "  It was built with plain cargo, so the dependency graph is not in it" >&2
    echo "  and the SBOM would list only a few components. Rebuild with:" >&2
    case "$artifact" in
      *.dll) echo "    cargo auditable build --locked --release --target x86_64-pc-windows-gnu" >&2 ;;
      *)     echo "    cargo auditable build --locked --release" >&2 ;;
    esac
    exit 1
  fi
}

# The Power Query connector is M source in a zip. Syft finds nothing in it and
# there is no cargo graph to enrich, so its document is built directly rather
# than run through the pipeline. An empty component list is the honest answer:
# the connector has no third-party dependencies.
build_mez_sbom() {
  local artifact="$1" sha version serial
  sha="$(sha256sum "$artifact" | cut -d' ' -f1)"

  # release.toml keeps this in step with the crate version; see AGENTS.md.
  version="$(grep -oE '\[Version = "[^"]+"\]' "$REPO_ROOT/connector/StackableTrinoODBC.pq" \
    | head -1 | sed -E 's/.*"(.*)".*/\1/')"
  [ -n "$version" ] || { echo "ERROR: no [Version = \"...\"] in StackableTrinoODBC.pq" >&2; exit 1; }

  # Derived from the artifact digest so the same input yields the same document.
  serial="urn:uuid:${sha:0:8}-${sha:8:4}-${sha:12:4}-${sha:16:4}-${sha:20:12}"

  jq -n --arg name "$BASENAME" --arg sha "$sha" --arg version "$version" \
        --arg serial "$serial" --arg rustc "$(rustc --version)" \
  '{
    bomFormat: "CycloneDX",
    specVersion: "1.7",
    serialNumber: $serial,
    version: 1,
    metadata: {
      component: {
        type: "application",
        name: $name,
        version: $version,
        description: "Power Query custom connector for the Stackable ODBC driver for Trino.",
        licenses: [ { license: { id: "Apache-2.0" } } ],
        hashes: [ { alg: "SHA-256", content: $sha } ]
      },
      properties: [ { name: "stackable:rustc-version", value: $rustc } ]
    },
    components: []
  }' > "$OUT"

  syft convert "$OUT" -o spdx-json="$OUT_SPDX" --quiet

  echo "Wrote $OUT"
  echo "Wrote $OUT_SPDX"
}

if [ "${BASENAME##*.}" = "mez" ]; then
  build_mez_sbom "$ARTIFACT"
  exit 0
fi

require_audit_section "$ARTIFACT"

RAW="$OUTDIR/.$BASENAME.raw.json"
LOOKUP="$OUTDIR/.$BASENAME.lookup.json"
ENRICHED="$OUTDIR/.$BASENAME.enriched.json"
AUGMENTED="$OUTDIR/.$BASENAME.augmented.json"

# --- extract ---------------------------------------------------------------
syft "$ARTIFACT" -o cyclonedx-json="$RAW" --quiet

# --- enrich ----------------------------------------------------------------
# cargo-auditable embeds only name, version and source kind, so syft's output
# carries no licenses, and a git or path dependency is indistinguishable from a
# crates.io package. A scanner resolving pkg:cargo/trino-rust-client@0.11.0
# would reach the real upstream crate, which is not what shipped.
#
# Everything below keys off cargo metadata's source *kind*, never off a crate
# name, so a dependency moving between path, git and crates.io needs no change
# here.
cargo metadata --locked --format-version 1 --manifest-path "$REPO_ROOT/Cargo.toml" \
  | jq '[ .packages[]
          | { key: "\(.name)@\(.version)",
              value: {
                license: .license,
                kind: (if .source == null then "path"
                       elif (.source | startswith("git+")) then "git"
                       else "registry" end),
                vcs: (if ((.source // "") | startswith("git+"))
                      then "git+" + (.source | sub("^git\\+"; "") | sub("[?#].*$"; ""))
                           + "@" + (.source | capture("#(?<rev>[0-9a-f]+)$").rev)
                      else null end)
              } } ] | from_entries' > "$LOOKUP"

# The rev comes from the resolved source in Cargo.lock, not from the branch or
# tag name, so the purl names an immutable commit.
jq --slurpfile lut "$LOOKUP" '
  ($lut[0]) as $L
  | .components |= map(
      . as $c
      | ($L["\($c.name)@\($c.version)"]) as $m
      | if $m == null then . else
          .licenses = (
            if $m.license == null then []
            elif ($m.license | test(" OR | AND |/"))
            then [ { expression: $m.license } ]
            else [ { license: { id: $m.license } } ] end)
          | .purl = (
              if $m.kind == "git" then "\(.purl)?vcs_url=\($m.vcs)"
              elif $m.kind == "path" then "pkg:generic/\(.name)@\(.version)"
              else .purl end)
          | .properties = (
              (.properties // [] | map(select(.name | startswith("syft:cpe23") | not)))
              + (if $m.kind == "path"
                 then [ { name: "stackable:cargo-source", value: "path" } ]
                 else [] end))
        end)' "$RAW" > "$ENRICHED"

# --- augment ---------------------------------------------------------------
# Components the toolchain contributes are invisible to cargo, and the two
# platforms contribute different ones: the ELF object links unixODBC at load
# time, while the Windows DLL imports only the operating system's own libraries
# and instead carries the mingw runtime statically.
case "$BASENAME" in
  *.so) NATIVE_KEY="linux" ;;
  *.dll) NATIVE_KEY="windows" ;;
  *) NATIVE_KEY="" ;;
esac

if [ -n "$NATIVE_KEY" ]; then
  jq --slurpfile native "$SBOM_NATIVE" \
     --arg key "$NATIVE_KEY" \
     '.components += ($native[0][$key] // [])' "$ENRICHED" > "$AUGMENTED"
else
  cp "$ENRICHED" "$AUGMENTED"
fi

# --- finalize --------------------------------------------------------------
# Syft reports the scanned artifact as an ordinary component: type "file" named
# by its absolute path on the build host, and for the PE artifact a second
# type "application" entry as well. Both are the *subject* of this document
# rather than dependencies, so they move to metadata.component, and the build
# path stops travelling with the release.
#
# They are selected by having no purl rather than by type, because the types
# differ between the two artifact formats. Every real component has one: the
# cargo crates from the enrich stage, the native ones from the fragment.
ARTIFACT_SHA="$(sha256sum "$ARTIFACT" | cut -d' ' -f1)"
RUSTC_VERSION="$(rustc --version)"

jq --arg name "$BASENAME" \
   --arg sha "$ARTIFACT_SHA" \
   --arg rustc "$RUSTC_VERSION" \
   '
   .components |= map(select((.purl // "") != ""))
   | .metadata.component = {
       type: "library",
       name: $name,
       hashes: [ { alg: "SHA-256", content: $sha } ]
     }
   | .metadata.properties = ((.metadata.properties // []) + [
       { name: "stackable:rustc-version", value: $rustc }
     ])' "$AUGMENTED" > "$OUT"

# --- convert ---------------------------------------------------------------
# SPDX is converted from the finished CycloneDX rather than generated afresh, so
# the enrichment and the native fragment reach both formats from one
# implementation and cannot drift apart. Some procurement processes ask for SPDX
# by name; CycloneDX is what ships inside the archive.
syft convert "$OUT" -o spdx-json="$OUT_SPDX" --quiet

rm -f "$RAW" "$LOOKUP" "$ENRICHED" "$AUGMENTED"

echo "Wrote $OUT"
echo "Wrote $OUT_SPDX"
