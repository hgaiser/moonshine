# Capture and encode pipeline

This guide describes current admission, ownership and performance constraints.
For the system context see [ARCHITECTURE.md](ARCHITECTURE.md). The dated
[optimization report](reports/PIPELINE_OPTIMIZATION.md) preserves measurements,
tradeoffs and a rejected shader experiment.

## Capture admission and presentation

`compositor/admission.rs` provides receiver-driven demand, one owned credit and
one handoff slot. The receiver requests a capture only when useful; the compositor
claims the credit before direct export, fd duplication, scene-element construction
or GLES rendering. Without demand it retains dirty state and skips capture work.

| Consumer | When demand is useful | When admission ends |
| --- | --- | --- |
| PyroWave | Prior frame's encode, packetization and socket send have completed | Actual socket-send completion, or an error/drop |
| Conventional codec | Fewer than three useful frames outstanding, including network work | Final socket submission, transport failure, epoch discard or cancellation |

The source buffer's `consumed` flag is independent. It releases the DMA-BUF only
after GPU reads finish; PyroWave can release the client's buffer before paced
network sending completes. RAII releases admission on early returns/errors.
Timeouts revoke unclaimed demand, not a capture already rendering.

Epoch changes drain queued frames and reject racing old sends/receives. An old
credit's completion cannot replenish the new generation. Receiver disconnect is
terminal. The capacity-one handoff slot is not another admission credit.

Demand wakes calloop through a coalesced ping and preallocated timer. Absolute
capture deadlines and application refresh callbacks share one clock grid. Both
skip whole missed slots without rebasing their phase or sending catch-up bursts;
codec changes reuse that refresh grid. Capture summaries include maximum capture
lateness and missed slots to distinguish pacing stalls from encoder stage cost.
Missed slots include static/no-demand gaps; they are not client frame-loss counts. The regular refresh timer
still dispatches Wayland, services input, releases buffers and sends frame
callbacks even while capture is blocked. Demand wakeups do not increase callback
cadence; skipped presentation feedback is discarded, not reported as presented.

Completed scanout holds must be released and resulting Wayland events flushed
before static-screen skipping. A clean screen can mean the game is waiting for
its swapchain buffers, not that client lifecycle work is unnecessary.

## Direct export and composition

Direct/override export requires complete-scene eligibility from
[COMPOSITOR.md](COMPOSITOR.md#scene-and-input-decisions). Visible cursors, overlays,
scaling and other scene content can require GLES composition. Rejection diagnostics
record the first blocking condition with fixed counters, avoiding per-frame strings.

Composition submits GLES, waits for its SyncPoint, then exports the image to the
Vulkan consumer. PyroWave packet pacing starts before compositor preparation and
rendering; the GLES fence wait and encoding consume the same frame budget rather
than extending it. Completion-based pipeline latency remains measured separately. Failed rendering/completion stops capture rather than publishing
or recycling a buffer with uncertain GPU ownership.

Do not relax direct eligibility just because PyroWave has a scaler: compositor
transforms include aspect fitting, offset, borders and sampling filters. Direct
scaling needs an explicit transform contract and pixel comparisons.

## Encoding, caches and queue selection

Conventional encoding uses Pixelforge's asynchronous pipeline with three owned
completion credits spanning submission, readback, packetization, queue residence
and socket submission. The capture thread issues demand only below that bound;
IDR replays use the same gate. The consumer moves each credit into its batch
rather than freeing it at enqueue. RAII also covers queued encoder messages,
packetization errors, sender failure, cancellation and discarded epochs.
The three credits retain the existing encode/send overlap on healthy links.

Network output storage is bounded by three frames. For a negotiated wire shard
size `S`, the protocol's maximum four unprotected blocks of 1023 data shards
bound it by `3 * 4092 * S` bytes; protected blocks have at most 255 total shards.
This is a representability bound, not a desired queue size. TRACE completion
records expose current/high-water output frames and bytes. An indefinitely
blocked socket can make those frames indefinitely old; stop and pause interrupt
socket waits, and pause is acknowledged only after old delivery is disabled.
No arbitrary already-encoded predictive frame is dropped to meet an age target.
Transport loss requests an IDR; epoch discard is followed by the existing reset
and IDR activation contract.

The source DMA-BUF's GPU-read completion remains independent of these network
credits. Do not use kernel submission as source-read completion or proof of
receiver delivery.
Rejecting capture before encoding preserves predictive reference
chains; discarding encoded reference frames can break decoding until an IDR.
PyroWave completes one encode and send before requesting the next capture.
This bounds latency but can limit FPS when capture + encode + paced send consumes
a refresh interval; enlarging queues is not a free throughput improvement.
PyroWave uses a lazily allocated, reusable CLOCK_MONOTONIC timerfd through Tokio
AsyncFd for precise packet deadlines. A mutable send borrow enforces one wait
per timer; cancellation and rearming cannot leak a prior frame's expiry. There
is no spin loop or pacing thread. Timer initialization/wait failure falls back
to Tokio's ordinary timer without failing startup. Unpaced conventional sends
do not allocate or use this timer. GSO and per-datagram fallback share the same readiness retry and observed
WouldBlock counters. An observed readiness wait or a chunk overrun moves the remaining pacing schedule forward;
no overdue later chunks are released together. The maximum within-chunk burst
remains the existing payload-capped GSO cadence (also used without GSO). FEC and
rate-control budgets remain unchanged.

See the [pacing follow-up](reports/PACING_CADENCE.md) for the correction to the
initial composited 4K120 result.

Both importers retain DMA-BUF open-file identity and full layout validation.
A recycled numeric fd or reused compositor index is insufficient identity.
Unused imports expire; conventional consumers can separately pin imports while
GPU work remains active. See [resource bounds](LONG_SESSION_PERFORMANCE.md#resource-lifetimes).

`[stream.video] pyrowave_queue = "auto"` prefers graphics, even when a
compute-only family is available. Explicit `graphics`/`compute` modes support A/B
measurement. Optional selection failure retains a graphics fallback. The logged
value is a preference, not proof of the native library's final queue choice.
Normal queue priority and external-memory synchronization remain in place.
Measure real-game contention before drawing conclusions from cube benchmarks.

## Measurement and changes needing more proof

[Benchmarking](BENCHMARKING.md) defines latency/GPU/throughput denominators;
[runtime diagnostics](LONG_SESSION_PERFORMANCE.md) explains stall signatures.
Server pipeline latency excludes game rendering, compositor CPU fence waits,
client decode and display. GPU utilization alone does not measure codec cost.

Deferred work needs specific evidence before implementation:

| Candidate | Required evidence/contract |
| --- | --- |
| Explicit EGL-to-Vulkan fence transport | Driver support, fd ownership on import failure, semaphore/buffer lifetime, reconfiguration and both encoder paths |
| Direct scaling | Matching crop, origin, borders, sampling and color output |
| RGB conversion/DWT fusion | Equivalent rounding/dithering/HDR output and lower GPU cost across representative devices; the recorded candidate failed quality/performance checks |
| Import-cache aliasing | Measured duplicate-fd churn plus preserved identity/layout checks |
| Capture/send overlap | Tightly bounded work without stale scenes, extra admission credits or epoch leakage |

Use [GPU validation](PYROWAVE.md#validation-matrix), compositor/Steam acceptance
and [reconnect checks](reconnect-validation.md) after pipeline changes. Loopback
runs cannot establish physical network capacity or end-to-end client behavior.

## Transport ownership and outcomes

Packetization plans every FEC block first and writes into disjoint views of one
zeroed contiguous frame allocation. Prefix bytes remain outside FEC; encryption
runs in place after parity and metadata. Sending borrows that owned allocation
through completion. Capacity pooling is deferred pending a measured benefit.

`TransportCompletion` distinguishes successful kernel submission, failure,
epoch discard and cancellation, separately from releasing a useful-work credit.
Logical attempted datagrams/UDP payload bytes count once even if readiness
retries or GSO fallback repeat the socket operation. `would_block_events` and
`fallback_chunks` count those additional operations. Submitted counters increase
only after successful complete socket submission. A cancelled partial frame
retains its successful prefix, with its unsent remainder reported as discarded.
Completion notifications never block resource release. Per-datagram failures
are aggregated into batch counters and warnings are limited to once per second.

Benchmark `FrameStats.enqueue` covers queue handoff/residence and `send` covers
socket work; `total` now ends at transport completion for conventional codecs too.
`wire_bytes` is successfully submitted UDP payload (including FEC and encryption),
not IP/link traffic or client delivery. The sender summary additionally estimates
Ethernet load with IPv6/UDP and framing overhead. Pipeline enqueue summaries
measure earlier handoff separately and do not report transmission throughput.
