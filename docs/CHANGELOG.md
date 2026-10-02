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

## [v0.17.0-beta-5] - 2026-10-01

### Security

- Generate private TLS identities with owner-only files, durable publication and interruption recovery; keep existing administrator-managed identities stable.
- Commit pairing ID/certificate trust atomically and implement durable host-authorized and HTTPS self-revocation, including active-session teardown. Preserve legacy trust and UUIDs without guessing certificate ownership.

### Fixed

- Bound pending pairing transactions, expire abandoned approvals and handshakes, clean up completed requests, and coalesce desktop notifications.

## [v0.17.0-beta-4] - 2026-10-01

### Fixed

- Audio now pauses on disconnect and every reconnect, including unchanged settings. Endpoint discovery cannot bypass the epoch barrier; new delivery commits keys, PCM generation, Opus, RTP/FEC and negotiated settings together while retaining application Pulse connections.
- Authorized control-peer disconnect/replacement now releases held keys, modifiers, mouse buttons, touch/pen contacts and queued text, neutralizes controllers, cancels Home/Guide timers and retires old feedback callbacks. Late previous-peer disconnects cannot reset the new peer's input.
- High-quality surround audio at 10 ms now respects Moonlight’s 1400-byte receive limit, including encryption and FEC overhead.

## [v0.17.0-beta-3] - 2026-10-01

### Fixed

- Video and audio no longer intermittently fail to start when the client's start signal arrives before both pipelines are waiting; a repeated start signal is harmless.
- Stopping a session before streaming began now releases its UDP ports, so the next session no longer fails with address-in-use errors.
- Cancelling a session, an application exit or a stream failure now finishes the whole teardown (application unit, stream workers, sockets and the PulseAudio socket) before the session is reported idle; a new launch waits for it instead of racing the old resources.
- A launch that times out or is cancelled while systemd is still starting the application no longer leaves the application unit running. Stopping a session also cancels a launch, stream start or reconnect that is still in progress.
- A duplicate or early RTSP PLAY, or a PLAY racing a reconnect ANNOUNCE, no longer stops the retained application or leaves a resumed stream paused. A reconnect whose reconfiguration fails stops the session cleanly instead of continuing with partially changed capture/encoder settings.
- Service shutdown (SIGTERM/SIGINT) now waits, within the session teardown deadline, for the application unit to stop.
- Captured frames now own their DMA-BUF descriptors, so frames still queued or being encoded during shutdown, a resolution change or a client buffer release can no longer import a closed or reused descriptor.
- A video encoder or GPU device that has stopped working now ends the session instead of logging a failure for every frame indefinitely. Recoverable frame failures drop the frame (requesting an IDR when the reference chain may be affected) with rate-limited warnings, and release the captured buffer only after its GPU reads are known to be complete.

### Changed

- Session teardown has an end-to-end deadline of 16 seconds (6 for stopping the application). If it is exceeded, new sessions are refused and Pyroshine shuts down so its service manager can restart it.

## [v0.17.0-beta-2] - 2026-10-01

### Security

- `/launch` and `/resume` require an exact 16-byte `rikey` (32 hex digits) and a 32-bit `rikeyid` (signed or unsigned decimal). Malformed keys are rejected before any session state, key or authorization changes; an encrypted video epoch can no longer fall back to plaintext or keep using a stale key after a same-ID key change.
- AES-GCM nonces for video and host control messages are owned by the key rather than by the packetizer or control task. Recreating a packetizer, reconfiguring, reconnecting, changing only the key ID, or reusing a key in a later session continues the key's counters, so no (key, nonce) pair is reused; counter exhaustion stops sending instead of wrapping. The IV wire format is unchanged.

### Fixed

- Launch, resume and RTSP ANNOUNCE validate resolution, refresh rate, packet size (including encryption and UDP limits), bitrate, audio channel count and audio packet duration before pausing or reconfiguring a stream, and reply with the rejected value instead of failing later or panicking. High-end requests (for example 7680×4320, 240 Hz, and 650–900 Mbps) remain accepted.
- Unsupported audio packet durations and channel counts are rejected instead of silently streaming 5 ms stereo audio.
- An application connecting to the session's PulseAudio socket with zero/out-of-range channels or sample rate, a mismatched channel map, or extreme buffer attributes receives a PulseAudio error instead of crashing the audio server; buffer attributes are always returned in a consistent order.
- Audio encryption no longer overflows for key IDs near the 32-bit limit.

### Changed

- With video encryption, `max_packet_size` now also covers the 32-byte encryption prefix, so the datagram stays at `max_packet_size + 16` bytes as documented.
- Startup rejects configurations whose bind `address` is not an IP address or whose TCP (`webserver.port`, `webserver.port_https`, `stream.port`) or UDP (`stream.video.port`, `stream.audio.port`, `stream.control.port`) listeners share a port.

## [v0.17.0-beta-1] - 2026-10-01

### Security

- Pairing approval (`/pin`, `/submit-pin`) is accepted only from the host itself: a loopback peer, a loopback `Host`, and same-origin browser requests. The PIN applies only to the pending request shown on the page, which now lists the requester address and certificate fingerprint; unapproved requests expire after five minutes. A non-loopback `address` gets an additional loopback-only approval listener.
- RTSP negotiation, media endpoint discovery and the control connection are bound to the paired client and the latest `/launch` or `/resume`. Clients supporting Moonlight's session-ID extension also receive per-launch/resume `X-SS-Ping-Payload` and `X-SS-Connect-Data` values that their media PINGs and control connection must echo; other clients are bound by address.
- Control messages are accepted only from the peer that authenticated with the current session key. Plaintext, wrong-key, replayed and stale-generation messages are dropped and cannot inject input, keep a stream alive, or receive feedback.

### Fixed

- Malformed, truncated or nested control messages are rejected without panicking or ending the session, and non-finite gamepad motion/touch values are ignored.
- A client that stalls during the TLS handshake no longer blocks other HTTPS clients. HTTP, HTTPS and RTSP connections are bounded in number, size and duration, and are released promptly on shutdown.

## [v0.16.15] - 2026-10-01

### Fixed

- Keep capture and application frame callbacks on the same refresh clock through pacing stalls and reconnects, instead of retaining a shifted capture phase after missed slots.
- Pause video delivery on client disconnect and require an acknowledged encoder/transport epoch on every reconnect, preventing old paced PyroWave frames from entering a resumed or changed-codec stream.

## [v0.16.14] - 2026-10-01

### Added

- Explicit Nonary/Vibepollo record transport negotiation using the verified PyroWave block-format family, while retaining the single native wire-v1 frame/packetizer/FEC path and pinned C API 0.7.0.
- Paired HTTPS clients can discover and run a bounded 32 MiB bandwidth probe when no session is active; advertise routed physical Ethernet capacity when known.
- Configurable disabled, legacy Back hold, and recommended Back+Start Guide shortcuts; existing nonzero `hold_ms` configurations retain their legacy behavior.

### Fixed

- Accept Nonary's PyroWave record ANNOUNCE capabilities instead of requiring its client to claim native wire-v1. Reject unknown revisions and contradictory protocol/profile attributes with a compatibility reason.
- Preserve real Select holds with Back+Start shortcuts, extended controller flags, and held inputs during activation-rumble timer completion.
- Reject unsafe Wine/Proton XWayland bypass before top-level acceptance, cache safety behind X11 events, repair XCB child-query layout, and retire stale compositor overrides when falling back to XCB.
- Restore graphics as the automatic PyroWave queue preference; compute remains an explicit option.

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
