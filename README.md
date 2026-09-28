[![CI](https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml/badge.svg)](https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml)

# Pyroshine

Pyroshine is a maintained Linux game-streaming server based on
[Moonshine](https://github.com/hgaiser/moonshine). It preserves Moonshine's
isolated, headless streaming architecture and conventional H.264, HEVC, and AV1
support while adding native PyroWave streaming and a managed KDE Plasma 6
desktop session.

Pyroshine speaks the Moonlight/GameStream protocol. Standard Moonlight clients
can use its conventional codecs; PyroWave requires
[Moonlight Qt PyroWave](https://github.com/karsyboy/moonlight-qt-pyrowave) or
another client implementing the same versioned extension.

## Why this fork exists

Moonshine provides a compact Rust host with a per-stream compositor. Pyroshine
maintains the additional codec negotiation, zero-copy PyroWave encode path,
transport handling, packaging, diagnostics, and desktop-session integration
needed for this project's use cases. It is an independent community fork, not
an official Moonshine or Moonlight release.

The installed executable, systemd service, configuration directory, and Vulkan
layer retain their `moonshine` names for compatibility with upstream tooling.

## Features

- Native PyroWave 4:2:0 and 4:4:4 streaming in SDR and HDR10.
- Hardware Vulkan encode with DMA-BUF import and no CPU pixel conversion.
- H.264, HEVC, and AV1 through Moonshine's existing Vulkan Video pipeline.
- Isolated headless sessions that do not take over the host desktop.
- Managed nested KDE Plasma 6 desktop sessions sized to the client.
- Low-latency stale-frame handling, multi-block FEC, encryption, and UDP GSO.
- Mouse, keyboard, touch, pen, controller, motion, haptics, and surround audio.
- Focused health checks and `moonshine-bench` latency/throughput reporting.

## Requirements

- Linux with systemd and a working Wayland/Vulkan stack.
- A GPU and driver supported by Moonshine/Pixelforge for conventional hardware
  encoding.
- For PyroWave: a hardware Vulkan GPU with the required compute, timeline
  semaphore, external-memory, and DMA-BUF interoperability features. Software
  Vulkan devices are intentionally rejected.
- For the Desktop entry: KDE Plasma 6, KWin, Xwayland, D-Bus, PipeWire, and the
  KDE desktop portal. See [docs/PLASMA.md](docs/PLASMA.md).
- A Moonlight-compatible client. Use Moonlight Qt PyroWave for the PyroWave
  codec; upstream Moonlight clients remain usable with conventional codecs.

Run `moonshine healthcheck` after installation to see the exact capabilities
available on the selected GPU.

## Installation

Tagged builds are published on the
[Pyroshine releases page](https://github.com/karsyboy/pyroshine/releases) as
Debian and RPM packages plus a portable x86_64 archive. Package and service
filenames currently use the compatible `moonshine` runtime name.

### Debian or Ubuntu

```sh
sudo apt install ./moonshine_*.deb
sudo systemctl enable --now moonshine@$USER
```

### Fedora or RHEL

```sh
sudo dnf install ./moonshine-*.rpm
sudo systemctl enable --now moonshine@$USER
```

### SteamOS and other supported x86_64 systems

```sh
curl -fsSL https://github.com/karsyboy/pyroshine/releases/latest/download/moonshine-install.sh | bash
```

The installer places the portable build under `/opt/moonshine` and installs the
required systemd, udev, modules-load, Vulkan-layer, and polkit files.

For unattended headless use, enable lingering first:

```sh
sudo loginctl enable-linger "$USER"
```

For headless gamepad access, add the streaming user to `input`, then log out and
back in:

```sh
sudo usermod -aG input "$USER"
```

### NixOS

The flake exports a package and `services.moonshine` module. See
[nix/README.md](nix/README.md) for a complete configuration.

### Build from source

Install Rust plus the C/C++ and Linux development dependencies used by
Moonshine, then build the pinned PyroWave library and the Rust workspace:

```sh
./scripts/build-pyrowave.sh /tmp/pyrowave-src /tmp/pyrowave-install
export LD_LIBRARY_PATH="/tmp/pyrowave-install/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cargo build --release --workspace
```

The release and Nix builds pin
[`karsyboy/pyrowave`](https://github.com/karsyboy/pyrowave) at the revision
documented in [docs/PYROWAVE.md](docs/PYROWAVE.md). Pyroshine checks the exact C
API version at runtime and leaves conventional codecs available when the
optional library cannot be loaded.

The upstream build still produces `target/release/moonshine` and
`target/release/libmoonshine_wsi.so`. Follow Moonshine's system integration or
the packaged files under `dist/` when installing a source build system-wide.

## Configuration and use

Pyroshine reads the same TOML format and state locations as Moonshine. Packaged
services use `~/.config/moonshine/config.toml`; a default file is created when
the requested path does not exist.

Add a normal application with:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
```

Add the managed desktop with:

```toml
[[application]]
title = "Desktop"
type = "desktop"

[application.desktop]
environment = "plasma"
scale = 1.0
```

Start the service, add the host in Moonlight, and enter the displayed pairing
PIN at `http://localhost:47989/pin`. PyroWave clients negotiate the codec only
when both sides advertise wire version 1; conventional clients are unaffected.

Do not expose Pyroshine directly to the public internet. Use it on a trusted LAN
or through a VPN and restrict the GameStream ports with a firewall.

## Documentation

- [PyroWave architecture, negotiation, dependency pin, and validation](docs/PYROWAVE.md)
- [Managed Plasma 6 desktop architecture and setup](docs/PLASMA.md)
- [NixOS package and module](nix/README.md)
- [Tips and troubleshooting](TIPS.md)

Advanced options such as `MOONSHINE_PYROWAVE_LIBRARY`, packet-size caps,
benchmarking, and manual GPU validation live in the detailed documents rather
than the quick-start path.

## Upstream

Pyroshine is derived from [Moonshine](https://github.com/hgaiser/moonshine) and
retains its architecture, protocol implementation, runtime naming, and much of
its documentation. Credit for that work belongs to Moonshine's authors and
contributors. Pyroshine-specific changes are maintained in this repository;
they should not be presented as upstream Moonshine features.

Moonshine in turn builds on the Moonlight ecosystem and work pioneered by
[Sunshine](https://github.com/LizardByte/Sunshine).

## License and acknowledgements

Pyroshine remains licensed under the [BSD 2-Clause License](LICENSE). The
original Moonshine copyright and license notices are preserved. PyroWave and
other dependencies retain their own licenses and notices.
