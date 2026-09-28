#!/bin/sh
# Post-remove script for Pyroshine packages.

udevadm control --reload || true
