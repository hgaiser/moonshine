[![CI](https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml/badge.svg)](https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml)

# Pyroshine

Pyroshine is a Linux game-streaming server based on
[Moonshine](https://github.com/hgaiser/moonshine). It runs applications in isolated,
headless sessions and streams them to Moonlight-compatible clients.

- Native PyroWave 4:2:0 and 4:4:4 streaming in SDR and HDR10.
- H.264, HEVC, and AV1 through Vulkan Video.
- Hardware encoding with DMA-BUF import, low-latency transport, and forward error correction.
- Keyboard, mouse, touch, pen, controller, haptics, and surround audio support.

Use [Moonlight Qt PyroWave](https://github.com/karsyboy/moonlight-qt-pyrowave)
for PyroWave streaming. Standard Moonlight clients can use the conventional codecs.

## Requirements

A Linux host with systemd, a working Wayland/Vulkan stack, and a supported GPU
and driver. PyroWave requires hardware Vulkan with compute, timeline semaphore,
external-memory, and DMA-BUF support; software Vulkan devices are rejected.
Run `pyroshine healthcheck` after installation to check your host.

## Quick start

Download a package from the [releases page](https://github.com/karsyboy/pyroshine/releases)
and install it with your distribution's package manager:

| Distribution | Install or upgrade |
| --- | --- |
| Arch Linux / CachyOS | `sudo pacman -U ./pyroshine-*.pkg.tar.zst` |
| Debian / Ubuntu | `sudo apt install ./pyroshine_*.deb` |
| Fedora / RHEL | `sudo dnf install ./pyroshine-*.rpm` |

Then start Pyroshine for your user:

```sh
sudo systemctl enable --now "pyroshine@$USER"
```

For SteamOS or a portable installation, see the [installation guide](docs/INSTALLATION.md).
NixOS users should use the [NixOS module](docs/NIXOS.md).

Pyroshine creates `~/.config/moonshine/config.toml` on first start, with Steam
and a Steam library scanner enabled by default. To configure another application,
add an entry to that file:

```toml
[[application]]
title = "My game"
command = ["/absolute/path/to/game"]
```

Restart the service after editing, add the host in Moonlight, and enter the
client's pairing PIN at `http://localhost:47989/pin` on the host:

```sh
sudo systemctl restart "pyroshine@$USER"
```

Use a trusted LAN or VPN and restrict the GameStream ports with a firewall.
Do not expose Pyroshine directly to the public internet.

## Documentation

- [Installation, upgrades, and headless setup](docs/INSTALLATION.md)
- [Complete config.toml reference](docs/CONFIGURATION.md)
- [Tips and troubleshooting](docs/TIPS.md)
- [All documentation](docs/README.md)
- [Contributing, manual builds and installation, and releases](CONTRIBUTING.md)
- [Changelog](docs/CHANGELOG.md)

## AI-assisted development

Pyroshine is developed with substantial assistance from AI coding tools,
including Codex, for implementation, refactoring, testing, documentation,
debugging, and code review. The maintainer intentionally guides architectural
direction and project decisions.

AI-generated or AI-modified code is not assumed correct because it came from AI.
All changes are expected to meet the same review, testing, validation,
maintainability, security, and compatibility standards, whether written by a
human or with AI assistance.

## License and credits

Pyroshine is an independent community fork of
[Moonshine](https://github.com/hgaiser/moonshine), which builds on the Moonlight
ecosystem and work pioneered by [Sunshine](https://github.com/LizardByte/Sunshine).
It retains Moonshine's internal crate names, configuration paths, and protocol
names to ease upstream synchronization.

Licensed under the [BSD 2-Clause License](LICENSE). Original copyright notices
are preserved; dependencies retain their own licenses.
