# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
