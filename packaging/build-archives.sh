#!/usr/bin/env bash
# Assemble release archives for stackable-odbc-trino.
#
# Preconditions:
#   - $VERSION unset, or set to the version in Cargo.toml. It defaults to that
#     version, so a local build needs no argument; release.yaml sets it from the
#     release tag, which its verify-version job has already checked against
#     Cargo.toml.
#   - target/release/libstackable_odbc_trino.so exists
#   - target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll exists
#   - both built with `cargo auditable`, which embeds the .dep-v0 section the
#     SBOM is generated from. sbom.sh refuses an artifact without it.
#   - syft on PATH
#
# Output (written to packaging/dist/):
#   - stackable-odbc-trino-<version>-linux-x64.tar.gz
#   - stackable-odbc-trino-<version>-windows-x64.zip  (includes StackableTrinoODBC.mez)
#   - StackableTrinoODBC-<version>.mez                          (standalone Power BI asset)
#   - a CycloneDX and an SPDX SBOM per artifact, six files
#   - sha256sums.txt over everything above
#
# Each archive also carries the CycloneDX SBOM for what it contains, so an
# offline or air-gapped install has it without going back to the release page.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$REPO_ROOT/packaging/dist"
LINUX_SO="$REPO_ROOT/target/release/libstackable_odbc_trino.so"
WINDOWS_DLL="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll"
LICENSE_FILE="$REPO_ROOT/LICENSE"
PACKAGING_DIR="$REPO_ROOT/packaging"
CONNECTOR_DIR="$REPO_ROOT/connector"
MEZ_SOURCE="$CONNECTOR_DIR/bin/StackableTrinoODBC.mez"

if [ ! -f "$LINUX_SO" ]; then
  echo "ERROR: $LINUX_SO not found. Run 'cargo auditable build --locked --release' first." >&2
  exit 1
fi
if [ ! -f "$WINDOWS_DLL" ]; then
  echo "ERROR: $WINDOWS_DLL not found. Run 'cargo auditable build --locked --release --target x86_64-pc-windows-gnu' first." >&2
  exit 1
fi
if [ ! -f "$LICENSE_FILE" ]; then
  echo "ERROR: LICENSE file not found at $LICENSE_FILE" >&2
  exit 1
fi

# $VERSION names the archives; the crate version is what build.rs compiled into
# the DLL's VERSIONINFO resource and what the .pq carries. Nothing links the
# two, so without this an archive called 0.1.0 can hold a DLL the ODBC Data
# Source Administrator lists as 0.0.1, and the mismatch is invisible until
# someone reads a bug report.
#
# The crate version is therefore the default rather than something to be typed:
# it is the only value that can be correct, so requiring it as an argument only
# creates the chance of getting it wrong. release.yaml still passes $VERSION
# explicitly, from a tag its verify-version job has already compared against
# Cargo.toml, and that agreeing value is checked here rather than assumed.
CRATE_VERSION="$(cargo metadata --no-deps --format-version 1 --manifest-path "$REPO_ROOT/Cargo.toml" | jq -r '.packages[0].version')"
VERSION="${VERSION:-$CRATE_VERSION}"
if [ "$VERSION" != "$CRATE_VERSION" ]; then
  echo "ERROR: VERSION=$VERSION but Cargo.toml declares $CRATE_VERSION." >&2
  echo "  The archives would be named $VERSION while the DLL's version resource" >&2
  echo "  and connector/StackableTrinoODBC.pq both report $CRATE_VERSION." >&2
  echo "  Release with 'release/release.sh <patch|minor|major> --execute', which" >&2
  echo "  bumps all three together, or leave VERSION unset to package the tree" >&2
  echo "  as it stands." >&2
  exit 1
fi

# Build the .mez using the connector's own build script.
(cd "$CONNECTOR_DIR" && ./build.sh)
if [ ! -f "$MEZ_SOURCE" ]; then
  echo "ERROR: connector build did not produce $MEZ_SOURCE" >&2
  exit 1
fi

# Cleared, not merely created. `sha256sums.txt` below globs the whole
# directory, so artefacts left by a run at a different $VERSION would be
# checksummed into this release's manifest, and the "Built:" count would
# describe a directory rather than a release. CI starts from a fresh checkout
# and never sees this; the person following packaging/README.md does.
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

# --- SBOMs ---
# Generated first, because each archive carries the one describing its contents.
# sbom.sh writes <basename>.cdx.json and <basename>.spdx.json.
SBOM_DIR="$DIST_DIR/sbom"
rm -rf "$SBOM_DIR"
mkdir -p "$SBOM_DIR"

"$PACKAGING_DIR/sbom.sh" "$LINUX_SO" "$SBOM_DIR"
"$PACKAGING_DIR/sbom.sh" "$WINDOWS_DLL" "$SBOM_DIR"
"$PACKAGING_DIR/sbom.sh" "$MEZ_SOURCE" "$SBOM_DIR"

LINUX_SBOM="$SBOM_DIR/$(basename "$LINUX_SO").cdx.json"
WINDOWS_SBOM="$SBOM_DIR/$(basename "$WINDOWS_DLL").cdx.json"
MEZ_SBOM="$SBOM_DIR/$(basename "$MEZ_SOURCE").cdx.json"

# --- Linux archive ---
LINUX_STAGING="$DIST_DIR/staging-linux"
rm -rf "$LINUX_STAGING"
mkdir -p "$LINUX_STAGING"
cp "$LINUX_SO" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/linux/install.sh" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/linux/uninstall.sh" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/README.md" "$LINUX_STAGING/"
cp "$LICENSE_FILE" "$LINUX_STAGING/"
cp "$LINUX_SBOM" "$LINUX_STAGING/"
chmod +x "$LINUX_STAGING/install.sh" "$LINUX_STAGING/uninstall.sh"

LINUX_ARCHIVE="stackable-odbc-trino-${VERSION}-linux-x64.tar.gz"
tar -czf "$DIST_DIR/$LINUX_ARCHIVE" -C "$LINUX_STAGING" .
rm -rf "$LINUX_STAGING"

# --- Windows archive (includes StackableTrinoODBC.mez) ---
WINDOWS_STAGING="$DIST_DIR/staging-windows"
rm -rf "$WINDOWS_STAGING"
mkdir -p "$WINDOWS_STAGING"
cp "$WINDOWS_DLL" "$WINDOWS_STAGING/"
cp "$MEZ_SOURCE" "$WINDOWS_STAGING/StackableTrinoODBC.mez"
cp "$PACKAGING_DIR/windows/install.bat" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/uninstall.bat" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/configure-dsn.ps1" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/README.md" "$WINDOWS_STAGING/"
cp "$LICENSE_FILE" "$WINDOWS_STAGING/"
# Two, because this archive ships both the driver and the connector.
cp "$WINDOWS_SBOM" "$MEZ_SBOM" "$WINDOWS_STAGING/"

WINDOWS_ARCHIVE="stackable-odbc-trino-${VERSION}-windows-x64.zip"
(cd "$WINDOWS_STAGING" && zip -r "$DIST_DIR/$WINDOWS_ARCHIVE" .)
rm -rf "$WINDOWS_STAGING"

# --- Standalone .mez asset ---
STANDALONE_MEZ="StackableTrinoODBC-${VERSION}.mez"
cp "$MEZ_SOURCE" "$DIST_DIR/$STANDALONE_MEZ"

# --- SBOMs as release assets ---
# Named with the version, so an asset downloaded on its own still says which
# release it describes.
for fmt in cdx spdx; do
  cp "$SBOM_DIR/$(basename "$LINUX_SO").$fmt.json" \
     "$DIST_DIR/stackable-odbc-trino-${VERSION}-linux-x64.$fmt.json"
  cp "$SBOM_DIR/$(basename "$WINDOWS_DLL").$fmt.json" \
     "$DIST_DIR/stackable-odbc-trino-${VERSION}-windows-x64.$fmt.json"
  cp "$SBOM_DIR/$(basename "$MEZ_SOURCE").$fmt.json" \
     "$DIST_DIR/StackableTrinoODBC-${VERSION}.$fmt.json"
done
rm -rf "$SBOM_DIR"

# --- Checksums ---
# Over every published file, generated last so it covers the SBOMs too. Paths
# are relative, so `sha256sum -c sha256sums.txt` works from the download
# directory.
(cd "$DIST_DIR" && sha256sum ./*.tar.gz ./*.zip ./*.mez ./*.json > sha256sums.txt)

echo "Built:"
echo "  $DIST_DIR/$LINUX_ARCHIVE"
echo "  $DIST_DIR/$WINDOWS_ARCHIVE"
echo "  $DIST_DIR/$STANDALONE_MEZ"
echo "  $DIST_DIR/sha256sums.txt  ($(wc -l < "$DIST_DIR/sha256sums.txt") entries)"
