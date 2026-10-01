# Capture and encode optimization report

> Historical snapshot from September 30 / October 1, 2026. Results and test
> counts describe those runs, not current validation. Workspace evidence paths
> are local artifacts and may not exist in a fresh checkout. For current design
> and procedures, use the [architecture overview](../ARCHITECTURE.md).

Measured on September 30 / October 1, 2026, using an AMD RX 9070 XT,
RADV/Mesa 26.2.3, and a Wayland `vkcube` in isolated benchmark sessions.
The production changes are in Pyroshine. PyroWave's checkout is unchanged.

## Changes delivered

| Module | Change and purpose |
| --- | --- |
| `compositor/admission.rs` (new) | Receiver-driven capture demand, one owned credit, capacity-one relay, generation invalidation, and coalesced calloop wakeup. Reject work before export/composition. |
| `compositor/mod.rs` | Register the demand timer alongside the existing lifecycle timer; stop the session on uncertain compositor GPU completion. |
| `compositor/state.rs`, `capture.rs`, `frame.rs` | Acquire admission before render work, retain scene damage while blocked, service client lifecycle, classify direct rejection, and carry the credit separately from buffer consumption. Remove the direct export's temporary window collection. |
| `compositor/gpu_timing.rs` (new) | Optional GLES elapsed-time queries, four preallocated query objects, nonblocking result collection, and disjoint handling. No GLES query/context work on the direct path. |
| `stream/video/pyrowave.rs` | Expose the pinned queue-selection and performance APIs, select queues at normal priority, aggregate stage timestamps, and count import cache activity. |
| `stream/video/pipeline/mod.rs` | Hold PyroWave admission until socket-send completion; apply admission to conventional encoder capacity; invalidate queued captures on epoch changes. |
| `stream/video/pipeline/dmabuf.rs` | Conventional import hit/miss/recreate/evict counters; retain existing identity/layout validation. |
| `stream/video/diagnostics.rs` | Separate GPU timestamps from wall-clock wait; report delivered frames, encoded/UDP/estimated Ethernet rates. |
| `stream/video/mod.rs`, `moonshine-tools/src/bin/bench.rs` | Queue configuration; `--pyrowave-queue` and `--composited` benchmark switches. |
| Configuration and benchmarking docs | Document defaults, timing interpretation, measurement boundaries, and new switches. |

No codec, bitrate, FEC, packet format, HDR metadata, color conversion, chroma,
or encryption policy was changed. No encoder/network queue was enlarged.

## Capture admission and presentation

The receiver publishes `REQUESTED` only when another capture is useful. The
compositor atomically claims it as `OCCUPIED` before direct export, FD duplication,
GLES element construction, damage processing, or render submission. It captures
the current scene at that time. With no credit, it preserves dirty state and
skips that work. There is no rendered-frame backlog to drain in PyroWave.

For PyroWave, the credit remains occupied through encode, packetization and
actual socket-send completion. The independent `consumed` flag releases the
source DMA-BUF after GPU reading, so network pacing does not retain a client's
GPU buffer unnecessarily. RAII releases admission on errors and early returns.
Timeouts revoke unclaimed demand, but cannot revoke a capture already rendering.

Generation changes drain queued old frames and reject racing old sends/receives.
An old completion cannot replenish a new epoch. Receiver disconnect is terminal.
The capacity-one channel is a handoff slot, not an additional capture credit.

Conventional codecs request capture only below the existing three-frame
in-flight bound. Their credit ends with submission; their existing asynchronous
GPU and packet-consumer pipeline remains bounded as before.

Consumer demand wakes the event loop through a coalesced eventfd ping and one
preallocated timer. It can capture after a late send without waiting an entire
additional refresh tick. Absolute deadlines skip missed slots without a catch-up
burst. The original refresh timer continues Wayland dispatch, text input, buffer
release and frame callbacks. Demand wakeups do not emit additional frame callbacks
or increase the game's callback cadence. Skipped presentation feedback is
discarded rather than falsely reported as presented.

## Direct export

Fullscreen direct and override export remain the preferred path, with the
same complete-scene eligibility. Native-size direct runs used direct export for
all 600 captures in the final steady five-second windows; no compositor GLES
render was necessary.

Five-second rejection counters identify the first blocking reason:
forced composition, cursor, Steam overlay, notification, external overlay,
dropdown, decoration/underlay, scaling, fractional scale, output origin,
opacity, surface tree/popups, transform, crop, size, missing surface,
non-DMA-BUF buffer, or other override mismatch. The classification uses an enum
and fixed counters, with no per-frame log or string construction.

Direct scaling was deliberately not relaxed. The compositor's output transform
can include aspect fitting, offset, letterboxing and selected sampling filters;
passing a differently sized source to PyroWave's scaler does not by itself
reproduce those semantics. A future optimization needs an explicit transform
contract and pixel comparisons for crop, origin, borders and sampling centers.

## GPU scheduling and timing

`[stream.video] pyrowave_queue = "auto"` prefers compute when the matching physical
GPU advertises a compute-only family. Otherwise it requests graphics. Advanced
`"graphics"` and `"compute"` overrides support hardware-specific comparison.
Failure of optional queue selection keeps graphics usable; PyroWave/Granite also
falls back internally when necessary. Startup logs the preference and dedicated
family availability once per encoder creation. The public API does not expose
the ultimately selected queue handle, so the log calls this a preference.

All modes retain the compatibility builder's normal MEDIUM global priority.
Existing external-memory acquire/release operations remain intact, and one
PyroWave queue owns the full encode operation. No new intra-process queue-family
transfer was introduced.

The compute preference is not a claim that compute is faster on every GPU. In
the cube experiment, graphics reported 0.498 ms of codec stages/frame versus
0.531 ms for auto/compute. The purpose of compute is potential overlap with a
busy game's graphics work. That interference benefit still needs real-game
frametime measurements; the graphics override is available for comparison.

GPU reports include DWT, quantization, RDO analyze, resolve, packing, scaler/color
conversion, their exclusive sum per encoded/delivered frame, and stage ms per
delivered second. Packet-only replay is accounted for in the delivered-frame
denominator. Timestamp resolution lags by Granite's bounded frame contexts.
These intervals exclude time waiting before GPU execution, but can include
preemption/contention within an interval; they are not active-CU cycle counters.
Transfers are excluded from the stage sum. `queue_to_readback_us` remains the
separate wall-clock submission-to-completion latency.

GLES queries report composition cost per composited capture, per accepted
capture (including zero-render direct captures), and per second. Unsupported,
unavailable and disjoint measurements are not reported as measured zero.
Samples may cross reporting boundaries. In ordinary operation captured and
delivered counts align; use both windows when drops/reconfiguration occur.

## PyroWave fusion investigation

The shipping path remains RGB → scaler/conversion → Y/Cb/Cr images → level-0
DWT → subsequent DWT/quantization/RDO/packing. The existing codec and scaler are
the quality reference. Pyroshine still pins
`e344479d6c0439e346c788a918ad5645713f7573`, with its existing 4:4:4 and SDR
normalization patches. Rust, Nix and packaging pins remain consistent.

A separate experimental patch fused nonlinear RGB conversion and level-0 DWT
for native-size 4:4:4 SDR709/PQ2020, reproducing intermediate UNORM rounding,
FP16 conversion, chroma midpoint, Bayer dithering and mirrored apron sampling.
Scaling/crop, 4:2:0, scRGB and transfer/primaries conversions retained the scaler.
It was tested with the same frame-size budget as the reference.

The candidate was rejected, and is **not in the production dependency or build**:

* On 4K SDR, reference DWT plus conversion measured about 0.269 ms; the improved
  fused DWT still measured about 0.323 ms. Earlier versions measured 0.51 ms.
* Decoded results repeated deterministically, but 1080p detailed dithered SDR
  differed from the reference by up to 3380/65535 (about 13.2 eight-bit code
  values), exceeding the two-code-value limit. RMS difference was 0.000491666,
  also above the test's 0.1-code-value bound. The test intentionally failed.
* The early failure means the full final candidate HDR/resolution matrix was
  not validated. The 192-invocation variant also needs an explicit device-limit
  eligibility check before any future production use.

At 4K R16 4:4:4 the proposed optimization would avoid 49.8 MB of intermediate
writes and 49.8 MB of reads/frame, about 11.9 GB/s at 120 FPS. These are theoretical
intermediate savings, **not savings delivered by this change**. Extra RGB apron
reads and conversion instructions matter; lower intermediate traffic alone was
not sufficient to justify shipping the candidate.

## Synchronization and import identity

Composition still submits GLES, waits for its SyncPoint on the CPU, then exports
to the Vulkan consumer. The dependency is preserved. Failed rendering or fence
wait now stops capture instead of publishing/recycling an uncertain buffer.

Smithay can export a native fence and PyroWave can import temporary binary
SYNC_FD semaphores. Replacing the wait was deferred: an end-to-end implementation
must negotiate both EGL/Vulkan support, preserve FD ownership on import failure,
retain the semaphore and DMA-BUF until completion, and handle foreign ownership,
reconfiguration and both consumers. Those error/lifetime paths have not been
validated on NVIDIA/Intel. No synchronization was weakened for a latency result.

Both importers retain FD-keyed caches with kcmp/same-open-file checks and complete
extent/format/modifier/plane-count/offset/stride checks. Duplicate FDs compare as
the same object, but different numeric FD keys can still create separate entries.
The measured steady PyroWave windows were 600 hits, zero misses/recreates/evictions;
startup had three or four imports. Stable-index aliasing was deferred because
there is no measured churn here, compositor direct indices are removed when
scanout holds are released, and indices can be reused across output epochs.
Index-only reuse would be unsafe. Future aliasing must retain FD and full-layout
validation and demonstrate a benefit under actual duplicate-FD churn.

## Networking and 1 GbE

The existing packet consumer remains separate, but PyroWave capture admission
stays occupied until the consumer finishes sending that frame. No extra encoded
frame queue or catch-up sender was added. Frame numbers, RTP timestamps, FEC,
adaptive FEC, reconnect epoch barriers and pacing calculations are unchanged.

All reported PyroWave runs used 150–400 Mbps configured payload limits and the
default 20% FEC. The codec's existing per-frame rate limit remains authoritative.
Measurements include UDP protocol/FEC bytes; Ethernet estimates add 86 bytes
per packet for IPv6, UDP, Ethernet header/FCS, preamble and inter-frame gap.
VLAN/tunnel overhead and other traffic are additional. The observed maxima
were approximately 497 Mbps estimated Ethernet, leaving substantial 1 GbE margin.

These are loopback send tests with pacing, not tests across a physical Ethernet
link or Moonlight client. Encryption remains covered by existing transport tests;
the performance matrix used its default disabled setting. Quality has not been
improved by spending more bits: the shipping color/codec path is unchanged.

## Measurements

Runs lasted 12 seconds with a three-second warmup. Session latency is measured
from direct export or composition completion to send completion. It excludes
game rendering, the compositor CPU fence wait, client decode and display.
CPU is the benchmark/host process percentage, not the game's CPU overhead.
Clocks were not locked, so these short sequential runs are observations rather
than statistical certification or an isolated attribution of each optimization.

| Scenario | Before FPS → after | Before → after mean latency ms | Before → after p95 ms | Stale captured drops before → after |
| --- | ---: | ---: | ---: | ---: |
| Direct HDR10 4:4:4 1080p120, 400 Mbps | 120.1 → 120.0 | 2.873 → 2.673 | 3.173 → 2.887 | 0 → 0 |
| Direct HDR10 4:4:4 4K120, 400 Mbps | 112.9 → 120.0 | 7.979 → 6.748 | 17.691 → 8.034 | 62 → 0 |
| Forced composition HDR10 4:4:4 4K120, 400 Mbps | 120.0 → 120.1 | 6.350 → 6.408 | 7.224 → 7.317 | 0 → 0 |
| Forced composition SDR8 4:4:4 4K120, 400 Mbps | 120.0 → 113.8 | 8.314 → 8.262 | 8.766 → 8.795 | 0 → 0 |

The original log named `baseline-composited4k` actually used direct export:
vkcube honored the fullscreen resize. The forced-composition baselines were
built from original Pyroshine HEAD `9dc22fd` with only the benchmark switch added.
No before-change codec/GLES timestamps were collected; they must not be inferred
from encode waits or GPU utilization. The fusion experiment has a separate
reference-versus-candidate timestamp comparison above.

The saturated SDR composition result is a material tradeoff: strict admission
serializes capture with paced sending. It rejected 34 attempts before rendering
in a steady five-second window (566 captures), with zero rendered stale drops.
It does not guarantee 120 FPS when capture+encode+send consumes a whole refresh
interval. This change does not hide that constraint with more buffering.
Startup also rejected roughly 300 attempts before the consumer became ready.

| After scenario | FPS | Codec stage GPU ms/frame | Mean / p95 pipeline ms | Estimated Ethernet Mbps |
| --- | ---: | ---: | ---: | ---: |
| SDR8 4:2:0 1080p60, 150 Mbps | 60.1 | 0.243 | 11.224 / 12.627 | 133 |
| SDR8 4:2:0 1080p120, 200 Mbps | 120.1 | 0.172 | 7.659 / 8.130 | 254 |
| HDR10 4:2:0 1080p120, 200 Mbps | 120.1 | 0.182 | 5.070 / 5.816 | 152 |
| HDR10 4:4:4 1080p120, 400 Mbps | 120.0 | 0.227 | 2.673 / 2.887 | 171 |
| HDR10 4:4:4 1440p120, 400 Mbps | 120.0 | 0.289 | 4.373 / 5.231 | 243 |
| HDR10 4:4:4 4K60, 400 Mbps | 60.1 | 1.422 | 6.852 / 8.076 | 202 |
| HDR10 4:4:4 4K120, 400 Mbps | 120.0 | 0.531 | 6.748 / 8.034 | 405 |
| SDR8 4:4:4 4K120, 300 Mbps | 119.4 | 0.517 | 8.279 / 8.958 | 390 |
| Composited HDR10 4:4:4 4K120, 400 Mbps | 120.1 | 0.528 | 6.408 / 7.317 | 405 |
| Composited SDR8 4:4:4 4K120, 400 Mbps | 113.8 | 0.526 | 8.262 / 8.795 | 497 |

All these after runs had zero stale capture drops. HDR compositing measured
0.0952 GPU ms/accepted capture and 11.4 GPU ms/second; its codec stages added
about 63.3 GPU ms/second. Direct HDR4K120 required no GLES render and measured
0.531 codec stage ms/frame, about 63.7 ms/second, separately from 0.896 ms
queue-to-readback latency. A steady CPU sample was 15.8%, versus approximately
16% in the initial direct baseline; this is not evidence about game frametimes.
The higher 4K60 timestamp cost illustrates clock/contention sensitivity.

Conventional 1080p120 SDR8 4:2:0 at 20 Mbps sustained 120 FPS:
H.264 mean/p95 2.101/2.411 ms; HEVC 1.962/2.264 ms; AV1 2.013/2.293 ms.

## Tests and remaining validation

Passed:

* `MOONSHINE_TEST_PYROWAVE=1 cargo test --workspace --offline`: 230 core tests
  and 37 WSI tests, zero failures. Baseline: 214 core plus 37 WSI tests.
* `cargo clippy --workspace --all-targets --offline -- -D warnings`.
* `cargo fmt --all --check`, `git diff --check`, and the release benchmark build.
* Reference PyroWave GPU color test: all 40 cases, with numerical reports
  identical before/after. Covers SDR709, PQ2020, scRGB transfer conversions,
  R8/R16, 4:2:0/4:4:4, dithering, odd dimensions and scaled input.
* PyroWave packet-validation CTest.
* Local hardware runs listed above, graphics override, and conventional codecs.

New targeted tests cover admission without demand, demand timeout, one-credit
exclusion, error/drop release, buffer consumption independent of network credit,
disconnect, queued-buffer release, reset generations, old-render sends, receive
races, coalesced demand wakeups, cadence without catch-up, direct rejection
classification, queue fallback/overrides and GPU-report parsing. Existing tests
continue covering FD recycling/dup identity, HDR formats, chroma, packet recovery,
encryption, pacing cancellation, epoch barriers and configuration round trips.

Not validated here: actual game frametimes/1%/0.1% lows, GPU bandwidth counters,
Steam overlay/notification and visible-cursor hardware scenarios, end-to-end
Moonlight latency/visual inspection, live remote reconnect/reconfiguration,
physical 1 GbE congestion, or NVIDIA/Intel driver behavior. Unit tests do not
replace those acceptance scenarios.

Next work should first measure real-game contention and those lifecycle cases.
The saturated-send case warrants evaluating tightly bounded, just-in-time
overlap without capturing a future stale frame. Explicit-fence transport needs
driver/error-path validation. Direct scaling needs a transform contract. Fusion
needs equivalent conversion/dither results and lower timestamps on representative
GPUs before adoption. Cache aliasing needs measured duplicate-FD misses.

## Reproduction and evidence

```sh
cargo build --release --offline -p moonshine-tools --bin moonshine-bench
MOONSHINE_TEST_PYROWAVE=1 cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings

target/release/moonshine-bench --codec pyrowave --chroma 444 --bit-depth 10 \
  --hdr --resolution 3840x2160 --fps 120 --bitrate 400000000 \
  --duration 12 --warmup 3 --pyrowave-queue auto -- \
  vkcube --wsi wayland --width 1920 --height 1080
```

Add `--composited` for fallback composition, or `--pyrowave-queue graphics` for
queue comparison. Stream mode does not imply the source is PQ: vkcube is SDR,
so the HDR benchmarks exercise SDR-to-HDR conversion; PQ/scRGB fixtures are in
the separate GPU color test.

The workspace's `optimization-evidence/` directory contains numeric log extracts,
`measurements.csv`/JSON, and the rejected `native-rgb-candidate.patch`. The patch
targets the local PyroWave `e87ba6e` checkout and is research material, not a
release patch. Complete original run logs remain under `/tmp` on this host.
