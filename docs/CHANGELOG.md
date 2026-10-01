# Pyroshine changelog

Notable changes in the Pyroshine fork, newest first. Release dates use YYYY-MM-DD.
Earlier Moonshine releases and their original contributor attribution are kept
in the [upstream changelog](UPSTREAM_CHANGELOG.md).

Fork entries below were reconstructed from local tags and their commits. Early
tags did not always match the embedded Cargo version or follow commit order;
those discrepancies are recorded rather than rewriting release history.

Add upcoming changes under **Unreleased**. The release workflow requires a dated,
nonempty entry matching both the tag and workspace version and publishes that
entry as the GitHub release notes. See [release preparation](../CONTRIBUTING.md#publishing-a-release).

## [Unreleased]

## [v0.16.13] - 2026-10-01

### Fixed

- Include compositor work in PyroWave's frame pacing budget and use reusable Linux high-resolution packet timers, restoring the measured saturated composited 4K120 cadence without adding capture credits or frame queues.

### Added

- Add `[stream.video] log_stats` to enable or disable streaming diagnostic summaries and process sampling while preserving benchmark statistics and operational warnings.
- Implemented one-credit capture admission, direct-export rejection counters, GPU timing, import-cache telemetry, and async-compute selection with graphics fallback.

## [v0.16.12] - 2026-09-30

### Fixed

- Release completed direct-scanout buffers and flush Wayland release events before static-screen skipping, preventing swapchain image starvation under encoder load.

### Added

- Report scanout buffer releases and allocated swapchain image counts to diagnose capture stalls and image-count negotiation.

## [v0.16.11] - 2026-09-30

### Fixed

- Clear Tokio writable readiness after raw GSO sends encounter socket backpressure, preventing a retry loop from starving video transmission.
- Expire unused PyroWave DMA-BUF imports and release partial Vulkan import resources on setup errors.

### Added

- Five-second streaming diagnostics for capture resources, pipeline stages, transport backpressure, runtime delay, CPU usage, memory, and open file descriptors.

## [v0.16.10] - 2026-09-30

### Fixed

- Fix the 0.16.9 Vulkan query dispatch regression that crashed Steam's GPU process at Big Picture startup and forced software compositing, degrading streaming performance across encoders.
- Discover instance extensions through the Vulkan loader's global entry point so WSI initialization works when Mesa's device-selection layer is next in the chain.

## [v0.16.9] - 2026-09-30

### Fixed

- Negotiate driver-supported three-image swapchains on the XWayland Vulkan bypass using per-present-mode capabilities and swapchain maintenance, addressing conservative Wayland image-count limits.
- Gate WSI maintenance injection on real extension/feature support and restrict dynamic presentation to declared compatible modes.
- Refactor HDR and YUV 4:4:4 handleing

### Changed

- Deprecate and ignore `MOONSHINE_WSI_MIN_IMAGE_COUNT`; preserve ordinary driver image-count capabilities and report actual allocated swapchain counts.

## [v0.16.8] - 2026-09-30

### Added

- Preserve DualSense Edge identity and its four native extra buttons from compatible Moonlight clients through the virtual PlayStation controller.

### Changed

- Simplify the README and consolidate supporting guides under `docs/`.
- Document every `config.toml` setting and manual build, installation, and upgrade steps.
- Separate fork and upstream release history and require matching changelog notes before publishing releases.

## [v0.16.7] - 2026-09-30

### Fixed

- Capture the complete visible compositor scene, including cursors, overlays, and notifications, while keeping fullscreen direct export when safe.
- Preserve cursor visibility and correctly handle cursor replacement and destruction.
- Improve Steam overlay input routing and virtual controller family selection.

## [v0.16.6] - 2026-09-29

### Fixed

- Reconfigure video and audio on reconnect when codec, resolution, frame rate, HDR, or audio format changes.
- Resume unchanged streams through a fast path while restarting client-visible frame and packet epochs.

## [v0.16.5] - 2026-09-29

### Changed

- Pace UDP GSO transmission by frame size and bitrate and report pacing and send metrics.
- Preserve packetization across GSO chunk boundaries.

This tag points to a commit with embedded workspace version `0.16.4`.

## [v0.16.4] - 2026-09-29

### Added

- Configurable fixed, automatic, or disabled FEC with client feedback and bounds.
- PyroWave benchmark options and wire-byte/packet metrics.

### Fixed

- Respect minimum parity requirements and protocol limits when laying out FEC blocks.
- Rate-limit repeated FEC layout warnings and report PyroWave frames approaching size limits.

## [v0.16.3] - 2026-09-28

### Added

- Per-application `output_scale` for fractional Wayland output scaling.
- Public Pyroshine command, service, installer, and release package names.
- Release publishing from version tags, including portable and native packages.

### Changed

- Launch Plasma through a regular application entry and remove managed desktop-session handling.
- Improve PyroWave buffer handling and packetization efficiency and require hardware Vulkan devices.

### Fixed

- Improve compositor source-size selection and output geometry while preserving the client's physical stream resolution.

This release includes the branding, packaging, and compositor changes after the
commit referenced by v0.16.2, including the earlier legacy-tagged snapshots below.

## [v0.16.2] - 2026-09-28

### Added

- Native PyroWave encoding through the pinned C API, with separate codec/chroma/HDR negotiation and DMA-BUF interoperability checks.

This tag points to the initial PyroWave integration commit `fb5a89a`, earlier
than the v0.16.1.1 and v0.16.1.2 snapshots. Its embedded workspace version is
`0.16.1`; it does not contain all changes made by the later version-bump commit.

## [v0.16.1.2] - 2026-09-28

### Changed

- Remove redundant default initializers from application scanners.

Legacy four-component tag; embedded workspace version remains `0.16.1`.

## [v0.16.1.1] - 2026-09-28

### Added

- Initial fork integration of PyroWave, wire version 1 negotiation, hardware Vulkan checks, and encoding efficiency improvements.
- Pyroshine branding, install assets, and automatic release packaging.

### Changed

- Refactor desktop launching into normal application entries.

Legacy four-component tag; embedded workspace version remains `0.16.1`.
