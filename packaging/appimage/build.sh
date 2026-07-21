#!/usr/bin/env bash
#
# build.sh — Build an AppImage for Moonshine
#
# Usage: ./build.sh [VERSION]
#   VERSION defaults to the version in Cargo.toml
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
	VERSION=$(grep -m1 '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
fi

echo "Building Moonshine-${VERSION}-x86_64.AppImage"

# --- Build binaries ---
echo "Building moonshine..."
cargo build --release

echo "Building moonshine-wsi..."
cargo build --release -p moonshine-wsi

# --- Create AppDir ---
APPDIR="AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib/moonshine" "$APPDIR/usr/lib/moonshine/vulkan-layers"

cp target/release/moonshine "$APPDIR/usr/lib/moonshine/moonshine"
cp target/release/libmoonshine_wsi.so "$APPDIR/usr/lib/moonshine/vulkan-layers/libmoonshine_wsi.so"

# --- AppRun ---
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/bash
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib/moonshine/vulkan-layers:${LD_LIBRARY_PATH:-}"
export VK_LAYER_PATH="$HERE/usr/share/vulkan/explicit_layer.d${VK_LAYER_PATH:+:$VK_LAYER_PATH}"
CONFIG="${1:-$HOME/.config/moonshine/config.toml}"
exec "$HERE/usr/lib/moonshine/moonshine" "$CONFIG"
EOF
chmod +x "$APPDIR/AppRun"

# --- Desktop file and icon ---
cp dist/dev.lizardbyte.app.Moonshine.desktop "$APPDIR/dev.lizardbyte.app.Moonshine.desktop"
mkdir -p "$APPDIR/usr/share/icons/hicolor/scalable/apps"
cp dist/moonshine.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/dev.lizardbyte.app.Moonshine.svg"

# --- Download linuxdeploy ---
LD_DIR="$REPO_ROOT/.appimage-tools"
mkdir -p "$LD_DIR"

if [[ ! -x "$LD_DIR/linuxdeploy-x86_64.AppImage" ]]; then
	echo "Downloading linuxdeploy..."
	curl -fSL -o "$LD_DIR/linuxdeploy-x86_64.AppImage" \
		"https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
	chmod +x "$LD_DIR/linuxdeploy-x86_64.AppImage"
fi

# --- Build AppImage ---
echo "Creating AppImage..."
APPIMAGE_EXTRACT_AND_RUN=1 "$LD_DIR/linuxdeploy-x86_64.AppImage" \
	--appdir="$APPDIR" \
	--desktop-file="$APPDIR/dev.lizardbyte.app.Moonshine.desktop" \
	--output=appimage

# Rename output
APPIMAGE_OUTPUT="Moonshine-${VERSION}-x86_64.AppImage"
if [[ -f "Moonshine-x86_64.AppImage" ]]; then
	mv "Moonshine-x86_64.AppImage" "$APPIMAGE_OUTPUT"
fi

echo "Done: $APPIMAGE_OUTPUT ($(du -h "$APPIMAGE_OUTPUT" | cut -f1))"
