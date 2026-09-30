# Pyroshine documentation

Start with the [project README](../README.md) for requirements and a quick start.

## Using Pyroshine

- [Installation and upgrades](INSTALLATION.md): native packages, portable builds, headless setup, and service diagnostics.
- [Configuration reference](CONFIGURATION.md): every supported `config.toml` setting, defaults, and examples.
- [DualSense Edge](DUALSENSE_EDGE.md): native controller mapping, dependency patch, and Steam acceptance checks.
- [Tips and troubleshooting](TIPS.md): Steam, Flatpak, Gamescope, desktop sessions, and application logs.
- [NixOS](NIXOS.md): flake package, service module, and development shell.
- [Changelog](CHANGELOG.md): Pyroshine fork releases and upcoming changes.
- [Upstream changelog](UPSTREAM_CHANGELOG.md): archived Moonshine release history and attribution.

## Development and architecture

- [Contributor guide](../CONTRIBUTING.md): manual build, install/upgrade, validation, and release publishing.
- [PyroWave architecture](PYROWAVE.md): dependency pins, codec negotiation, transport, and GPU validation.
- [Vulkan image counts](VULKAN_IMAGE_COUNTS.md): bypass capability negotiation, extension gates, and PoE acceptance checks.
- [Compositor architecture](COMPOSITOR.md): scene capture, cursor lifetime, and Steam input.
- [Benchmarking](BENCHMARKING.md): encoding pipeline measurements with `moonshine-bench`.
- [Reconnect validation](reconnect-validation.md): manual stream reconfiguration checks.
