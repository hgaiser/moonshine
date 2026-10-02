# Working in Pyroshine

Pyroshine is a Linux game-streaming server derived from Moonshine, serving
Moonlight-compatible clients. This file is a repository map and engineering
contract; read the deeper documents relevant to the change, not every guide.

## Identity and compatibility

- Native packages and portable installations expose the `pyroshine` command and
  service; the Rust server artifact remains `moonshine`.
- Internal crate names, config/state paths, `MOONSHINE_*` environment variables,
  and protocol identifiers intentionally retain `moonshine`. Do not casually
  rename them: this preserves compatibility and eases upstream synchronization.
- Nix also retains the `moonshine` package/executable and `services.moonshine`
  module interface; follow `docs/NIXOS.md` for that integration.
- Preserve public/protocol behavior unless the task intentionally changes it.
  Avoid needless rewrites that complicate upstream comparison, while respecting
  intentional Pyroshine divergence. Preserve license notices and attribution.

## Repository map

Paths are relative to the repository root; core rows use `moonshine-core/src/`
unless explicitly marked as root.

| Area | Responsibility |
| --- | --- |
| Root `src/main.rs` | CLI, config loading, startup probes, server wiring, shutdown |
| `config.rs`, `healthcheck.rs` | Config loading/defaults and host/codec capability checks |
| `app_scanner/`, `clients.rs`, `state.rs`, `tls.rs`, `discovery.rs` | Application discovery, pairing/client state, certificates, mDNS |
| `webserver/`, `rtsp.rs`, `ingress.rs` | GameStream HTTP/HTTPS API and pairing; RTSP negotiation and stream orchestration; bounded connection supervision |
| `session/mod.rs`, `session/manager.rs`, `session/application.rs` | Session states, launch/resume/reconfiguration, application lifetime |
| `session/authorization.rs` | Launch/resume authorization generations binding RTSP, control and media discovery to the paired client |
| `session/compositor/` | Embedded headless Smithay compositor, scene/focus/cursor/input, GBM/DMA-BUF capture |
| `session/stream/audio/` | Embedded PulseAudio-compatible capture server, Opus encoding, audio packets/UDP |
| `session/stream/control/` | Control protocol, input decoding/routing, Inputtino devices and feedback |
| `session/stream/video/` | Negotiated formats, stream epochs, packetization/FEC, UDP GSO/pacing |
| `session/stream/video/pipeline/` | Capture consumption, DMA-BUF import, Pixelforge/Vulkan Video encoding and PyroWave integration |
| `session/stream/video/pyrowave.rs` | Dynamic PyroWave C API, ABI/provenance checks, native resource ownership |

Other workspace and integration areas:

- `moonshine-wsi/`: Vulkan implicit layer intercepting instance/device/surface/
  swapchain behavior and routing presentation to the compositor over Wayland.
  Its `protocols/` bindings must agree with compositor-side protocol handling.
- `moonshine-tools/`: developer tools, including the `moonshine-bench` pipeline benchmark.
- `scripts/`: pinned PyroWave build helper and changelog tooling/tests.
- `dist/`, `nfpm.yaml`, `.github/workflows/release.yaml`: native/portable packaging,
  installers, systemd, Vulkan manifests, device permissions and system policy.
- `nix/`, `flake.nix`: Nix package, dependency build, development shell and service module.
- `vendor/inputtino/`: maintained native Inputtino patch, including build/binding sources.

## Choose the source of truth

| Change | Consult |
| --- | --- |
| Cross-layer ownership, startup or session lifecycle | `docs/ARCHITECTURE.md` |
| Build, install locally, contribute, release | `CONTRIBUTING.md`; CI commands in `.github/workflows/ci.yaml` |
| Configuration fields, defaults or semantics | `docs/CONFIGURATION.md` and the owning Rust config/default implementations |
| PyroWave, codec negotiation, GPU ownership, FEC or transport | `docs/PYROWAVE.md` |
| Capture, cursor lifetime, focus, Steam surfaces/overlays or input | `docs/COMPOSITOR.md` |
| Vulkan bypass, extension gates, swapchain image counts | `docs/VULKAN_IMAGE_COUNTS.md` |
| Capture admission, completion or pipeline backpressure | `docs/PIPELINE_OPTIMIZATION.md` |
| Performance measurements | `docs/BENCHMARKING.md`; lifetime/soak diagnostics in `docs/LONG_SESSION_PERFORMANCE.md` |
| Reconnect or stream reconfiguration | `docs/reconnect-validation.md` |
| Native controller backend/Edge mapping | `docs/DUALSENSE_EDGE.md`, `vendor/inputtino/LOCAL_CHANGES.md` |
| Packaging, service or host integration | `docs/INSTALLATION.md`, `CONTRIBUTING.md`; `docs/NIXOS.md` for Nix |
| Release notes and inherited history | `docs/CHANGELOG.md`; `docs/UPSTREAM_CHANGELOG.md` is the upstream archive |

`docs/README.md` indexes current guides; link new guides there when applicable.
Dated results/rejected experiments belong in `docs/reports/`, not current design
instructions. Treat archived validation as evidence for its recorded revision only.

## Architectural boundaries

- Keep protocol/session orchestration, compositor scene decisions, encoding,
  packet transport, input/control, WSI interception and packaging in their owning
  layers. Extend existing interfaces rather than coupling unrelated layers or
  introducing parallel systems; inspect nearby patterns first.
- Capture, rendering, encoding, packetization, UDP/FEC/pacing, WSI presentation
  and controller/input routing are latency-sensitive. Avoid adding unnecessary
  allocations, copies, blocking I/O, locks, synchronous waits, per-frame logging
  or indirection in these loops. Preserve bounded queues/backpressure and buffer
  release semantics; existing GPU completion waits are ownership requirements,
  not invitations to remove synchronization without proof.
- For session/audio/video/HDR/resolution/codec/reconnect changes, cover initial
  connection, disconnect, reconnect, changed parameters and unchanged-mode resume.
  Check stream epochs/state transitions, cleanup and stale keys/frames/resources
  from the prior connection. Use the reconnect validation matrix.
- Capture visibility and input focus are distinct but interacting responsibilities.
  Review cursor, scene, Steam overlays and input routing together before fixing
  capture/focus/cursor/Steam-input symptoms; follow `docs/COMPOSITOR.md`.
- WSI changes must preserve Vulkan object lifetimes, dispatch/extension semantics
  and compositor protocol agreement. Assess capture and session consequences;
  changing the layer alone does not establish correct streaming behavior.
- At unsafe/native boundaries (Vulkan, DMA-BUF handles, Inputtino, PyroWave), keep
  new `unsafe` scopes narrow. Explain non-obvious ownership, lifetime,
  synchronization, ABI and cleanup assumptions, including failure paths.

## Dependencies and configuration

- Root `Cargo.toml` patches only `inputtino-sys` to the vendored native backend;
  the public Rust `inputtino` API stays on the pinned upstream Git dependency.
  Do not casually replace/reorganize this arrangement. Follow `LOCAL_CHANGES.md`
  when updating it, comparing the patch with upstream and coordinating Nix.
- For PyroWave pins, C API assumptions or build changes, follow the pinned
  dependency process in `docs/PYROWAVE.md`. Keep Rust provenance/API requirements,
  `scripts/build-pyrowave.sh`, `nix/pyrowave.nix` and documented pins synchronized.
  Keep dependency patches under `nix/patches/` consistent across build paths;
  do not silently substitute another PyroWave source or ABI.
- Configuration additions/removals/renames/default or behavior changes must update
  owning Rust structures/defaults, examples and `docs/CONFIGURATION.md` together.
  Update healthcheck/capability behavior where affected. Keep user-facing settings
  documented; code-only options must be intentionally internal/diagnostic.

## Validation

Run commands from the repository root. Use the narrowest relevant test while
iterating. For completed implementation work, run appropriate workspace checks
from CI (native prerequisites are in `CONTRIBUTING.md`):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --all-features
```

CI also runs `cargo machete` (install with `cargo install cargo-machete`),
`python3 scripts/changelog.py check`, and
`python3 -m unittest discover -s scripts -p 'test_changelog.py'`.
Its test leg builds PyroWave with `scripts/build-pyrowave.sh` and runs
`session::stream::video::pyrowave::tests::ffi_loads_pinned_api` in `moonshine-core`
with `MOONSHINE_TEST_PYROWAVE=1` and `MOONSHINE_PYROWAVE_LIBRARY` pointing to the
built library; use the full invocation in `CONTRIBUTING.md` or CI.

- Documentation-only work needs diff/Markdown/path checks and `git diff --check`,
  not a full native build. Select other checks according to the affected behavior.
- Fix reproducible bugs with regression coverage when reasonably automatable.
  Test externally meaningful behavior or stable internal contracts, not incidental
  implementation details. For live-only behavior, cover lower-level contracts
  where practical and record the remaining manual checks.
- GPU capture, PyroWave/Vulkan/HDR, compositor/Steam/controller behavior,
  reconnects and transport/performance need relevant documented GPU/client,
  benchmark or acceptance checks in addition to automated coverage. Report tested
  hardware/clients and checks not performed; a build/unit test is not hardware validation.

## Change discipline and documentation

- Keep work focused; avoid unrelated cleanup. Remove obsolete paths when a
  replacement is intentionally complete; add compatibility shims only for an
  actual compatibility requirement. Update relevant tests/docs with implementation.
- Record meaningful user-visible changes under `Unreleased` in `docs/CHANGELOG.md`
  using the contributor guide's categories. Do not add fork entries to the
  archived `docs/UPSTREAM_CHANGELOG.md`; follow `CONTRIBUTING.md` for release work.
- Comments should explain architectural intent, compatibility/protocol quirks,
  safety invariants and non-obvious performance decisions that could otherwise
  be incorrectly simplified. Avoid narrating code; document public/complicated
  interfaces where it materially helps humans and agents maintain them.
- Update this file when architectural boundaries, canonical commands, document
  locations or repository-wide invariants change; ordinary features need no edit.
