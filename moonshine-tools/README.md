# moonshine-tools

Utilities for testing and benchmarking Moonshine.

## moonshine-bench

Benchmarks Moonshine's video encoding pipeline by spawning an application inside a headless compositor, running the full encode path, and collecting per-frame timing statistics.

### Building

```bash
cargo build -p moonshine-tools --release
```

The binary will be at `target/release/moonshine-bench`.

### Usage

```
moonshine-bench [OPTIONS] <COMMAND>
```

`<COMMAND>` is the application to run inside the compositor. A good test target is `/usr/bin/vkcube` (from `vulkan-tools`), which renders an animated rotating cube.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--matrix` | off | Run the built-in 4K, 1440p, and 1080p matrix across 60/120/360 FPS and `hevc`, `h264`, and `av1` |
| `--resolution <WxH>` | `1920x1080` | Stream resolution |
| `--fps <N>` | `60` | Target frame rate |
| `--bitrate <N>` | `20000000` | Target bitrate in bits per second |
| `--codec <codec>` | `h264` | Video codec: `h264`, `hevc`, or `av1` |
| `--duration <N>` | `0` | Seconds to run before stopping (`0` = run until Ctrl+C) |
| `--warmup <N>` | `4` | Seconds to discard before recording stats |
| `--hdr` | off | Enable HDR mode |
| `--verbose` | off | Print per-frame stats instead of periodic summary |

### Examples

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
