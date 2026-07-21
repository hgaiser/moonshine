#!/bin/bash
#
# install-system.sh — Install system integration files for Moonshine AppImage
#
# Run with sudo after extracting the AppImage or alongside it.
# Installs: udev rules, kernel module config, systemd service, Vulkan layer.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

if [[ $EUID -ne 0 ]]; then
	echo "Error: This script must be run as root (sudo)" >&2
	exit 1
fi

echo "Installing Moonshine system integration files..."

# udev rules
cp "$REPO_ROOT/dist/60-moonshine.rules" /lib/udev/rules.d/
echo "  -> udev rules installed"

# Kernel modules
cp "$REPO_ROOT/dist/moonshine-modules.conf" /usr/lib/modules-load.d/
echo "  -> kernel module config installed"

# Systemd service
cp "$REPO_ROOT/dist/moonshine@.service" /usr/lib/systemd/user/
echo "  -> systemd user service installed"

# Vulkan layer
mkdir -p /usr/lib/moonshine/vulkan-layers
cp "$REPO_ROOT/target/release/libmoonshine_wsi.so" /usr/lib/moonshine/vulkan-layers/
cp "$REPO_ROOT/dist/VkLayer_moonshine_wsi.json" /usr/share/vulkan/explicit_layer.d/
echo "  -> Vulkan layer installed"

# Reload
udevadm control --reload
udevadm trigger --subsystem-match=misc --subsystem-match=input

echo ""
echo "System integration files installed."
echo "Enable the service with:"
echo "  sudo loginctl enable-linger \$USER"
echo "  sudo systemctl enable --now moonshine@\$USER"
