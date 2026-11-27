---
name: verify-moonshine
description: "Verify Moonshine's Linux host video pipeline using the repository's benchmark, portable smoke harness, and reproducible evidence. Use for encoder, capture, color conversion, HDR session mode, or stream performance changes."
---

# Verify Moonshine host video

Exercises Moonshine's **host encode path** only (not a Moonlight client or TV display). Read [tools/verification/README.md](../../../tools/verification/README.md) for full flag/artifact details and [docs/alpha-c3-baselines.md](../../../docs/alpha-c3-baselines.md) for settled RX 6700 XT limits.

## Steps

1. **Build in the Nix dev shell** (each worktree keeps its own `target/`; `sccache` shares compiled crates):
   ```sh
   nix develop -c cargo build --release -p moonshine-tools
   ```

2. **Run preflight before every smoke run** (checks binary provenance, DRM render node, UDP 47998, Moonshine `serverinfo` idle, VCN decode/encode idle, and host benchmark lock):
   ```sh
   nix develop -c python3 tools/verification/verify_video.py doctor
   ```
   Resolve every reported problem (e.g., pause background browser video decoding using VCN, or wait for an active stream) before running `smoke`.

3. **Run a single targeted smoke case** (pass `--compare-against <baseline-binary-or-report>` for A/B tests so colliding binaries fail fast):
   ```sh
   nix develop -c python3 tools/verification/verify_video.py smoke
   nix develop -c python3 tools/verification/verify_video.py smoke --codec hevc --resolution 3840x2160 --fps 60 --bitrate 150000000 --hdr
   ```
   For performance claims, run interleaved A/B pairs with matched configs and name the measured limiter.

4. **Confirm evidence**: verify exit code 0, `report.json` verdict `PASS` with positive `frames`, distinct binary `sha256` on A/B pairs, and retained `moonshine-bench.log` under `target/verification/`.

5. **Run offline harness tests** after editing `tools/verification/` or `tools/src/bin/bench.rs`:
   ```sh
   nix develop -c python3 -m unittest discover -s tools/verification -p 'test_*.py'
   ```
