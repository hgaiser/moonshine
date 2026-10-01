# Benchmarking Pyroshine

The `moonshine-tools` crate provides utilities for testing and benchmarking
Pyroshine. The crate and binary names retain their upstream spelling.

## moonshine-bench

Benchmarks Pyroshine's video encoding pipeline by spawning an application inside a headless compositor, running the full encode path, and collecting per-frame timing statistics.

### Building

```bash
cargo build -p moonshine-tools --release
```

The binary will be at `target/release/moonshine-bench`.
See the [contributor guide](../CONTRIBUTING.md#manually-building-the-app) for
build dependencies and the pinned PyroWave library. Streaming benchmarks also
need the installed Vulkan layer and device rules.

### Usage

```
moonshine-bench [OPTIONS] <COMMAND>
```

`<COMMAND>` is the application to run inside the compositor. A good test target is `/usr/bin/vkcube` (from `vulkan-tools`), which renders an animated rotating cube.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--matrix` | off | Run the built-in 4K, 1440p, and 1080p matrix across 60/120/360 FPS and `hevc`, `h264`, and `av1` |
| `--composited` | off | Force the GLES fallback, independent of fullscreen direct-export eligibility |
| `--pyrowave-queue <mode>` | `auto` | Compare PyroWave `auto`, `graphics`, or `compute` at normal queue priority |
| `--pyrowave-matrix` | off | Run the PyroWave 1080p/1440p/4K matrix across 60/120/144 FPS |
| `--resolution <WxH>` | `1920x1080` | Stream resolution |
| `--fps <N>` | `60` | Target frame rate |
| `--bitrate <N>` | `20000000` | Target bitrate in bits per second |
| `--codec <codec>` | `h264` | Video codec: `h264`, `hevc`, `av1`, or `pyrowave` |
| `--chroma <mode>` | `420` | Chroma sampling: `420` or `444`, subject to codec support |
| `--bit-depth <N>` | `8` (SDR), `10` (HDR) | Select `8` or `10` bits; incompatible formats are rejected |
| `--full-range` | off | Use full-range YCbCr; PyroWave always uses full range |
| `--duration <N>` | `0` | Seconds to run before stopping (`0` = run until Ctrl+C) |
| `--warmup <N>` | `4` | Seconds to discard before recording stats |
| `--hdr` | off | Enable HDR mode |
| `--verbose` | off | Print per-frame stats instead of periodic summary |

### Examples

Run a PyroWave benchmark with the locally built backend:

```sh
MOONSHINE_PYROWAVE_LIBRARY=/tmp/pyrowave-install/lib/libpyrowave-shared.so.0 \
  target/release/moonshine-bench --codec pyrowave --chroma 444 --duration 30 /usr/bin/vkcube
```

Run a quick H.264 benchmark at 1080p60 for 30 seconds:

```bash
moonshine-bench --duration 30 --codec h264 /usr/bin/vkcube
```

Compare AV1 encoding at 4K:

```bash
moonshine-bench --resolution 3840x2160 --codec av1 --bitrate 50000000 --duration 60 /usr/bin/vkcube
```

Run the full 4K/1440p/1080p x 60/120/360 FPS x HEVC/H.264/AV1 matrix:

```bash
cargo run --release -p moonshine-tools --bin moonshine-bench -- --matrix --duration 30 --warmup 4 /usr/bin/vkcube
```

Run indefinitely until you press Ctrl+C:

```bash
moonshine-bench /usr/bin/vkcube
```

Per-frame output (useful for latency analysis):

```bash
moonshine-bench --verbose /usr/bin/vkcube
```

Filter logs via `MOONSHINE_LOG`:

```bash
MOONSHINE_LOG=debug moonshine-bench --duration 10 /usr/bin/vkcube
```

### Output

Every 5 seconds, a summary is printed with:

- **Frame count & FPS** — actual encoded frames per second
- **Bitrate** — average encoded bitrate in Mbps
- **Total latency** — avg/min/max and p50/p95/p99 time for the full pipeline per frame
- **Submit latency** — avg/min/max and p50/p95/p99 CPU time spent submitting a frame to the asynchronous encoder
- **Encode wait latency** — avg/min/max and p50/p95/p99 time waiting for the asynchronous encode/readback future
- **Breakdown** — avg time per stage: channel wait, DMA-BUF import, color conversion, submit, encode wait, packetization, send; consumer queue is reported as a diagnostic included inside encode wait
- **Key frames** — number of keyframes emitted

At the end of the run, a final summary covers the entire session (excluding the warmup period).

With `--matrix`, each combination prints the same per-run summaries and the command finishes with a consolidated latency distribution table plus an average pipeline breakdown table. Matrix mode uses fixed target FPS values of 60, 120, and 360. If `--duration` is left at `0`, matrix mode defaults to 8 seconds per combination so the matrix completes.

### How It Works

1. Spawns a `SessionManager` with a headless Smithay compositor
2. Launches the provided command as a child application
3. Sets up video/audio stream contexts with the specified codec, resolution, and bitrate
4. Starts the encoding pipeline and begins capturing frames
5. Collects `FrameStats` from each encoded frame via a broadcast channel
6. After the warmup period, accumulates and reports statistics
7. Stops after the specified duration or on Ctrl+C

### GPU cost and admission diagnostics

With `[stream.video] log_stats = true`, five-second summaries distinguish
`queue_to_readback_us` (wall-clock latency) from the PyroWave GPU timestamp
stages. `pyrowave_stage_gpu_ms_per_delivered_frame` accounts for packet-only
replays; the exclusive stage sum excludes transfers and CPU work. GPU intervals
can include contention/preemption during a stage, and are not a measurement of
active shader cycles. The reports lag by Granite's bounded timestamp contexts.

Capture summaries include direct/composited captures, pre-render rejection
attempts, stale-after-render drops, demand/busy state and optional GLES query
measurements. `compositor_gpu_ms_per_captured_frame` divides measured composition
GPU time by all accepted captures, including the zero-render direct path.
Composited timer samples may cross a five-second reporting boundary; compare
multiple windows. Unsupported or disjoint timer measurements are omitted.

Import hit/miss/recreate/evict counters preserve the existing FD identity and
complete import-layout checks. Direct rejection counters identify the first
blocking condition per attempted capture, rather than every possible condition.

Transport summaries show encoded Mbps, UDP payload Mbps including FEC and
protocol/encryption bytes, and estimated Ethernet Mbps including IPv6/UDP,
Ethernet header/FCS, preamble and inter-frame gap. VLAN/tunnel overhead and other
traffic are additional. Do not treat the UDP payload as physical wire bitrate.

The benchmark's latency starts at direct export or composition completion;
it does not include game rendering, client decode/display, or the compositor
CPU fence wait. Measure those separately when evaluating end-to-end latency.
See [the optimization report](PIPELINE_OPTIMIZATION.md) for measured results,
validation gaps, and changes deliberately deferred.
