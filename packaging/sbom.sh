#!/usr/bin/env bash
# Generate a CycloneDX SBOM for one release artifact.
#
# Usage:
#   sbom.sh <artifact> <outdir>     write <outdir>/<basename>.cdx.json
#
# The artifact must be built with `cargo auditable`, which embeds a .dep-v0
# section holding the crates actually linked in. Syft reads that section, so the
# component list describes what shipped rather than what Cargo.toml asked for,
# and dev-dependencies are excluded by construction.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  echo "usage: $0 <artifact> <outdir>" >&2
  exit 2
}

[ "$#" -eq 2 ] || usage

ARTIFACT="$1"
OUTDIR="$2"

[ -f "$ARTIFACT" ] || { echo "ERROR: artifact not found: $ARTIFACT" >&2; exit 1; }
mkdir -p "$OUTDIR"

BASENAME="$(basename "$ARTIFACT")"
OUT="$OUTDIR/$BASENAME.cdx.json"

RAW="$OUTDIR/.$BASENAME.raw.json"
LOOKUP="$OUTDIR/.$BASENAME.lookup.json"

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
        end)' "$RAW" > "$OUT"

rm -f "$RAW" "$LOOKUP"

echo "Wrote $OUT"
