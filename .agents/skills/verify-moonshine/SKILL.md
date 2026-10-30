---
name: verify-moonshine
description: "Verify Moonshine's Linux host video pipeline using the repository's benchmark, portable smoke harness, and reproducible evidence. Use for encoder, capture, color conversion, HDR session mode, or stream performance changes."
---

# Verify Moonshine host video

This skill drives Moonshine's **host encode path**, not a real Moonlight client. Read [the harness README](../../../tools/verification/README.md) first. Do not treat an HDR session or an encoded-frame count as proof of HDR display, network delivery, or client rendering.

## Launch

From the Moonshine repository root:

```sh
nix develop -c cargo build --release -p moonshine-tools
```

On another Linux distribution, install the documented build dependencies and use `cargo build --release -p moonshine-tools`. The build's completion is not a healthy runtime. The executable is `target/release/moonshine-bench`; the rendering workload defaults to `vkcube`, which must be provided by the host.

The smoke command starts one isolated `moonshine-bench` instance and stops it after a finite duration. A benchmark session is not a full Moonshine service and does not pair a client.

## Doctor

Do this **before** every smoke run, and after any surprising failure:

```sh
python3 tools/verification/verify_video.py doctor
```

This checks executable paths, DRM render-node access, and whether loopback UDP port 47998 is already occupied. If the doctor fails, do not run a smoke test. Device access alone does not establish Vulkan Video codec support; only actual codec initialization and encoded-frame evidence do.

Never run a smoke test against a device being used for streaming or gaming without permission from its operator. Running as root is not a substitute for correct render permissions.

## Drive

Consult [the feature map](features/README.md); every smoke run must name the feature and codec being exercised.

```sh
python3 tools/verification/verify_video.py smoke
python3 tools/verification/verify_video.py smoke --codec hevc --resolution 3840x2160 --fps 60 --bitrate 150000000 --hdr
```

Run a single case first, not the full 27-case `moonshine-bench --matrix`. The harness fails if the benchmark errors, times out or produces no completed frames, even when the binary exits zero. If minimum FPS is part of a hardware-specific contract, specify `--min-fps` using a grounded local baseline rather than choosing a universal threshold.

If the failure depends on live client behavior, **stop** and record it as a separate Moonlight integration-test requirement. The host smoke command cannot prove client decoding or display.

## Evidence

A successful drive has three observations: the runner exits zero, `report.json` says `PASS` and contains a positive completed-frame count, and `moonshine-bench.log` shows the matching completed session. A failed run retains both files under `target/verification/`.

Record the checked-out commit SHA, input resolution/FPS/codec/HDR-session setting, frame count, observed FPS, and any available GPU timestamp sample count. `gpu_timing_samples: null` means the log did not report that metric; `0` is an explicitly reported zero count. Do not interpret either as a zero-duration hardware encode.

For performance claims, follow pstack's [benchmark checklist](https://github.com/cursor/plugins/tree/main/pstack/skills/benchmark-checklist): matched configurations, interleaved A/B runs, enough repetitions for variability, and a named limiter. A one-off smoke PASS is correctness evidence, not a performance claim.

## Cleanup

The runner stops only the process group it starts. When the deadline expires, it requests graceful interrupt and escalates signals only for that group. Do not use `pkill` or kill Moonshine by name; doing so may interrupt a real user. Inspect any external/systemd-launched descendants independently.

Preserve `report.json` and the raw log across cleanup. Check they still exist before calling verification complete. The run does not create or modify persistent Moonshine service configuration.

## Helpers and maintenance

- `python3 tools/verification/verify_video.py doctor` — read-only readiness check, aside from a brief loopback port probe.
- `python3 tools/verification/verify_video.py smoke` — bounded host encode run with artifacts.
- `python3 -m unittest discover -s tools/verification -p 'test_*.py'` — hardware-independent harness tests.

Keep this skill and the feature map aligned with `tools/src/bin/bench.rs` and `tools/verification/verify_video.py`. Changes to those files should prompt a verification-skill review. The helper unit tests have run without GPU, but the smoke procedure still requires a real Vulkan Video host to establish its runtime validity.
