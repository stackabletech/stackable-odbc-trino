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

# --- extract ---------------------------------------------------------------
syft "$ARTIFACT" -o cyclonedx-json="$OUT" --quiet

echo "Wrote $OUT"
