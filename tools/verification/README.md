# Portable Moonshine host verification

This directory contains a **host-side** smoke harness around the existing `moonshine-bench` binary. It is not a Moonlight client simulator: it exercises the compositor/capture, color conversion, GPU video encode, readback, packetization and local UDP send path, but does **not** verify real-client decode, HDR output, network delivery, or display.

## Build and check readiness

From the repository root, with the usual system dependencies or `nix develop`:

```sh
cargo build --release -p moonshine-tools
python3 tools/verification/verify_video.py doctor
```

The Nix development shell provides Python, `vkcube`, Xwayland and the EGL/Vulkan
loader paths required by a Cargo-built benchmark. Keep the smoke command inside
that shell. Outside Nix, provide those runtime dependencies separately.

The doctor checks that the benchmark binary and rendering application (`vkcube` by default) are executable, that at least one DRM render node is accessible, and that UDP port 47998 is not already bound on loopback. A free port is only a best-effort check; another process could claim it later. Do not start a smoke run while another Moonshine video stream is using the port.

The host must support Vulkan Video encoding for the requested codec. The doctor cannot reliably prove that from device-node access alone. A failing codec initialization is a real smoke-test failure, not a reason to bypass the check.

## Smoke tests

```sh
python3 tools/verification/verify_video.py smoke
python3 tools/verification/verify_video.py smoke --resolution 3840x2160 --fps 60 --codec hevc --bitrate 150000000 --hdr
```

Defaults: H.264, 1080p60, 20 Mbps, 12 seconds (3 seconds warmup), `vkcube`. Override the source with `--app /path/to/renderer`, the binary with `--bench /path/to/moonshine-bench`, and artifacts with `--output /path/to/artifacts`.

A successful process exit is **not** enough: the harness requires a completed benchmark session summary with at least one encoded frame. It also refuses missing tools/device access or an already-bound video port. `--min-fps` is optional and should only be chosen after establishing a stable baseline; different devices do not share a useful universal FPS threshold.

Each run writes `moonshine-bench.log` and `report.json` under `target/verification/smoke-<UTC timestamp>/` by default. The report records the requested workload, exit code, observed frames/FPS, optional GPU timestamp sample count, Git commit, and reason for failure. Keep the artifacts when reporting a failure.

**Caveat:** `--hdr` requests an HDR *session*. `vkcube` can still render SDR pixels, so this does not establish that the client receives PQ/HDR10 content. Real Moonlight HDR validation requires a source that renders HDR and a client.

The script owns the benchmark process it launches. It tries SIGINT on timeout and escalates to SIGTERM/SIGKILL for that process group. External processes launched through a different supervisor are not guaranteed to be in that group; stop any leftover instance using its own supervisor. Never kill arbitrary processes by name.

## Test the harness without GPU

```sh
python3 -m unittest discover -s tools/verification -p 'test_*.py'
```

These tests validate that a zero-frame run fails even with exit code 0, a positive-frame run succeeds, and optional GPU timestamps do not masquerade as zero. The Rust workspace's existing unit tests are independent and should still run.

For a real LG/webOS client, use a **separate** fork-local TV integration procedure. This portable harness deliberately does not control a TV or install software on the host.
