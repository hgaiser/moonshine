#!/bin/sh
# Post-install script for Pyroshine packages.

udevadm control --reload || true
udevadm trigger || true
systemd-sysusers 2>/dev/null || true
modprobe uinput || true
modprobe uhid || true

echo "pyroshine: enable for your user with:"
echo "  sudo loginctl enable-linger <user>   # optional, for headless use"
echo "  sudo systemctl enable --now pyroshine@<user>"
