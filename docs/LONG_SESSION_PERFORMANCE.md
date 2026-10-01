# Long-session streaming diagnostics

Use this guide to distinguish capture, encoder, transport and runtime stalls.
The [historical investigation](reports/LONG_SESSION_PERFORMANCE.md) preserves
September 2026 fixes, version correlations and hardware smoke-test results.
Current capture admission is described in [the pipeline guide](PIPELINE_OPTIMIZATION.md).

## Collect comparable evidence

Record server/client revisions, GPU/driver, codec, chroma/HDR, resolution/FPS,
requested bitrate, capture mode, scene/workload and elapsed session time. Compare
stable windows at the same delivered frame rate; requested bitrate and GPU
utilization alone are not comparable measures of work.

The packaged systemd service sets `MOONSHINE_LOG=moonshine=info`. Direct server
launches otherwise default to `error`; use `MOONSHINE_LOG=info` for these summaries.
With `[stream.video] log_stats = true` (default), active sessions report
five-second windows. Setting it false disables summary accumulation and process
sampling, but preserves benchmark `FrameStats`, operational warnings/errors and
`log_frame_spikes`. See [configuration](CONFIGURATION.md#streamvideo).

```sh
journalctl -u "pyroshine@$USER" --since '30 minutes ago' -o short-iso --no-pager
journalctl --user -u moonshine-session.service --since '30 minutes ago' -o short-iso --no-pager
journalctl -k --since '30 minutes ago' --no-pager
```

Protect pairing credentials and other private data before sharing logs.

## Read the samples

| Message | Useful evidence |
| --- | --- |
| `Video runtime health` | CPU (100% = one core), open fds, resident KiB, packet occupancy and independent timer lateness; unavailable values are `None` |
| `Video capture resources` | Capture path/age, dirty state, busy/held buffers, releases, retired pools, admission/rejection and GLES timing counters |
| `Video swapchain feedback` | Surface identity, actual image count (zero = unknown), Vulkan format/colorspace; WSI logs add requested/effective modes |
| `Video pipeline summary` | Completed FPS, stage timings, stale drops, in-flight/packet occupancy and PyroWave import-cache size |
| `Video direct-export rejections` | First blocking reason for direct eligibility, counted per attempt |
| `PyroWave DMA-BUF import summary` | Import-cache hit/miss/recreate/evict counters |
| `PyroWave GPU timestamps …` | Exclusive GPU-stage costs, separately from wall-clock encode waits |
| `Video transport summary` | GSO/fallback sends, `would_block_events`, rebases, send duration, pacing lateness, queue occupancy and throughput |
| `Video DMA-BUF resources` | Conventional cache and retired imports, sampled during import sweeps |

Accumulators reset per window; `/proc` sampling is independent and occurs once
per five seconds, not per frame/packet. TRACE transport logs are detailed and
per-frame; enable them only for a bounded diagnostic capture.

`encode_wait_us` includes conventional consumer queue time, not just shader
execution. Conventional `channel_send_us` measures enqueue time; PyroWave waits
for actual send completion. Paced sends consume part of the frame interval,
so long send duration alone does not prove congestion. Compare `WouldBlock`,
rebases, queue occupancy and lateness. Missing completion samples can reflect a
static scene; inspect independent runtime ticks and capture state as well.

See [benchmark interpretation](BENCHMARKING.md#gpu-cost-and-admission-diagnostics)
for GPU timestamp, composition and encoded/UDP/Ethernet measurement boundaries.

## Narrow the cause

| Signature | Investigate |
| --- | --- |
| FPS falls, captures age, encoding timings stay stable | Game frame cadence, demand, scene changes and buffer release/flush progress |
| Clean scene, completed buffers still held, client stops committing | Release-before-static-gate ordering; a swapchain-starved client cannot dirty the scene |
| Import/conversion latency or fd/cache counts grow | Full DMA-BUF identity/layout, eviction and partial allocation cleanup |
| Submit/encode wait grows with in-flight/consumer occupancy | GPU contention, encoder progress, device loss or overflow |
| Packetization grows | Shard counts and FEC layout/policy |
| `WouldBlock`, rebases, packet occupancy or send lateness grow | Socket readiness, link/receiver capacity and pacing |
| CPU approaches one core, runtime ticks are late, multiple stages stall | Worker starvation, especially raw-I/O retry loops |

Raw quinn-udp sends bypass Tokio. After `WouldBlock`, retries must run through
Tokio `async_io(WRITABLE, ...)` so repeated failures clear cached readiness;
awaiting `writable()` alone can spin. Preserve the successful raw fast path and
the cancellation/fallback behavior in `stream/video/gso_socket.rs`.

## Resource lifetimes

Retention during blocked GPU work is not automatically a leak. Check progress
and counts together before weakening ownership or synchronization.

| Resource | Bound/release contract |
| --- | --- |
| Capture handoff | Capacity one and one generation-tagged credit; old epochs cannot replenish demand |
| Conventional encode/consumer | Bounded in-flight admission and consumer channel; guards release counts on errors |
| Packet channel | Bounded, awaited sends; PyroWave waits for actual send completion |
| Held scanout buffers | Release completed holds and flush events before static skipping; retain buffers still read by GPU |
| Retired compositor pools | Drop only after all slots are consumed; output changes can retire pools |
| Conventional imports | Unused cache entries expire; retired entries and consumer references protect active views |
| PyroWave imports | Unused entries expire after synchronous GPU completion; preserve fd identity/layout checks |
| Partial Vulkan imports | Owned fd/image/memory guards clean up failures; transfer fd ownership only on successful import |
| FEC and diagnostics | Bounded policy/cache keys and windowed samples, not histories indexed by frame number |
| WSI maps | Remove instance/device/surface/swapchain state on object destruction |

Presentation timing is an inactive boundary: the compositor ignores
`SetPresentTime` and produces no `past_present_timing` events. Before enabling
that path, verify query consumption, count-only calls and `VK_INCOMPLETE`
retention semantics; current smoke tests do not establish live timing behavior.

## Long-run acceptance

1. Install the intended build and identify which server, WSI layer and PyroWave
   library are loaded. A running game retains its old layer until restart.
2. Reproduce the actual game/scene at the requested mode for long enough to cover
   the reported degradation. Keep periodic stage/resource/transport summaries.
3. Compare early and late windows: delivered FPS/latency, imports/fds/RSS, held
   buffers, queue occupancy, pacing and runtime lateness. Check kernel GPU errors.
4. Exercise static scenes, cursor/Steam overlays and direct/composited transitions,
   then [reconnect with changed and unchanged modes](reconnect-validation.md).
5. Stop/cancel and confirm resource/task cleanup; launch a new session afterward.

Use the [GPU matrix](PYROWAVE.md#validation-matrix), [benchmarks](BENCHMARKING.md)
and [compositor checks](COMPOSITOR.md#validation-and-runtime-checks) as appropriate.
Report duration, tested hardware/clients and unperformed scenarios. Short cube
runs, loopback UDP and stable resource counts cannot prove a game's symptom is
resolved or that a physical link sustains the configured bitrate.
