# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v0.6.0] - 2025-11-18

### Added

- Steam library scanner now recursively searches Steam `librarycache` directories for box art, improving compatibility with different Steam layouts.
- RTSP server now advertises server capabilities and supported encryption flags in the SDP, aligning better with Moonlight’s expectations.
- Structured session shutdown reasons (`SessionShutdownReason`) added and wired through video, audio, control, and input components for clearer diagnostics.
- Audio pipeline coordinated shutdown via `ShutdownManager`, and a dedicated audio packet handler task.
- Control feedback commands (rumble, RGB LED, motion enable, trigger effects) and a feedback channel added to send encrypted feedback to clients.
- Full support for gamepad motion (gyro/accel), touchpad input (PS5), battery status, and haptics using [`inputtino`](https://github.com/games-on-whales/inputtino) (thanks @ABeltramo!).
- New `VideoFrameCapturer` and `VideoEncoder` components added to separate CUDA frame capture and encoding, coordinated via shared buffers and condition variables.

### Changed

- Input handling for keyboard, mouse, and gamepads migrated from `evdev` to `inputtino` virtual devices, simplifying mapping and improving cross-device behavior.
- Session lifecycle refactored: `SessionManager` and `Session` now use explicit start/stop flows, `oneshot` channels for stop completion, and centralized shutdown management instead of ad-hoc flags.
- Audio and video streams now use dedicated UDP packet handler tasks for send/receive (with QoS, PING discovery, and graceful shutdown) rather than ad-hoc loops inside stream logic.
- Control stream rebuilt on `rusty_enet` with AES‑128‑GCM encrypted control messages (sequence-number–based IVs and explicit tags) and extended handling of Moonlight control message types (HDR mode (HDR not implemented yet), haptics, motion/LED/trigger control).
- Video pipeline restructured to build CUDA device/frame contexts explicitly, validate capture resolution vs requested resolution, and coordinate frame flow via atomics and condition variables.
- Logging levels and messages tuned across components (e.g. service registration, shutdown logs, command logs) to reduce noise and make lifecycle events clearer.

### Fixed

- Gamepad ID / kind now follow the client-provided controller type, improving correct button layouts and feature support for Xbox/PS5/Switch controllers.
- More robust handling of closed channels and unexpected terminations in audio, video, control, and input threads, preventing silent failures and dangling sessions.
- Session stop requests now wait (with timeout) for underlying streams to fully terminate, reducing the chance of partially torn-down sessions.
- Escape XML characters in Steam game titles to prevent XML parsing issues in Moonlight client.

## [v0.5.0] - 2024-12-19

### Removed

- Removed verbosity commandline flags in favor of `RUST_LOG` environment flag.

### Changed

- Improved frame pacing by using the latest captured frame, instead of waiting for a new frame.

### Fixed

- Fix sending the remaining packets when there are more than 4 video blocks.

## [v0.4.1] - 2024-12-09

### Changed

- CI fix.

## [v0.4.0] - 2024-12-09

### Changed

- Update dependencies.
- Send audio as f32.
- Set audio bitrate to 512k.

## [v0.3.1] - 2024-05-20

### Added

- Add support for Turing and older NVIDIA cards.

### Changed

- Released tar file includes version number.
- Optimizations to audio buffer management.
- Run audio encode & capture in a dedicated thread.
- Use `tracing` instead of `env_logger`.

## [v0.3.0] - 2024-04-24

### Added

- Add notification when PIN is expected.
- Add interface for submitting a PIN from Moonlight.

## [v0.2.3] - 2024-04-21

### Added

- Add workflow for releasing binary file.

### Changed

- Update dependencies.

## [v0.2.2] - 2024-03-05

### Added

- Allow to set certificate path with expansion (environment variables and `~`).

### Changed

- Create certificate directory if it does not exist.

## [v0.2.1] - 2024-03-05

### Added

- Generate a config.toml file if it did not exist yet.

### Removed

- config.toml file in the repository.

## [v0.2.0] - 2024-03-05

### Added

- Certificate creation through code. This creates a certificate if none exists yet.
- Github workflow.
- VSCode launch.json file.

### Removed

- Unused dependencies.
- Removed many unwrap calls.
- Removed xml crate (replaced by simple String formatting).
- Removed `make-cert` script (as this is now handled in code).

### Changed

- Replaced custom ffmpeg binding with [ffmpeg-next](https://github.com/zmwangx/rust-ffmpeg).
- Replaced CUDA from ffmpeg binding with [cudarc](https://github.com/coreylowman/cudarc).
- Replaced [cpal](https://github.com/RustAudio/cpal/) with [libpulse_binding](https://github.com/jnqnfe/pulse-binding-rust). Because of this change, Moonshine will automatically pick up the right pulseaudio monitor for capturing desktop audio.
- Replaced custom Reed Solomon encoding with [reed-solomon-erasure](https://github.com/rust-rse/reed-solomon-erasure).


## [v0.1.0] - 2024-01-25

### Added

- Initial release.
