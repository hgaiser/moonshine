#!/usr/bin/env bash
#
# build.sh — Build a .deb package for Moonshine
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

echo "Building moonshine_${VERSION}_amd64.deb"

# --- Build binaries ---
echo "Building moonshine..."
cargo build --release

echo "Building moonshine-wsi..."
cargo build --release -p moonshine-wsi

# --- Create staging directory ---
STAGING="debian/staging"
rm -rf "$STAGING"
mkdir -p "$STAGING"/{DEBIAN,usr/bin,usr/lib/moonshine/vulkan-layers,usr/lib/modules-load.d,usr/lib/systemd/system,usr/share/vulkan/explicit_layer.d,usr/share/applications,usr/share/metainfo,usr/share/man/man1,usr/share/icons/hicolor/scalable/apps,lib/udev/rules.d}

# --- Copy binaries ---
cp target/release/moonshine "$STAGING/usr/lib/moonshine/moonshine"
cp target/release/libmoonshine_wsi.so "$STAGING/usr/lib/moonshine/vulkan-layers/libmoonshine_wsi.so"

# --- Wrapper script (defaults config to ~/.config/moonshine/config.toml) ---
cat > "$STAGING/usr/bin/moonshine" <<'WRAPPER'
#!/bin/sh
CONFIG="${1:-$HOME/.config/moonshine/config.toml}"
exec /usr/lib/moonshine/moonshine "$CONFIG"
WRAPPER
chmod +x "$STAGING/usr/bin/moonshine"

# --- Copy dist/ assets ---
cp dist/start-moonshine.sh "$STAGING/usr/bin/start-moonshine.sh"
chmod +x "$STAGING/usr/bin/start-moonshine.sh"
cp dist/moonshine@.service "$STAGING/usr/lib/systemd/system/moonshine@.service"
cp dist/60-moonshine.rules "$STAGING/lib/udev/rules.d/60-moonshine.rules"
cp dist/moonshine-modules.conf "$STAGING/usr/lib/modules-load.d/moonshine-modules.conf"
cp dist/VkLayer_moonshine_wsi.json "$STAGING/usr/share/vulkan/explicit_layer.d/VkLayer_moonshine_wsi.json"
cp dist/dev.lizardbyte.app.Moonshine.desktop "$STAGING/usr/share/applications/dev.lizardbyte.app.Moonshine.desktop"
cp dist/dev.lizardbyte.app.Moonshine.metainfo.xml "$STAGING/usr/share/metainfo/dev.lizardbyte.app.Moonshine.metainfo.xml"
cp dist/moonshine-launcher.sh "$STAGING/usr/bin/moonshine-launcher.sh"
chmod +x "$STAGING/usr/bin/moonshine-launcher.sh"
cp dist/moonshine.1 "$STAGING/usr/share/man/man1/moonshine.1"
cp dist/moonshine.svg "$STAGING/usr/share/icons/hicolor/scalable/apps/dev.lizardbyte.app.Moonshine.svg"

# --- Compress man page ---
gzip -9n "$STAGING/usr/share/man/man1/moonshine.1"

# --- DEBIAN/control ---
cat > "$STAGING/DEBIAN/control" <<EOF
Package: moonshine
Version: ${VERSION}
Section: games
Priority: optional
Architecture: amd64
Depends: libc6, libdrm2, libevdev2, libexpat1, libgbm1, libopus0, libpulse0, libshaderc1, libvulkan1, libwayland-client0, libxkbcommon0, systemd
Recommends: udev
Homepage: https://github.com/hgaiser/moonshine
Maintainer: Mario Lameiras
Standards-Version: 4.6.2
Description: Game streaming server (Moonlight host)
 Moonshine lets you stream games from your PC to any device
 running the Moonlight client. Each stream runs in its own
 isolated Wayland compositor.
 .
 Features:
  - Isolated streaming sessions per application
  - Headless server support (no monitor required)
  - Hardware video encoding (H.264, H.265, AV1)
  - HDR and full input support
 .
 After installation, enable the service with:
  sudo loginctl enable-linger \$USER
  sudo systemctl enable --now moonshine@\$USER
EOF

# --- DEBIAN/changelog ---
cat > "$STAGING/DEBIAN/changelog" <<EOF
moonshine (${VERSION}-1) unstable; urgency=medium

  * Initial Ubuntu/Debian packaging release.
  * Includes moonshine binary, Vulkan WSI layer, systemd service,
    udev rules, and default configuration.

 -- Mario Lameiras <mario@example.com>  $(date -R)
EOF
gzip -9n "$STAGING/DEBIAN/changelog"

# --- DEBIAN/copyright ---
cat > "$STAGING/DEBIAN/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: moonshine
Upstream-Contact: https://github.com/hgaiser/moonshine
Source: https://github.com/hgaiser/moonshine

Files: *
Copyright: 2024-2026 Moonshine contributors
License: MIT

License: MIT
 Permission is hereby granted, free of charge, to any person obtaining a copy
 of this software and associated documentation files (the "Software"), to deal
 in the Software without restriction, including without limitation the rights
 to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 copies of the Software, and to permit persons to whom the Software is
 furnished to do so, subject to the following conditions:
 .
 The above copyright notice and this permission notice shall be included in all
 copies or substantial portions of the Software.
 .
 THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 SOFTWARE.
EOF

# --- DEBIAN/postinst ---
cat > "$STAGING/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "configure" ]; then
	# Reload udev rules so input device permissions take effect.
	udevadm control --reload
	udevadm trigger --subsystem-match=misc --subsystem-match=input

	# Print setup instructions for the installing user.
	if [ -n "$SUDO_USER" ]; then
		INSTALL_USER="$SUDO_USER"
	elif [ -n "$PKGMGR_USER" ]; then
		INSTALL_USER="$PKGMGR_USER"
	else
		INSTALL_USER=""
	fi

	if [ -n "$INSTALL_USER" ]; then
		cat <<MSG
Moonshine installed successfully!

To set up the streaming service for user '$INSTALL_USER':

  1. Enable user lingering (allows service to run without active login):
     sudo loginctl enable-linger $INSTALL_USER

  2. Enable and start the service:
     sudo systemctl enable --now moonshine@$INSTALL_USER

  3. Connect with Moonlight client. A configuration file will be created
     automatically at ~/.config/moonshine/config.toml on first run.

  4. Pair your client: visit http://localhost:47989/pin or use the PIN
     prompt when connecting from Moonlight.

For more info: man moonshine
MSG
	fi
fi
EOF
chmod 755 "$STAGING/DEBIAN/postinst"

# --- DEBIAN/prerm ---
cat > "$STAGING/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "remove" ] || [ "$1" = "deconfigure" ]; then
	# Stop any running moonshine user services.
	for dir in /run/user/*/; do
		uid=$(basename "$dir")
		if [ -d "$dir/systemd" ]; then
			runuser -u "#$uid" -- systemctl stop 'moonshine@*.service' 2>/dev/null || true
		fi
	done
fi
EOF
chmod 755 "$STAGING/DEBIAN/prerm"

# --- DEBIAN/postrm ---
cat > "$STAGING/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "purge" ]; then
	# Clean up udev rules on purge.
	udevadm control --reload
	udevadm trigger --subsystem-match=misc --subsystem-match=input 2>/dev/null || true
fi
EOF
chmod 755 "$STAGING/DEBIAN/postrm"

# --- Build .deb ---
echo "Packaging..."
dpkg-deb --root-owner-group --build "$STAGING" "moonshine_${VERSION}_amd64.deb"

echo "Done: moonshine_${VERSION}_amd64.deb ($(du -h "moonshine_${VERSION}_amd64.deb" | cut -f1))"
