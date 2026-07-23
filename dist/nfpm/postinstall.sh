#!/bin/sh
# Post-install script for nfpm-generated packages (.deb/.rpm/.pkg.tar.zst).
# Runs as root at package install time.

# Reload udev rules and apply them to already-present devices.
udevadm control --reload || true
udevadm trigger || true

# Load the virtual input modules now so no reboot is required
# (dist/moonshine-modules.conf takes care of subsequent boots).
modprobe uinput || true
modprobe uhid || true

echo "moonshine: enable for your user with:"
echo "  sudo loginctl enable-linger <user>   # optional, for headless use"
echo "  sudo systemctl enable --now moonshine@<user>"
