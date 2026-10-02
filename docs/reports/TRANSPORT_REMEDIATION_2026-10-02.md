# Transport remediation — 2026-10-02

PERF-001, PERF-002 and OBS-001 implementation and measured evidence, against
baseline `d5b3fa451de9a29b2b4ecca667104c93f5c3be96` with Batches 1–6 retained.
This is server-side validation. It does **not** certify physical 1 Gbps LAN
capacity, receiver quality or the full high-motion acceptance matrix.

## Changes and contracts

The packetizer plans all FEC block sizes, allocates one zeroed contiguous batch
and writes each block into a disjoint region. Encryption prefixes stay outside
FEC inputs; encryption occurs after parity generation, with the original nonce
reservation and block/record metadata. The final buffer moves to the sender.
There is no aggregate `extend_from` copy, including the single-block case.
Storage pooling was not added: these measurements justify removing the copy and
allocation, but do not establish that a completion-side pool would improve the
remaining cost. Zero initialization remains mandatory.

Conventional codecs retain three credits spanning encode submission through
socket completion or resource release. Queued encoder messages and network
batches own the credit. Capture demand and IDR replay admission both obey the
bound. The network channel's control capacity does not permit a hidden 128-frame
encoded backlog. At most three conventional frames are useful work outstanding;
output bytes are bounded by `3 * 4092 * S`, where `S` is the negotiated wire shard
size, including encryption. There is no finite age guarantee for a permanently
blocked socket: bounded storage and interruptible stop/reconfigure are the
contract. Source DMA-BUF GPU-read completion remains a separate ownership signal.

Healthy links keep three-way overlap. Encoded predictive frames are not
arbitrarily dropped to reduce latency. Preventing capture/encode admission
preserves reference chains. Packetization or transport failures request IDR
recovery; epoch transitions discard old work and activate a new pipeline.

Transport outcomes count logical attempted datagrams/payload bytes separately
from successfully kernel-submitted datagrams/payload bytes, failed datagrams,
discarded bytes/datagrams and resource-release completion. Readiness retries and
GSO-to-fallback retries do not double-count logical attempts. Partial outcomes
survive cancellation. Submission is not receiver delivery. Frame statistics end
at socket completion and distinguish packetizer-to-sender enqueue time from
socket-send time. Recurring sender failures are summarized at most once per
second rather than once per datagram. Five-second diagnostics aggregate outcomes;
TRACE batch completion records expose output queue frames/bytes/high-water.

GSO and fallback use the same readiness retry policy and chunk schedule.
Observed readiness waits and chunk overruns move remaining deadlines forward,
avoiding overdue-chunk catch-up after 100–500 ms stalls. Healthy send cost within
a chunk slot does not extend every interval. Conventional codecs retain their
existing immediate-send policy; PyroWave retains bitrate-based pacing. The
existing capped GSO-sized chunk is also the maximum scheduled fallback burst.
No bitrate, image quality, FEC default, packet size or codec tuning was reduced.

## Reproducible baseline and denominators

Raw evidence is in [transport-remediation](transport-remediation/).
[environment.json](transport-remediation/environment.json) records hardware and
binary identity; each GPU directory has its own executable hashes and each run
has exact argv, process CPU/RSS resources, 0.5-second CPU/RSS/GPU samples and logs.
Hardware: Ryzen 7 9800X3D, Radeon RX 9070 XT, Mesa 26.2.4-arch3.1,
DRM 3.64, kernel 7.2.8-2-cachyos, CPU governor `performance`.

The initial CPU baseline preceded hot-path edits. Release allocation/timing
comparisons use deterministic content sizes 16,000 / 128,000 / 512,000 /
2,000,000 bytes, requested FEC 0/20%, encryption off/on, packet size 1024,
minimum parity zero, keyframe=true, fixed RTP timestamp and key/nonce state.
Each of 16 cells has 20 warmups and 100 measured samples. Three sequential
paired repetitions retain all observations. Timing covers packetization;
whole-batch hashing occurs outside timing. Per-thread allocator instrumentation
counts the packetizer calling thread's allocation requests and old bytes moved
by reallocation, not worker-thread allocations or allocator internals. The
explicit assembly-copy column counts the removed aggregate full-batch copy,
not every memcpy within FEC or encryption.

GPU runs use the same `/usr/bin/vkcube` scene, direct capture, receiver drain,
10 seconds per run / 2 seconds warmup, INFO diagnostics, resolution/refresh,
codec/chroma/range, bitrate, packet size, requested FEC and encryption in each
pair. Default requested FEC is fixed 20%, minimum parity 2, packet size 1400;
PyroWave is 4:4:4/full range. H.264/HEVC/AV1 use identical existing codec settings.
Large conventional frames can exceed protected FEC representability and use the
**existing** unprotected layout; logs retain this warning. Requested FEC is not
misrepresented as actual parity. A live loopback UDP receiver replaces the old
benchmark's closed PING socket in both executables.

Original conventional latency ended at enqueue. The measurement-only baseline
patch adds socket-start/completion observation and actual successful submission
counts, while retaining original packetization, admission and pacing. The same
benchmark reader is used in both builds. Total frame age runs from export/
composition completion to socket completion; it excludes rendering, composition
CPU fences, client decode/display. Baseline PyroWave total already ended at
completion; its old send-stage value includes enqueue, so compare total age,
not the old/new PyroWave send-stage decomposition.

Use separate target directories when reproducing: both checkouts have identical
Cargo package identities and a shared target can reuse the wrong executable.
The GPU runner rejects identical binaries and checks transport-contract markers.
`invalid-identical-binaries/`, `legacy-undrained/` and `port-conflict/` preserve
excluded attempts; none supports a performance conclusion.

```sh
python3 scripts/transport_measurements.py prepare-baseline /tmp/transport-before \
  --revision d5b3fa451de9a29b2b4ecca667104c93f5c3be96
# Build baseline and current release test binaries with distinct target directories.
CARGO_TARGET_DIR=/tmp/transport-before-target cargo test \
  --manifest-path /tmp/transport-before/Cargo.toml -p moonshine-core --release --no-run
CARGO_TARGET_DIR=/tmp/transport-after-target cargo test -p moonshine-core --release --no-run
# Supply the emitted moonshine_core test executable paths:
python3 scripts/transport_measurements.py collect --before BEFORE_TEST_BINARY \
  --after AFTER_TEST_BINARY --output /tmp/transport-cpu --trials 3
python3 scripts/transport_measurements.py compare --output /tmp/transport-cpu
CARGO_TARGET_DIR=/tmp/transport-before-target cargo build \
  --manifest-path /tmp/transport-before/Cargo.toml -p moonshine-tools --release --bin moonshine-bench
CARGO_TARGET_DIR=/tmp/transport-after-target cargo build \
  -p moonshine-tools --release --bin moonshine-bench
# Requires the normal GPU/WSI prerequisites and a quiet session/port window.
python3 scripts/transport_gpu_measurements.py \
  --before /tmp/transport-before-target/release/moonshine-bench \
  --after /tmp/transport-after-target/release/moonshine-bench --output /tmp/transport-gpu
python3 scripts/transport_gpu_compare.py /tmp/transport-gpu
# Repeat a flagged case without changing its quality parameters:
# Add --only CASE_LABEL and a fresh --output directory to the GPU invocation.
```

## Packetizer results

[comparison.csv](transport-remediation/comparison.csv) retains individual trials;
`before/after-repeat-{1,2,3}.csv` retain all 4,800 paired samples. Every corresponding
whole-batch SHA-256 matches, including padding, parity, encryption and metadata.
All new explicit assembly-copy measurements are zero.

| Encoded bytes / requested FEC / encryption | Allocations before → after | Allocated request bytes before → after | Removed assembly copied bytes |
| --- | --- | --- | --- |
| 16,000 / 0 / off | 2 → 1 | 33,280 → 16,640 | 16,640 |
| 512,000 / 20 / off | 15 → 10 | 2,139,040 → 653,920 | 634,400 |
| 2,000,000 / 0 / off | 4 → 1 | 5,162,560 → 2,064,400 | 2,064,400 |

Full benchmark process user+system CPU seconds for trials 1/2/3:
1.449/1.450/1.443 before; 1.364/1.359/1.351 after. Peak RSS KiB:
16,332/16,556/16,348 before; 13,140/13,140/13,140 after. These include test setup,
hashing and all cells; they are not isolated production CPU/RSS claims.

Several packetizer p99 cells exceed +10% in individual trials, including tiny
sub-microsecond plain workloads. Repeating all workloads twice more did not
reproduce a >10% increase in the same cell across all three trials. Some encrypted
cells regress in two trials and improve in the third; those signals are retained.
The result establishes copy/allocation reduction, not a universal p99 improvement.

## GPU/loopback first-pass results

All 22 before/after pairs completed successfully. All pairs retain equal settings;
submitted UDP payload throughput changes range from -0.30% to +0.29% in this
single pass. Conventional rates are approximately the configured 650–900 Mbps
plus actual parity/padding/headers. PyroWave vkcube content uses fewer encoded
bytes than its configured ceiling (e.g. about 674 Mbps submitted payload at
4K120), so this scene does not prove 900 Mbps PyroWave saturation. All standard
1080p/4K 60/120 modes sustained their target refresh on this hardware.

[Full comparison](transport-remediation/gpu/comparison.csv) includes age
p50/p95/p99/max, enqueue/send/packetize averages, process CPU/RSS, sampled GPU
utilization and stale frames. Five first-pass p99 signals exceed +10%; they are
repeated separately below, without changing quality or averaging away outliers.

| Case | FPS before → after | Submitted UDP payload Mbps before → after | Completion age p99 µs before → after |
| --- | --- | --- | --- |
| 1920x1080-120-av1 | 120.1 → 120.0 | 923.63 → 923.04 | 5107 → 6049 |
| 1920x1080-120-h264 | 120.0 → 120.0 | 922.88 → 923.09 | 7365 → 5832 |
| 1920x1080-120-hevc | 120.0 → 120.0 | 923.22 → 923.09 | 4978 → 4970 |
| 1920x1080-120-pyrowave | 120.1 → 120.0 | 255.76 → 255.79 | 2388 → 2386 |
| 1920x1080-60-av1 | 60.0 → 60.0 | 767.7 → 767.67 | 8047 → 7538 |
| 1920x1080-60-h264 | 60.0 → 60.1 | 767.17 → 768.7 | 10843 → 8186 |
| 1920x1080-60-hevc | 60.0 → 60.1 | 767.01 → 768.68 | 7163 → 10323 |
| 1920x1080-60-pyrowave | 60.0 → 60.0 | 127.68 → 127.78 | 2387 → 2396 |
| 3840x2160-120-av1 | 120.0 → 120.0 | 922.99 → 922.85 | 7794 → 11258 |
| 3840x2160-120-h264 | 120.0 → 120.0 | 923.04 → 923.17 | 9520 → 9525 |
| 3840x2160-120-hevc | 120.1 → 120.0 | 923.48 → 922.97 | 7704 → 8656 |
| 3840x2160-120-pyrowave | 120.1 → 120.0 | 674.51 → 674.08 | 6844 → 6840 |
| 3840x2160-60-av1 | 60.1 → 59.9 | 768.54 → 766.23 | 10427 → 10692 |
| 3840x2160-60-h264 | 59.9 → 60.1 | 766.21 → 768.46 | 18149 → 10084 |
| 3840x2160-60-hevc | 60.1 → 60.1 | 768.14 → 768.27 | 10470 → 13917 |
| 3840x2160-60-pyrowave | 60.1 → 60.1 | 336.83 → 336.76 | 6843 → 6833 |
| 4k120-hevc-650000000 | 120.1 → 119.9 | 801.33 → 799.97 | 7379 → 7450 |
| 4k120-hevc-900000000 | 120.1 → 120.1 | 1107.33 → 1106.99 | 8288 → 8300 |
| 4k120-hevc-encrypted-fec-fixed | 120.1 → 120.1 | 944.56 → 944.49 | 8276 → 8394 |
| 4k120-hevc-encrypted-fec-off | 120.1 → 120.1 | 786.14 → 785.73 | 8043 → 6944 |
| 4k120-hevc-no-gso | 120.1 → 120.0 | 923.68 → 923.18 | 8531 → 8608 |
| 4k120-pyrowave-no-gso | 120.0 → 120.0 | 674.36 → 674.13 | 6860 → 6857 |

## Latency regression investigation

Every first-pass >10% p99 case was rerun twice, unchanged, in independent paired
runs. [Repeat 1](transport-remediation/gpu-repeat-1/comparison.csv) and
[repeat 2](transport-remediation/gpu-repeat-2/comparison.csv) retain raw logs and
samples. The table reports each trial rather than averaging the signals away.

| Case | Initial p99 change | Repeat 1 | Repeat 2 |
| --- | --- | --- | --- |
| 1920x1080-120-av1 | 18.45% | 25.15% | -1.94% |
| 1920x1080-60-hevc | 44.12% | -28.8% | 0.46% |
| 3840x2160-120-av1 | 44.44% | 13.07% | -0.5% |
| 3840x2160-120-hevc | 12.36% | -1.78% | -1.28% |
| 3840x2160-60-hevc | 32.92% | -0.06% | -30.09% |

All repeat throughput changes remain within 5%. No original flagged case crosses
+10% p99 in all three trials. AV1 crosses the threshold twice and then returns
to baseline; this remains a sensitivity signal, not a universal latency pass.
Stage inspection found effectively unchanged average enqueue/socket send costs
(typically ~3 µs enqueue / ~200–335 µs send); large excursions occur in GPU
encode/readback or sporadic import/scheduling timing. For example, 1080p60 HEVC
first-pass average encode wait changes 4110 → 6096 µs while socket send changes
338 → 335 µs and packetization 76 → 47 µs. This does not establish a transport
cause for the tail excursions, nor prove that driver variability is their sole
cause. Longer high-motion/client traces are needed for a production p99 budget.
Copy/allocation removal has a directly improved targeted metric; healthy-link
latency improvement is not claimed.

## Correctness and fault validation

The full workspace/all-features suite passes: 402 core and 51 WSI tests, one
ignored measurement test. Relevant coverage includes:

- Golden full-batch fingerprints and exact ordinary/PyroWave wire equivalence.
- Reed–Solomon reconstruction under burst loss, encrypted decryption, record
  metadata, zero padding, nonce/epoch ownership and multi-block limits.
- Three-credit admission held by a delayed sender; no extra admission until
  completion; exactly-once releases for send, failure, discard, cancellation,
  dropped notification receivers and queued-message destruction.
- Real loopback GSO, forced no-GSO, runtime GSO rejection into successful fallback,
  oversized-datagram fallback errors, partial/permanent failure counters.
- Raw readiness fast path/errors and genuine WouldBlock readiness invalidation.
- 100/500 ms injected fallback/no-GSO stalls, deterministic no-catch-up deadline
  arithmetic and cancellation while sending/pacing.
- Stop and reconfigure while a sender is blocked after one datagram: partial
  accounting, all credits released and no old epoch data after activation.
- Reconnect endpoint PING sequencing and duplicate pause notifications.

The pinned installed PyroWave C API load test passes. Formatting, strict workspace/all-features clippy and documentation pass;
changelog validation and eight changelog tests pass. Raw check outputs are in
[checks](transport-remediation/checks/). `cargo machete` is unavailable on this
host; dependencies were not changed. The final pause-barrier notification
consumption and its regression test were added after GPU measurement; the
healthy steady-state transport measured above is unchanged by that fix.

## Local Moonlight and Valheim validation

The selected AppImage is
`/home/karsy/Applications/moonlight-qt-pyrowave-v6.1.10-linux-x86_64.AppImage`;
its runtime reports **Moonlight 6.1.11**. Existing localhost pairing was absent,
so the selected local client was paired through the original request-bound
loopback PIN approval protocol. No pairing/crypto protection was relaxed.
Baseline and final remediation server executable identities are in
[local-client](transport-remediation/local-client/), alongside client/server
logs and [matched parameters](transport-remediation/local-client/client-parameters.json).

Both HEVC sessions request 1080p60, 750 Mbps, packet size 1400, hardware decoding,
windowed rendering, SDR/4:2:0, no V-sync, requested frame pacing and the same
fixed 20% FEC / unencrypted server settings. Moonlight warns that 750 Mbps
exceeds its conventional codec suggested range (500 Mbps); it accepts the
requested value. The server keeps that bitrate. Actual local UDP shard size is
1408 bytes; large frames retain the existing unprotected-layout fallback.
The user confirmed baseline Valheim gameplay, then closed the game. The
remediation client also initialized HEVC hardware decoding and received frames.

The baseline's final receiver summary reports 53.39 received/decoded/rendered
FPS, 0.00% network drops, 0.00% jitter drops, 0.33 ms average decode time and
0.00 ms frame queue delay. Startup, Steam, menus and user-driven gameplay are
mixed in that denominator; it is not a sustained gameplay FPS benchmark.
The remediation summary reports 52.93 received/decoded/rendered FPS, 0.00%
network drops, 0.00% jitter drops, 0.30 ms average decode time and 0.01 ms queue
delay. The user repeated the same activity and reported **no new visual artifacts,
decoder stalls or noticeable stutter**. These are receiver smoke-test outcomes;
the two sessions' startup/menu/gameplay periods are not a controlled replay.
[receiver-summary.json](transport-remediation/local-client/receiver-summary.json)
retains the denominators without presenting lower averages as a performance gain.
The after-run application stop logged a two-second Stop-job timeout; subsequent
verification found the unit inactive/dead with `Result=success`, `MainPID=0` and
all test ports free. The application-stop implementation is unchanged by this
batch; this timing signal is retained rather than classified as a proven transport
regression or silently omitted.

The attempted 30-second baseline process-resource sample caught the idle server
after disconnect and is explicitly marked invalid in its workload metadata.
It is excluded from game CPU/RSS comparisons. GPU/loopback CPU/RSS evidence
above remains valid. No equal-content performance conclusion is drawn from the
manual Valheim sessions.

## Physical link budget and remaining acceptance

A 1 Gbps LAN remains the design target; configured encoded bitrate is not wire
rate. With packet size 1400, the maximum conventional data payload is 1384 bytes,
plain UDP payload 1416 bytes, encrypted UDP payload 1448 bytes. With IPv6,
Ethernet header/FCS, preamble and inter-frame gap, add 86 bytes per datagram
(excluding VLAN/tunnels). At actual 20% parity the approximate physical-rate
factor is `1.2 * (1448 + 86) / 1384 ≈ 1.330` with encryption: 750 Mbps encoded is
about 998 Mbps before additional control/audio or reduced payload utilization;
900 Mbps is about 1197 Mbps and cannot fit that physical link. This arithmetic
is a capacity budget, not a measured LAN result. Actual emitted parity, padding,
frame sizes, IPv4/IPv6, MTU and encryption must be included per workload.

The user selected localhost and excluded the laptop from validation. Local HEVC
Valheim receiver/visual smoke checks completed as described above. The GPU A/B
matrix uses a draining loopback socket; vkcube is repeatable moving content,
not a representative high-motion game. Physical LAN captures, large-IDR/predictive
recovery under receiver loss, the full encrypted/FEC/codec client matrix, long
stalls with a real NIC, and separately timed replayable Valheim gameplay remain
unverified. They are required for a broader production acceptance claim; this
report makes no physical-LAN certification. Test faults establish sender ownership
and deadline behavior, not NIC wire timing. Queue byte high-water is exposed at
TRACE and structurally tested, but not continuously sampled in this INFO GPU
matrix. GPU samples are system-wide utilization, not isolated encoder utilization.

## Acceptance accounting

| Requested criterion | Evidence / scope |
| --- | --- |
| Remove redundant copy | All explicit assembly-copy samples zero; allocation requests fall; one zeroed owned batch |
| Wire/FEC/encryption correctness | 4,800 paired whole-batch hashes and golden/decryption/reconstruction/record tests |
| Bound conventional backlog / stop upstream work | Three completion credits, protocol byte bound, delayed-sender and admission tests |
| No stale epochs / predictive recovery | Blocked pause/stop/reconnect tests; pre-encode gating and IDR requests; receiver-loss recovery matrix remains unverified |
| Truthful submitted metrics / bounded errors | Partial/permanent/fallback/cancellation counters; one-second warning guard and five-second summaries |
| Fallback no catch-up | Shared retry/deadline policy, genuine WouldBlock, 100/500 ms fault tests; physical wire burst timing remains unverified |
| Equal-quality healthy throughput/latency | 22 matched GPU pairs, two repeats of all five flagged p99 cases, local HEVC Valheim smoke; tails retained without blanket latency claim |
| 1 Gbps design target | No quality reduction; overhead budget documented; localhost requested, physical 1 Gbps unverified |
| Before/after evidence | Raw packetizer samples, per-trial comparisons, argv/binary hashes, CPU/RSS/GPU samples, client/server logs and excluded attempts retained |
