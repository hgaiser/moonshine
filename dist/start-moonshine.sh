#!/bin/bash
#
# Moonshine systemd service startup script
#
# This script sets up the required environment variables before launching moonshine.
# It's designed to work with systemd template services (moonshine@.service) where
# the service runs as the target user via the User= directive.
#
# Usage: start-moonshine.sh [moonshine arguments...]
#   All arguments are passed directly to moonshine.
#
# The script sets:
# - XDG_RUNTIME_DIR: Required for Wayland socket creation and systemd-run --user.
# - DBUS_SESSION_BUS_ADDRESS: Required for applications to connect to D-Bus.
#

set -e

# Get the numeric UID of the current user.
# The service runs as the target user via User=%i directive.
USER_UID=$(id -u)
USERNAME=$(id -un)

# Set XDG_RUNTIME_DIR if not already set.
# This is required for:
# - Wayland socket creation (ListeningSocketSource::new_auto)
# - systemd-run --user (to connect to user's systemd instance)
# - PulseAudio and other application sockets
if [ -z "$XDG_RUNTIME_DIR" ]; then
    export XDG_RUNTIME_DIR="/run/user/${USER_UID}"
fi

# Set DBUS_SESSION_BUS_ADDRESS if not already set.
# This is required for applications launched via systemd-run --user
# to connect to the D-Bus session bus.
if [ -z "$DBUS_SESSION_BUS_ADDRESS" ]; then
    export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
fi

# Verify runtime directory exists.
# If it's missing, ensure user lingering is enabled (`sudo loginctl enable-linger <username>`).
if [ ! -d "$XDG_RUNTIME_DIR" ]; then
    echo "ERROR: Runtime directory does not exist: $XDG_RUNTIME_DIR" >&2
    echo "Check if your user's systemd instance is active and lingering is enabled:" >&2
    echo "  sudo loginctl enable-linger ${USERNAME}" >&2
    exit 1
fi

# Verify D-Bus socket exists.
DBUS_SOCKET="${XDG_RUNTIME_DIR}/bus"
if [ ! -S "$DBUS_SOCKET" ]; then
    echo "WARNING: D-Bus socket not found: $DBUS_SOCKET" >&2
    echo "Applications may fail to connect to D-Bus" >&2
fi

# Log environment for debugging.
echo "Moonshine starting with:" >&2
echo "  User: $USERNAME (UID $USER_UID)" >&2
echo "  Arguments: $*" >&2
echo "  XDG_RUNTIME_DIR: $XDG_RUNTIME_DIR" >&2
echo "  DBUS_SESSION_BUS_ADDRESS: $DBUS_SESSION_BUS_ADDRESS" >&2

# Execute moonshine with all arguments and environment variables preserved.
exec /usr/bin/moonshine "$@"
