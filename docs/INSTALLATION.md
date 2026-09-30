# Installation and upgrades

See the [README](../README.md#requirements) for host requirements. Release assets
are available from the [Pyroshine releases page](https://github.com/karsyboy/pyroshine/releases).
The published portable archive and native packages target x86_64 Linux; release
binaries are built on Ubuntu 24.04 (glibc 2.39), so older systems may need a source build.

## Native packages

Download the package for your distribution. These commands install a new package
or upgrade an existing installation:

```sh
# Arch Linux / CachyOS
sudo pacman -U ./pyroshine-*.pkg.tar.zst

# Debian / Ubuntu
sudo apt install ./pyroshine_*.deb

# Fedora / RHEL
sudo dnf install ./pyroshine-*.rpm
```

Run only the command for your distribution. The package installs the binary,
PyroWave library, Vulkan WSI layer, systemd service, udev rules, kernel-module
configuration, and sleep-inhibition policy. Start it as your regular streaming user:

```sh
sudo systemctl enable --now "pyroshine@$USER"
pyroshine healthcheck
```

After an upgrade, restart the service to use the new binaries:

```sh
sudo systemctl restart "pyroshine@$USER"
```

## Portable installer and SteamOS

Run the release installer as your normal user; it requests sudo for system changes:

```sh
curl -fsSL https://github.com/karsyboy/pyroshine/releases/latest/download/pyroshine-install.sh -o pyroshine-install.sh
bash pyroshine-install.sh
```

The installer deploys to `/opt/pyroshine`, configures systemd, udev, kernel
modules, the Vulkan layer, and polkit, and offers to enable lingering and start
the service. It also installs SteamOS atomic-update integration when available.
Runtime libraries such as Opus, libevdev, libxkbcommon, GBM, Wayland, and
Xwayland must be supplied by the host; the portable archive does not include them.

For an unattended install:

```sh
bash pyroshine-install.sh --user "$USER" --enable --linger --start --healthcheck
```

Run a freshly downloaded release installer again to upgrade. The copy installed
under `/opt/pyroshine/bin/` is pinned to its release version, so rerunning that
copy reinstalls that version. Configuration and pairing data are kept in your
home directory.

Open a new shell to pick up the installed PATH, or run diagnostics directly:

```sh
/opt/pyroshine/start-pyroshine.sh healthcheck
```

Use the [contributor guide](../CONTRIBUTING.md#manual-installation-and-upgrade)
for installation from local build artifacts without the downloader.

## Headless setup

To keep the user's systemd instance running while logged out:

```sh
sudo loginctl enable-linger "$USER"
```

For gamepad access without an active desktop session, add the streaming user to
`input`, then log out and back in:

```sh
sudo usermod -aG input "$USER"
```

The packaged service grants the `pyroshine` supplementary group for sleep
inhibition. When running the binary directly, join that group yourself if you
want the same policy; see [sleep inhibition](TIPS.md#prevent-the-host-from-suspending-while-streaming).

## Configuration and pairing

The default CLI config is `$XDG_CONFIG_HOME/moonshine/config.toml`, falling back
to `~/.config/moonshine/config.toml`. The shipped systemd unit explicitly uses
`/home/<user>/.config/moonshine/config.toml`; for a different home or config path,
create a service override as described in the [configuration reference](CONFIGURATION.md).
Pairing state remains under `~/.local/share/moonshine` by default. Back up these
paths before upgrading or migrating from Moonshine.

See the [configuration reference](CONFIGURATION.md) for application entries and
server settings. Add the host in Moonlight and submit its pairing PIN at
`http://localhost:47989/pin` on the host (or the configured HTTP port).
Keep the host on a trusted LAN or VPN and restrict its listening ports.

## Service diagnostics

```sh
systemctl status "pyroshine@$USER"
journalctl -u "pyroshine@$USER" -e
```

For application output, see [debugging a failing application](TIPS.md#debug-a-failing-application).
For NixOS use the [NixOS module guide](NIXOS.md), which manages service setup declaratively.
