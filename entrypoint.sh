#!/bin/bash

# Update UID/GID to match host (passed via environment variables)
USER_ID=${HOST_UID:-1000}
GROUP_ID=${HOST_GID:-1000}

if [ "$USER_ID" != "1000" ]; then
    groupmod -g $GROUP_ID moonshine
    usermod -u $USER_ID -g $GROUP_ID moonshine
    chown -R $USER_ID:$GROUP_ID /home/moonshine
fi

# Ensure D-Bus is running for the user session
mkdir -p /run/dbus
if [ ! -S /run/dbus/system_bus_socket ]; then
    dbus-daemon --system --fork
fi

# Execute CMD as the unprivileged user
exec gosu moonshine "$@"
