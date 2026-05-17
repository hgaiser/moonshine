#!/bin/bash
# Moonshine container entrypoint.
# Sets up the runtime environment and launches moonshine directly.
# Uses a systemd-run shim to avoid requiring a full systemd instance.

set -e

USER_ID=${HOST_UID:-1000}
GROUP_ID=${HOST_GID:-1000}

# Remap the container user to match the host UID/GID if needed.
if [ "$USER_ID" != "1000" ]; then
    groupmod -g "$GROUP_ID" moonshine 2>/dev/null || true
    usermod -u "$USER_ID" -g "$GROUP_ID" moonshine
fi

# Fix ownership of home directory and any bind-mounted paths within it.
chown -R "$USER_ID:$GROUP_ID" /home/moonshine

# Start D-Bus system bus (required by avahi).
mkdir -p /run/dbus
if [ ! -S /run/dbus/system_bus_socket ]; then
    dbus-daemon --system --fork
fi

# Start Avahi for zeroconf/mDNS discovery.
if ! pgrep -x "avahi-daemon" > /dev/null; then
    avahi-daemon --daemonize --no-drop-root 2>/dev/null || true
fi

# Create XDG_RUNTIME_DIR.
mkdir -p "/run/user/$USER_ID"
chown "$USER_ID:$GROUP_ID" "/run/user/$USER_ID"
chmod 700 "/run/user/$USER_ID"

# Create XWayland socket directory.
mkdir -p /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix

# Dynamically add the moonshine user to groups that own GPU and input devices.
ensure_device_access() {
    local dev="$1"
    if [ -e "$dev" ]; then
        local dev_gid
        dev_gid=$(stat -c '%g' "$dev")
        if ! id -G moonshine | tr ' ' '\n' | grep -qx "$dev_gid"; then
            groupadd -g "$dev_gid" -o "hostdev_${dev_gid}" 2>/dev/null || true
            usermod -aG "hostdev_${dev_gid}" moonshine
        fi
    fi
}

for dev in /dev/dri/card* /dev/dri/renderD*; do
    ensure_device_access "$dev"
done
ensure_device_access /dev/uinput

# Auto-detect GPU vendor and disable conflicting EGL/Vulkan ICDs.
if [ ! -e /dev/nvidia0 ] && [ ! -d /proc/driver/nvidia ]; then
    echo "No NVIDIA GPU detected, disabling NVIDIA EGL/Vulkan ICDs."
    rm -f /usr/share/glvnd/egl_vendor.d/10_nvidia.json
    rm -f /usr/share/vulkan/icd.d/nvidia_icd.json
    rm -f /usr/share/vulkan/implicit_layer.d/nvidia_layers.json
fi

# Set environment for the unprivileged user.
export HOME=/home/moonshine
export USER=moonshine
export XDG_RUNTIME_DIR="/run/user/$USER_ID"
export DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$USER_ID/bus"
export RUST_LOG="${RUST_LOG:-info}"

# Start a session D-Bus bus for the user (needed by applications like Steam).
sudo -u moonshine dbus-daemon --session \
    --address="$DBUS_SESSION_BUS_ADDRESS" \
    --fork --print-pid 2>/dev/null || true

# Execute moonshine as the unprivileged user.
exec sudo -E -u moonshine -- "$@"
