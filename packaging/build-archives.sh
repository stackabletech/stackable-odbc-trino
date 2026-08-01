#!/usr/bin/env bash
# Assemble release archives for stackable-odbc-trino.
#
# Preconditions:
#   - $VERSION environment variable set (e.g. "1.0.0-beta.1")
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

: "${VERSION:?VERSION environment variable must be set}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$REPO_ROOT/packaging/dist"
LINUX_SO="$REPO_ROOT/target/release/libstackable_odbc_trino.so"
WINDOWS_DLL="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll"
LICENSE_FILE="$REPO_ROOT/LICENSE"
PACKAGING_DIR="$REPO_ROOT/packaging"
CONNECTOR_DIR="$REPO_ROOT/connector"
MEZ_SOURCE="$CONNECTOR_DIR/bin/StackableTrinoODBC.mez"

if [ ! -f "$LINUX_SO" ]; then
  echo "ERROR: $LINUX_SO not found. Run 'cargo auditable build --release' first." >&2
  exit 1
fi
if [ ! -f "$WINDOWS_DLL" ]; then
  echo "ERROR: $WINDOWS_DLL not found. Run 'cargo auditable build --release --target x86_64-pc-windows-gnu' first." >&2
  exit 1
fi
if [ ! -f "$LICENSE_FILE" ]; then
  echo "ERROR: LICENSE file not found at $LICENSE_FILE" >&2
  exit 1
fi

# Build the .mez using the connector's own build script.
(cd "$CONNECTOR_DIR" && ./build.sh)
if [ ! -f "$MEZ_SOURCE" ]; then
  echo "ERROR: connector build did not produce $MEZ_SOURCE" >&2
  exit 1
fi

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
# Renamed to carry the version, so an asset downloaded on its own still says
# which release it describes.
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
