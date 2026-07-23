#!/bin/sh
# Post-remove script for nfpm-generated packages (.deb/.rpm/.pkg.tar.zst).

# The udev rule was removed with the package; reload so it stops applying.
udevadm control --reload || true
