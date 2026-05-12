#!/bin/bash
# Pre-systemd initialization.
# This runs as root before exec'ing into systemd (PID 1).
# Dynamic setup that depends on the runtime environment (host UID,
# GPU devices) must happen here because it can't be baked into the image.

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

# Create XDG_RUNTIME_DIR (systemd-logind does this on a real system,
# but in a minimal container it may not be fully functional).
mkdir -p "/run/user/$USER_ID"
chown "$USER_ID:$GROUP_ID" "/run/user/$USER_ID"
chmod 700 "/run/user/$USER_ID"

# Create XWayland socket directory.
mkdir -p /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix

# Dynamically add the moonshine user to groups that own GPU and input devices.
# Device files passed via --device retain their host GIDs, which won't match
# the container's named groups.
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
# nvidia-utils registers an EGL vendor that fails fatally when no NVIDIA GPU
# is present, preventing Mesa (AMD/Intel) from initializing.
if [ ! -e /dev/nvidia0 ] && [ ! -d /proc/driver/nvidia ]; then
    echo "No NVIDIA GPU detected, disabling NVIDIA EGL/Vulkan ICDs."
    rm -f /usr/share/glvnd/egl_vendor.d/10_nvidia.json
    rm -f /usr/share/vulkan/icd.d/nvidia_icd.json
    rm -f /usr/share/vulkan/implicit_layer.d/nvidia_layers.json
fi

# Hand off to systemd as PID 1.
exec /usr/lib/systemd/systemd
