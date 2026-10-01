# Composited 4K120 pacing correction — 2026-10-01

Follow-up to the [initial optimization report](PIPELINE_OPTIMIZATION.md).
Changes were made against Pyroshine `f3078c9` (workspace version 0.16.13).
Hardware: RX 9070 XT, RADV/Mesa 26.2.3. Workload: isolated Wayland vkcube,
4K, PyroWave 4:4:4, 400 Mbps configured encoded payload, default 20% FEC.

## Cause and correction

Strict admission exposed two avoidable timing costs. The composited frame's
`created_at` was updated after the GLES fence completed, and packet pacing used
that timestamp. Composition was therefore added outside the frame's transmit
budget. Also, the packet sender used Tokio Sleep, which operates at millisecond
granularity. Sequential waits for submillisecond GSO chunk deadlines could
overshoot enough to occupy admission into the next capture slot. The initial
113.8 FPS result was not evidence of a GPU or physical Ethernet capacity limit.

The correction keeps one capture credit occupied through actual send completion:

* `ExportedFrame` retains an optional composition-start timestamp. PyroWave
  pacing includes compositor preparation/render/fence time in its existing
  frame budget. Direct export and packet-only replay keep their current pacing
  origin behavior. Completion-based benchmark latency and RTP timestamp rules
  remain separate and unchanged.
* Paced sending lazily allocates one reusable CLOCK_MONOTONIC timerfd, driven
  through Tokio AsyncFd. It waits for kernel events without spinning or adding
  a thread. Mutable socket/timer ownership prevents concurrent waits from
  rearming one another. Cancellation followed by rearming clears old expiry;
  cached readiness is cleared through try_io on WouldBlock.
* Initialization/wait failure falls back to ordinary Tokio timing, with a
  warning. Unpaced conventional sends do not allocate/use the timer. Existing
  late-frame rebasing and socket backpressure behavior remain intact.
* Pacing lateness now includes actual post-wakeup overshoot, rather than only
  arriving at a chunk after its deadline. Older lateness logs do not use this
  complete measurement and should not be compared as identical metrics.

No extra credits, frame queues, altered codec/color processing, reduced image
quality, bitrate increases, FEC changes, or GPU synchronization changes were
needed. The earlier suggestion that capture/send overlap was necessary was
incomplete: correcting the pacing window and timer precision restored this case
while retaining strict serialization.

## Results

Short runs lasted 12 seconds with three seconds excluded for warmup. Hardware
clocks were not locked. These observations are not statistical certification.
Latency still starts at composition completion, not game presentation or capture
start, and ends at socket-send completion.

| Composited SDR8 4K120 | Delivered FPS | Mean / p95 pipeline ms |
| --- | ---: | ---: |
| Original one-credit result | 113.8 | 8.262 / 8.795 |
| Composition-origin correction alone | 115.6 | See local intermediate log |
| Composition origin + precise timer, first run | 120.0 | 7.085 / 7.352 |
| Repeat with post-wakeup lateness measured | 120.0 | 7.192 / 7.377 |
| Longer run, 20 seconds measured | 120.0 | 7.374 / 7.513 |

All three complete-fix SDR runs had zero stale drops and zero steady-state pre-render
rejections. Codec stage GPU time stayed around 0.52–0.53 ms/frame and composition
around 0.075 ms/frame. The repeat's steady maximum measured pacing lateness was
231 microseconds. Estimated Ethernet traffic was about 524 Mbps, versus the
original 497 Mbps: more frames were delivered at the same per-frame cap. UDP
payload including FEC was approximately 494 Mbps. Physical 1 GbE remains untested.

The longer run delivered 2,399 frames over the 20-second measurement window,
with p99 latency 7.561 ms and one maximum of 16.303 ms. The last diagnostic
window measured a maximum pacing overshoot of 247 microseconds, zero pre-render
rejections and zero stale drops. Steady CPU usage was 17.8% of one core and
fd count stayed at 67. This confirms sustained cadence for this isolated test;
it does not eliminate occasional scheduling outliers.

Additional complete-fix observations:

| Scenario | FPS | Mean / p95 pipeline ms | Estimated Ethernet Mbps |
| --- | ---: | ---: | ---: |
| Composited HDR10 4:4:4 4K120 | 120.1 | 5.379 / 6.027 | 404 |
| Direct SDR8 4:4:4 4K120 | 120.1 | 7.544 / 7.572 | 524 |
| Composited HDR10 4:4:4 4K60 | 60.1 | 5.680 / 6.322 | 202 |

These all had zero stale capture drops. CPU samples in the short SDR runs were
17.8–19.2% of one core, compared with 16.2% in the older lower-FPS run. More
frames and more precise wakeups have costs; no CPU reduction is claimed. One
additional fd is bounded by the video socket lifetime. Real-game interference
and end-to-end Moonlight latency still require separate acceptance measurements.

## Validation and evidence

Passed all-feature workspace tests: 233 core and 37 WSI tests. New tests cover
composition pacing origin, timer reuse, deadlines not firing early and expired
cancelled waits not completing a new wait early. Existing paced-send cancellation,
UDP readiness, packet delivery, FEC, HDR, epoch/reset and pinned ABI tests pass.
Clippy passed with all features/all targets and warnings denied; release build,
rustdoc with warnings denied, formatting, changelog checks and its eight Python
tests passed. Cargo machete was unavailable; no dependencies were added.

The workspace `optimization-evidence/` contains filtered `pyroshine-pacing-fix-*`
logs. Full logs remain under `/tmp`. The current
[pipeline guide](../PIPELINE_OPTIMIZATION.md) describes ownership and fallback.
Remote client reconnect/reconfiguration, Steam overlays, real-game frametimes,
physical Ethernet congestion and NVIDIA/Intel validation were not performed.

```sh
target/release/moonshine-bench --composited --codec pyrowave --chroma 444 \
  --bit-depth 8 --resolution 3840x2160 --fps 120 --bitrate 400000000 \
  --duration 12 --warmup 3 -- \
  vkcube --wsi wayland --width 1920 --height 1080
```
