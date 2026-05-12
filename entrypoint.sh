#!/bin/bash

# Update UID/GID to match host (passed via environment variables)
USER_ID=${HOST_UID:-1000}
GROUP_ID=${HOST_GID:-1000}

if [ "$USER_ID" != "1000" ]; then
    groupmod -g $GROUP_ID moonshine
    usermod -u $USER_ID -g $GROUP_ID moonshine
fi

# Unconditionally take ownership of the home directory
# so that docker-created bind mounts get the correct permissions.
chown -R $USER_ID:$GROUP_ID /home/moonshine

# Ensure D-Bus and Avahi are running for zeroconf discovery
mkdir -p /run/dbus /var/run/avahi-daemon
if [ ! -S /run/dbus/system_bus_socket ]; then
    dbus-daemon --system --fork
fi
if ! pgrep -x "avahi-daemon" > /dev/null; then
    avahi-daemon --daemonize --no-drop-root
fi

# Create XDG_RUNTIME_DIR to prevent PulseAudio socket permission errors
mkdir -p /run/user/$USER_ID
chown -R $USER_ID:$GROUP_ID /run/user/$USER_ID

# Set proper environment variables for the unprivileged user before preserving them
export HOME=/home/moonshine
export USER=moonshine
export XDG_RUNTIME_DIR=/run/user/$USER_ID

# Execute CMD as the unprivileged user
exec sudo -E -u moonshine -- "$@"
