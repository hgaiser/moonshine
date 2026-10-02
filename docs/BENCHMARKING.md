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
| `--pyrowave-queue <mode>` | `auto` | Compare PyroWave `auto` (graphics), `graphics`, or explicit `compute` at normal queue priority |
| `--pyrowave-matrix` | off | Run the PyroWave 1080p/1440p/4K matrix across 60/120/144 FPS |
| `--resolution <WxH>` | `1920x1080` | Stream resolution |
| `--fps <N>` | `60` | Target frame rate |
| `--bitrate <N>` | `20000000` | Target bitrate in bits per second |
| `--encrypt-video` | off | Enable the same video AES-GCM path used in streaming |
| `--fec-mode <mode>` | `fixed` | Select `off`, `fixed`, or `auto` FEC policy |
| `--fec-percentage <N>` | `20` | Requested parity percentage; effective layout still obeys the protocol limits |
| `--minimum-fec-packets <N>` | `2` | Negotiated minimum parity count |
| `--packet-size <N>` | `1400` | Negotiated packet size, validated by the production setup path |
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

Run the conventional resolution/FPS/codec matrix:

```sh
cargo run --release -p moonshine-tools --bin moonshine-bench -- --matrix --duration 30 --warmup 4 /usr/bin/vkcube
```

Use `--pyrowave-matrix` for PyroWave's matrix, `--composited` to force GLES,
and `--pyrowave-queue graphics` or `compute` for queue comparisons. Omitting
`--duration` runs a single mode until Ctrl+C. `--verbose` adds per-frame output;
`MOONSHINE_LOG=debug` enables diagnostic logging.

### Comparing runs

Record the checkout/backend revisions, GPU/driver, source application/scene,
resolution/FPS, codec/chroma/depth/HDR, bitrate/FEC, capture path, queue preference,
duration and warmup. Use matching workloads and multiple steady windows.
Avoid concurrent compilation or other GPU work unless contention is the subject.
A low-entropy cube cannot establish game quality or maximum-link throughput.
The benchmark exercises server encoding/loopback sending, not Moonlight decode,
display latency or physical network congestion.

### Output

Every 5 seconds, a summary is printed with:

- **Frame count & FPS** — actual encoded frames per second
- **Bitrate** — average encoded bitrate in Mbps
- **Total latency** — avg/min/max and p50/p95/p99 time for the full pipeline per frame
- **Submit latency** — avg/min/max and p50/p95/p99 CPU time spent submitting a frame to the asynchronous encoder
- **Encode wait latency** — avg/min/max and p50/p95/p99 time waiting for the asynchronous encode/readback future
- **Breakdown** — avg time per stage: channel wait, DMA-BUF import, color conversion, submit, encode wait, packetization, enqueue/residence and actual socket send; consumer queue is reported as a diagnostic included inside encode wait
- **Key frames** — number of keyframes emitted

At the end of the run, a final summary covers the entire session (excluding the warmup period).

With `--matrix`, each combination prints the same per-run summaries and the command finishes with a consolidated latency distribution table plus an average pipeline breakdown table. Matrix mode uses fixed target FPS values of 60, 120, and 360. If `--duration` is left at `0`, matrix mode defaults to 8 seconds per combination so the matrix completes.

### How It Works

The tool launches the application through `SessionManager`, negotiates stream
contexts, triggers stream gates and collects broadcast `FrameStats` after warmup.
It reuses the production compositor/encode/packet path; see
[architecture](ARCHITECTURE.md) and [capture admission](PIPELINE_OPTIMIZATION.md).

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

Pipeline summaries show encoded Mbps. Transport summaries distinguish logical
attempts, successful kernel submissions, failures and discards, and report
submitted UDP payload Mbps including FEC and protocol/encryption bytes, plus
estimated submitted Ethernet Mbps including IPv6/UDP,
Ethernet header/FCS, preamble and inter-frame gap. VLAN/tunnel overhead and other
traffic are additional. Do not treat the UDP payload as physical wire bitrate.

The benchmark's latency starts at direct export or composition completion;
it does not include game rendering, client decode/display, or the compositor
CPU fence wait. Measure those separately when evaluating end-to-end latency.
See [the historical optimization report](reports/PIPELINE_OPTIMIZATION.md) for measured results,
validation gaps, and changes deliberately deferred.

### Lifecycle cycles

Repeated-session acceptance on real hardware. Stop any running Pyroshine
service first: cycles bind the default video port and create the
`moonshine-session.service` application unit.

```sh
# Launch → stream → stop → relaunch through one session manager.
moonshine-bench --cycles 100 --cycle-log full.jsonl /usr/bin/vkcube
# Authenticated resume → ANNOUNCE → PLAY epochs on one retained application.
moonshine-bench --reconnect-cycles 100 --cycle-log reconnect.jsonl /usr/bin/vkcube
```

Each cycle changes one property, in order: none (unchanged resume), resolution
(1920x1080/1280x720), FPS (60/120), bitrate, codec (`--cycle-codecs`, default
`h264,hevc,av1,pyrowave`), encryption, SDR/HDR10, 4:2:0/4:4:4, audio
stereo/5.1/7.1, audio quality and 5/10 ms packets. Combinations the Vulkan Video
backend does not encode (conventional 4:4:4, 10-bit H.264) are normalized to the
nearest supported format; the `settings` field records what was negotiated.
A cycle passes when the epoch delivers at least a quarter of its target frames
in `--cycle-seconds` (default 2) and the loopback client receives video bytes.
Full cycles additionally require a successful stop, a bindable video port, no
active application unit and no remaining child process (XWayland) afterwards.
Every cycle records process FD/thread counts and RSS; look for a plateau.
Add `--composited` to run the cycles through forced scene composition instead
of direct export.

The benchmark has no Moonlight client: there is no ENet control, audio endpoint
or client decode. Cycles establish backend ownership and streaming continuity;
client compatibility still needs the [reconnect matrix](reconnect-validation.md).

## Packetizer and transport remediation evidence

See [the 2026-10-02 transport report](reports/TRANSPORT_REMEDIATION_2026-10-02.md)
for fixed-content CPU measurements, raw GPU/loopback runs, fault tests and remaining
physical-LAN/client acceptance work. `scripts/transport_measurements.py` prepares
an original-revision measurement harness and compares whole-batch fingerprints
as well as per-trial allocations and timing. Run measurements without concurrent
compilation, keep the governor and diagnostics unchanged, and retain outliers.
The pipeline benchmark keeps a live UDP drain but does not decode video.

The GPU A/B helper `scripts/transport_gpu_measurements.py` records the exact argv,
process CPU/RSS and system-wide GPU busy samples alongside each log. It refuses
to replace an active `moonshine-session.service` or use an occupied video port.
Finish active streaming before benchmarking and do not run compilation during
measurement. The default matrix holds the scene and 750 Mbps configuration fixed;
additional matched runs exercise encryption, FEC, no-GSO and 650/900 Mbps settings.
Configured bitrate is not proof that a physical 1 Gbps link can carry the resulting
FEC, encryption and link overhead.
