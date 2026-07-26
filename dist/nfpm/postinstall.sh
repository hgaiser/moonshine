#!/bin/sh
# Post-install script for nfpm-generated packages (.deb/.rpm/.pkg.tar.zst).
# Runs as root at package install time.

# Reload udev rules and apply them to already-present devices.
udevadm control --reload || true
udevadm trigger || true

# The 'moonshine' group used by the suspend-inhibit polkit rule
# (dist/50-moonshine-inhibit-sleep.rules) is defined by the shipped sysusers.d
# drop-in (dist/moonshine-sysusers.conf -> /usr/lib/sysusers.d/moonshine.conf).
# Running systemd-sysusers applies that drop-in so the group exists immediately
# instead of only after the next boot.
systemd-sysusers 2>/dev/null || true

# Load the virtual input modules now so no reboot is required
# (dist/moonshine-modules.conf takes care of subsequent boots).
modprobe uinput || true
modprobe uhid || true

echo "moonshine: enable for your user with:"
echo "  sudo loginctl enable-linger <user>   # optional, for headless use"
echo "  sudo systemctl enable --now moonshine@<user>"
