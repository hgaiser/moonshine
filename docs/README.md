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

Start with [Architecture overview](ARCHITECTURE.md) for component ownership,
startup, session negotiation and resource lifetimes. Then choose the guide for
the area being changed:

| Area | Guide |
| --- | --- |
| Build, install, CI and releases | [Contributor guide](../CONTRIBUTING.md) |
| Scene capture, cursor, focus and Steam input | [Compositor](COMPOSITOR.md) |
| Capture demand, GPU completion and bounded encoding | [Capture pipeline](PIPELINE_OPTIMIZATION.md) |
| PyroWave dependency, negotiation, color, FEC and transport | [PyroWave](PYROWAVE.md) |
| Cross-fork framing and authenticated calibration | [PyroWave compatibility](PYROWAVE_COMPATIBILITY.md) |
| Vulkan bypass, extension gates and swapchain counts | [Vulkan WSI](VULKAN_IMAGE_COUNTS.md) |
| Native controller identity/report mapping | [DualSense Edge](DUALSENSE_EDGE.md) |
| Repeatable pipeline measurements | [Benchmarking](BENCHMARKING.md) |
| Runtime stalls, resource bounds and long-run checks | [Streaming diagnostics](LONG_SESSION_PERFORMANCE.md) |
| Mode changes, epochs and teardown | [Reconnect validation](reconnect-validation.md) |

[Historical reports](reports/README.md) preserve dated investigations and measured
results. They may describe older designs or local artifacts; use current guides
and code for engineering contracts, and repeat relevant hardware acceptance for
new changes. Keep new evidence separate from current instructions.
