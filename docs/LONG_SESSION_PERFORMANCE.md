# Long-session streaming diagnostics

## Confirmed defects

The lockfile pins Tokio **1.53.1** and quinn-udp **0.6.1**. On Linux,
`UdpSocketState::try_send` reaches `libc::sendmsg` through a borrowed descriptor.
It does not call Tokio. Tokio's `UdpSocket::writable` only awaits cached
readiness; it does not clear that readiness. Consequently, the old loop could
repeatedly receive a raw `WouldBlock` and immediately complete `writable()`.
This can starve a runtime worker and delay transmission and upstream processing.

The sender keeps its original direct raw send for the successful fast path.
After `WouldBlock`, retries run inside `UdpSocket::async_io(WRITABLE, ...)`.
Tokio's registration code clears writable readiness when this raw closure
returns `WouldBlock`, before waiting for another event. The closure performs
only quinn-udp raw I/O and updates counters. There is no extra allocation,
mutex, syscall, timer, or log on successful packet sends. The first optimistic
failure and every failed raw retry are counted. A readiness wait error or GSO
rejection still uses the existing per-shard fallback, and socket backpressure
still rebases PyroWave pacing. GSO limits, codec bitrate, frame rate, FEC,
encoder concurrency, and negotiated image formats are unchanged.

The deterministic regression test fills a nonblocking Unix datagram queue,
latches Tokio writable readiness, and invokes the same raw-send helper used
by GSO. Datagram fd readiness uses the same Tokio registration mechanism.
It verifies `Pending`, cleared readiness, no additional raw attempts over
1,000 polls without a new event, multiple consecutive `WouldBlock` events,
and eventual success after draining the queue. It does not depend on network
congestion or elapsed-time thresholds. Loopback tests separately exercise
actual quinn-udp GSO and fallback.

Two additional resource issues were found:

- PyroWave's fd-keyed imported-image map retained unused images and identity
  fds until encoder destruction. Buffer/fd churn could grow it over a session.
  Unused imports now expire after two seconds, swept every 60 import calls.
  Active swapchain images stay cached. PyroWave completes each GPU encode
  synchronously before the next import, so expired imports need no retired
  queue. A 30,000-frame simulated-time soak verifies bounded resource counts,
  active-buffer reuse, and eventual release of every cached resource.
- Conventional DMA-BUF imports leaked an image on properties, fd duplication,
  or memory-type errors, and a duplicated fd on failed allocation. Partially
  created images/memory now have an ownership guard; the fd stays owned by Rust
  until Vulkan successfully takes ownership. Mock Vulkan dispatch tests verify
  image-before-memory cleanup and successful ownership transfer.

## Version-history evidence

`v0.16.8..v0.16.9` changes WSI capability/image-count negotiation and HDR/4:4:4
handling. `v0.16.9..v0.16.10` changes loader extension discovery, properties2
dispatch, associated tests, versioning, and documentation. It does not change
the steady-state encoder, compositor, packetizer, or UDP sender.

Commit `29850c172d90a62bb44b439c3e3b936ab6eb5e27` adds frame-aware pacing,
backpressure rebasing, and diagnostics around the faulty readiness loop.
The earlier `96a1244` sender already used raw sends followed by `writable()`.
Commit `0e30ad84811c5c337924798a36fe615a2b7dbc06` repairs Vulkan loader dispatch;
it can restore a hardware workload that exposes an older defect. That is a
possible explanation for the version correlation, not a demonstrated cause
of a specific game session.

## Surrounding lifetime audit

| State | Lifetime/bound |
| --- | --- |
| Capture channel | Two frames; nonblocking producer drops full-channel captures |
| Conventional encoder in flight | Existing depth-three drop gate; consumer decrements via `InFlightGuard`, including error exits |
| Consumer channel | `MAX_FRAMES_IN_FLIGHT + 2`; IDR resubmissions can bypass the ordinary drop gate but still encounter bounded-channel backpressure |
| Packet channel | 128 messages; awaited sends apply backpressure |
| PyroWave send pipeline | One synchronous encode followed by send completion; stale queued captures are released before the next encode |
| Held scanout buffers/map | Released and map entries removed after the encoder marks capture consumption; full-channel captures are marked consumed |
| Retired compositor pools | Removed when all slots are consumed; recreated on output-mode changes, not per frame |
| Conventional DMA-BUF cache | Two-second TTL; swept every 60 imports; stale entries retired for another TTL |
| Conventional retired imports | Expire by timestamp; consumers may separately pin an `Arc` while retaining a source image view |
| Converter source views | One source view/import reference per supported input format, replaced on a new source image |
| PyroWave imports | Now expire; recycled-fd identity and format/layout checks remain |
| FEC controller | Scalar counters; 120-report windows reset; percentage stays within existing configured bounds |
| FEC encoder cache | Finite shard-count pairs constrained by GF(256), rather than frame IDs; warmed for initial FEC and populated for new representable pairs |
| Latency samples | Cleared every five seconds; no history accumulation over session duration |
| WSI object maps | Removed on Vulkan instance/device/surface/swapchain destruction |
| Presentation history | At most 16 records; currently no compositor producer |

The compositor ignores `SetPresentTime` and sends no `past_present_timing`
events. WSI's `vkGetPastPresentationTimingGOOGLE` would replay records if they
were produced, conflicting with the specification's newly-available-record
semantics. This inactive path cannot explain the reported degradation and is
left outside this fix. Before enabling timing production, consume only copied
records, preserve count-only queries, and retain uncopied records on
`VK_INCOMPLETE`. See the [Vulkan timing query specification](https://docs.vulkan.org/refpages/latest/refpages/source/vkGetPastPresentationTimingGOOGLE.html).

No other raw `WouldBlock` retry loop was found in the video path. Audio socket
output stops flushing on `WouldBlock` and resumes through its event loop.
Resource retention while work is stuck is not automatically a leak: capture
buffers must remain alive while the GPU reads them. Inspect progress and
resource counts together before changing lifetime rules.

## Default logging

With the normal INFO log filter, each active session emits five-second samples:

| Message | Fields and interpretation |
| --- | --- |
| `Video runtime health` | Process CPU percentage (100% = one CPU core), open fds, resident KiB, packet queue occupancy, runtime timer lateness. Runs independently of frame completion; missing/delayed ticks suggest runtime starvation. Unavailable measurements are `None`. |
| `Video capture resources` | Capture path, dirty-screen flag, last successful capture age, busy GBM/scanout buffers, held buffers, map size, retired pool count. Busy captures include queued frames and the encoder's current capture; the standard synchronous channel has no direct occupancy query. |
| `Video pipeline summary` | Completed frames/FPS, stale captures dropped, mean/max capture-channel wait, import, conversion, submit, consumer queue, encode wait, packetization, channel/send completion, total latency, current encoder in-flight count and packet occupancy. PyroWave also reports import-cache size. |
| `Video transport summary` | Completed frames, GSO/per-shard sends, total `would_block_events`, fallback chunks, backpressure rebases, mean/max send duration, maximum pacing lateness, sampled peak/current packet occupancy. Counters reset each window. |
| `Video DMA-BUF resources` | Conventional importer cache and retired-entry sizes, reported during import sweeps at most once per five seconds. |

These summaries use constant-size accumulators. Only the independent watchdog
reads `/proc`, once per five seconds; no process/resource scan occurs per
packet. A weak packet sender avoids extending channel or session lifetime.
Existing detailed TRACE transport fields and DEBUG latency percentiles remain.
The existing encode-backpressure warning now includes in-flight and consumer
queue occupancy.

`encode_wait_us` includes consumer queue time for asynchronous codecs. The
conventional `channel_send_us` measures enqueue time, while PyroWave waits for
actual send completion. Compare both with transport `send_us`. Paced sends
naturally consume part of the frame interval; duration alone does not prove
socket congestion. Look for `would_block_events`, rebases, queue growth, and
lateness together. Summary absence can also mean a static screen; inspect the
capture-age and dirty-screen fields.

To collect evidence after a recurrence:

```sh
journalctl -u "pyroshine@$USER" --since '30 minutes ago' -o short-iso --no-pager
journalctl --user -u moonshine-session.service --since '30 minutes ago' -o short-iso --no-pager
journalctl -k --since '30 minutes ago' --no-pager
```

Use these stage signatures to narrow the investigation:

- Capture: falling completion FPS, old captures/high channel wait, unchanged
  import/convert/encode timings; check the game's own frame cadence.
- Import/conversion: rising import or conversion timings, cache/FD growth.
- Encoder: increasing submit/encode wait with consumer queue and in-flight
  counts; device-loss or overflow warnings.
- Packetizer: growing packetization time, especially during FEC changes.
- Socket: rising `WouldBlock`, send duration, rebases, or packet occupancy.
- Runtime: CPU near a full core with delayed ticks and several stages stalling.

## Validation limits

The local Valheim report matched a 120 FPS, 500 Mbps, HDR, 4:4:4 PyroWave
session on an AMD RX 9070 XT/RADV. Its existing INFO logs contain a reconnect
and an explicit session stop, but lack transport/stage/resource samples.
There was no host panic or GPU reset recorded in the inspected interval.
Those logs cannot prove which defect caused the slowdown.

On the same RX 9070 XT, a 30-second 2560x1440 HDR/4:4:4 PyroWave vkcube
benchmark at a requested 500 Mbps completed 3,119 measured frames at 120.0 FPS
(four-second warmup excluded). It emitted GSO packets without fallback or
`WouldBlock`, held three imported images, and sampled 63 open fds throughout
the steady-state interval. Its low-entropy scene actually encoded about
136 Mbps; this is a smoke test, not a 500 Mbps game workload or long-session
acceptance test. The new stage/resource/transport logs were visible at INFO.

Ten-second conventional Vulkan encode smoke tests at 2560x1440/120 FPS and a
requested 500 Mbps also completed:

| Codec/profile | Measured FPS | Encoded / wire Mbps |
| --- | ---: | ---: |
| H.264 SDR 4:2:0 | 120.1 | 500.51 / 616.42 |
| HEVC HDR 4:2:0 | 119.1 | 496.41 / 611.37 |
| AV1 HDR 4:2:0 | 120.1 | 500.56 / 616.45 |

Each sampled three conventional imports and no retired imports. HEVC recorded
one encoder-backpressure warning; transport recorded no `WouldBlock` or
fallback. These short checks exercise high shard counts and the new logging,
not every supported device/backend. NVENC/VAAPI, client reconnects, Steam
overlay/input, PoE image-count behavior, and a prolonged Valheim run still need
manual coverage on the corresponding systems.

Validation on this checkout:

- `cargo fmt --all -- --check`
- `cargo test --locked --workspace --all-features`: 207 core and 37 WSI tests
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `cargo build --locked --release` and `cargo build --locked --release --workspace`
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --workspace --all-features`
- Changelog check and eight changelog-tool tests
- Explicit pinned PyroWave API load test
- Mutation check: restoring the old raw-send/writable loop fails the readiness
  regression test immediately; restoring the fix passes it

The built binary's healthcheck passed GPU, codec profile, DMA-BUF, WSI, and
`kcmp` checks. Its overall exit was nonzero because the installed server already
occupied HTTP/HTTPS/RTSP ports. The installed service was left running.

Automated readiness, loopback, resource ownership, cache soak, and workspace
checks establish the code invariants. They do not establish sustained 120 FPS
on every encoder/GPU or prove the game symptom is gone. Repeat the
[GPU matrix](PYROWAVE.md), [benchmarks](BENCHMARKING.md),
[reconnect checks](reconnect-validation.md), and Valheim/PoE/Steam overlay
sessions using the built revision, collecting these summaries for comparison.
