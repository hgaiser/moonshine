# PyroWave architecture

Pyroshine treats codec, chroma sampling, bit depth, transfer function, color
primaries, matrix, range, and HDR state as independent negotiated properties.
The conventional H.264, HEVC, and AV1 paths continue to use Pixelforge and
Vulkan Video; PyroWave is a separate codec backend and never impersonates one
of those codecs.

For system/session ownership see [ARCHITECTURE.md](ARCHITECTURE.md); for capture
admission see [the pipeline guide](PIPELINE_OPTIMIZATION.md). Implementation lives
in `moonshine-core/src/session/stream/video/{format,pyrowave,packetizer,fec}.rs`,
`session/stream/video/pipeline/`, `rtsp.rs` and `healthcheck.rs` under
`moonshine-core/src/`.

## Pinned dependency

The only supported PyroWave source is:

- repository: `https://github.com/karsyboy/pyrowave`
- branch: `master` (the build uses the detached revision below, not the moving branch)
- revision: `4cff7867e603de5c9ab983fc762aad84d37c7dd6`
- C API: 0.9.0
- Granite: `1b2d1801d2910fb09ebcded2f0bb3a3a781103b5`
- Volk: `47cddf7ed97b94118a08aacb548a411188e016cc`
- Vulkan-Headers: `6802bb4733b63ed5efd3adb308a6c885ef180ea1`

Nix builds these revisions in `nix/pyrowave.nix`. CI and release packaging use
`scripts/build-pyrowave.sh`. A unit test checks that the Rust provenance and Nix
pins agree and rejects an accidental upstream PyroWave URL.

To update the dependency, review the fork relative to upstream, update every
revision above in the Nix expression and build script, update `SOURCE_REVISION`
and the required API version in `pyrowave.rs`, then run the full test suite and
the manual GPU matrix below. Never substitute the Themaister repository as an
implicit fallback.

At runtime Pyroshine normally searches the loader paths for
`libpyrowave-shared.so.0` and then `libpyrowave-shared.so`. Administrators and
developers may set `MOONSHINE_PYROWAVE_LIBRARY` to one explicit library path
for packaging tests or diagnostics. When set, no fallback path is attempted;
the library must expose exactly C API 0.9.0.

Both build paths apply the maintained patches in `nix/patches/`: the Granite
scaler's SDR normalization and its two-layer 1:1 overlay support, in that order. The fork
carries the same files and its generated `shaders/slangmosh_scaler.hpp` is
built from them. When updating
pins, review whether each patch remains needed, keep the script/Nix patch sets
aligned, and run packet validation plus the explicit C API load test documented
in [CONTRIBUTING.md](../CONTRIBUTING.md#validation). A library load test does not
exercise GPU encoding.

## Capability and negotiation extension

The normal `ServerCodecModeSupport` bits retain their existing meanings. The
PyroWave-aware wire-v1 extension uses orthogonal chroma and HDR bits:

| Bit | Mode |
| ---: | --- |
| `0x00800000` | PyroWave 4:2:0 |
| `0x01000000` | PyroWave 4:4:4 |
| `0x02000000` | PyroWave HDR10 (with either advertised chroma mode) |

The server advertises a chroma bit only after loading API 0.9.0, matching
PyroWave to the capture-verified Vulkan adapter, confirming external-memory
interoperability, and creating the corresponding SDR encoder. It advertises the
shared HDR bit only when every advertised chroma mode also passes its 10-bit
probe. Conventional codec bits are likewise set only after creating the exact
Pixelforge pixel-format/bit-depth profile.

PyroWave encoding requires a hardware Vulkan device. Software Vulkan devices
are rejected explicitly, and the selected GPU is logged when the encoder starts;
there is no CPU encoder or fallback to a conventional codec.

The DESCRIBE response advertises native version 1, the verified `186f0393`
block-format family, the `native-wire-v1 record-framed` dialect list, and only
profiles that passed startup probes. Native ANNOUNCE echoes version 1.
Nonary-compatible record ANNOUNCE instead declares record feature bit 1 or its
adaptive-FEC capability attribute. Missing, contradictory, duplicate, and unknown
markers are rejected. See [compatibility and calibration](PYROWAVE_COMPATIBILITY.md).

A compatible client selects the following ANNOUNCE SDP attributes:

| Attribute | Values |
| --- | --- |
| `x-nv-vqos[0].bitStreamFormat` | `0` H.264, `1` HEVC, `2` AV1, `3` PyroWave |
| `x-ss-video[0].chromaSamplingType` | `0` 4:2:0, `1` 4:4:4 |
| `x-nv-video[0].dynamicRangeMode` | `0` SDR, `1` HDR10 |
| `x-nv-video[0].encoderCscMode` bit 0 | `0` limited, `1` full range |
| `x-ss-pyrowave.version` | `1` |

Wire v1 uses an 8-bit intermediate for SDR and a 10-bit intermediate for HDR10.
The optional Pyroshine bit-depth attribute remains available to conventional
codecs, but contradictory or unsupported PyroWave combinations are rejected;
Pyroshine does not silently fall back to another chroma, depth, or codec.

PyroWave's current scaled RGB API always produces full-range YCbCr, so only
full-range PyroWave modes are advertised. Conventional codecs support either
range according to the negotiated VUI. Adding limited-range PyroWave requires a
clean output-range control in the fork's scaler; lying in bitstream metadata is
not an acceptable workaround.

PyroWave has floating-point decoded samples rather than a coded 8/10-bit
profile. Pyroshine's bit-depth selection controls the scaler's R8 or R16
intermediate planes; HDR10 always uses the R16 path with BT.2020/PQ metadata.

## Bitrate and bandwidth

Pyroshine passes the client's requested bitrate to conventional encoders. For
PyroWave, whose current API exposes a per-frame maximum rather than a CBR
target, Pyroshine derives that maximum in bytes from `bitrate / (frame_rate * 8)`, aligned
down to a 32-bit word exactly as wire-v1 clients do. Valid budgets range from
1 KiB to just under 3 MiB. The value is logged when the encoder starts. There
is no separate low bitrate cap for 4:4:4 or HDR.

Client default-bitrate heuristics are owned by Moonlight Qt PyroWave, not a host
congestion controller. The negotiated manual bitrate remains authoritative;
FEC feedback adjusts parity, not PyroWave codec bitrate. Live bitrate adaptation
would need an end-to-end capacity estimator.

Actual throughput depends on content and format. Measure encoded payload, UDP
bytes (including FEC/protocol/encryption) and estimated Ethernet bytes separately;
see [benchmarking](BENCHMARKING.md#gpu-cost-and-admission-diagnostics). Resolution
or the requested bitrate alone does not establish physical-link headroom.

## Transport pacing and diagnostics

PyroWave produces a complete intra frame at once. Pyroshine retains UDP GSO,
but spaces GSO super-packets across the frame's bitrate-derived transmit window
instead of submitting every chunk back-to-back. The schedule is anchored to the
capture timestamp, so GPU encode time consumes part of the window rather than
being added to it. The sender distributes chunks across the remaining time
without reordering or interleaving frames. If the whole window elapsed during
encode, or socket backpressure consumes scheduled slots, it rebases the
remaining schedule instead of emitting an unbounded catch-up burst. The
pipeline holds capture admission until actual send completion, so paced sending
cannot accumulate a backlog of rendered scenes. See [capture admission](PIPELINE_OPTIMIZATION.md#capture-admission-and-presentation).

Packet size and GSO limits are derived from negotiated shard layout, not a fixed
number of submissions per frame. `stream.video.max_packet_size` can cap client
requests for an MTU-constrained path; its on-wire meaning is documented in the
[configuration reference](CONFIGURATION.md#streamvideo).

Trace logging emits one `Video frame transport` event per frame with encoded
and wire bytes, data/parity shard counts, FEC blocks, GSO chunks, final partial
chunk size, `WouldBlock`/fallback counts, pacing lateness, send duration, and
effective wire bitrate. For a diagnostic A/B test only, setting
`MOONSHINE_VIDEO_DISABLE_GSO=1` sends individual shards while retaining the
same frame-aware pacing cadence. This is intentionally not a configuration
setting or a recommended production mode.

## FEC policy

Video FEC is configured under `[stream.video]`:

```toml
fec_mode = "auto"       # off, fixed, or auto
fec_percentage = 20     # fixed percentage or auto starting point
fec_min_percentage = 0
fec_max_percentage = 25
```

The default mode is `fixed`, preserving existing configurations that only set
`fec_percentage`. `off` emits exactly zero parity and overrides Moonlight's
minimum-parity request. `fixed` retains that minimum for small frames. `auto`
consumes Moonlight's existing Sunshine `SS_FRAME_FEC_STATUS` feedback, raises
protection after persistent/recovered loss (more quickly after unrecoverable
blocks), and decays slowly after sustained clean windows. Changes are clamped
to the configured range and take effect only at a subsequent frame boundary.
The PyroWave policy decays clean-link protection sooner because every frame is
independently decodable.

Large frames must fit representable Reed-Solomon blocks. If requested protection
does not fit, choose the largest lower integer percentage Moonlight can recover
exactly; use zero only when no protected layout is representable.

Sender parity uses the same rule as Moonlight's receiver:
`ceil(data_shards * fec_percentage / 100)`. The emitted integer percentage is
chosen first, so metadata always reconstructs the exact parity count. FEC is
computed over the complete plaintext shard and each transmitted shard is then
encrypted independently; changing that ordering would break recovery.

## Visible dimensions and scaling

The negotiated/compositor, bitstream, decoder output, and libplacebo crop all
use the true visible dimensions. PyroWave alone aligns its wavelet storage to
32 pixels internally (for example, 3840x2160 uses an internal height of 2176).
Moonlight allocates visible-sized output planes, so padded rows or columns are
never presented or fractionally rescaled. At matching source and stream sizes,
the GPU path preserves 1:1 texels. A differing compositor extent uses the
existing PyroWave GPU scaler with its quality path; it does not introduce CPU
readback or a software conversion.

## GPU path and ownership

The production data path is:

```text
application -> Pyroshine compositor GBM image -> DMA-BUF fd
            -> PyroWave-owned Vulkan device on the same physical GPU
            -> imported VkImage in GENERAL layout -> GPU scaler/color transform
            -> GPU wavelet encode -> persistently mapped encoded bitstream
```

Late-composition layers (a Steam notification, then the cursor; see
[late composition](COMPOSITOR.md#late-composition-cursor-and-steam-notifications))
travel with the frame and are passed to
`pyrowave_encoder_encode_gpu_scaled_layers_synchronous`: CPU texels are uploaded
only when their generation changes; DMA-BUF layers are imported through the same
identity-checked cache as frame sources and read in place. Both are blended 1:1
with their opacity before color conversion.

There is no CPU framebuffer readback or CPU RGB/YUV conversion. Pyroshine must
copy the compressed bitstream to host memory to send it over UDP. Imported
images are cached by DMA-BUF open-file description plus dimensions, modifier,
format, offsets, and strides. File descriptors are duplicated because the C API
takes ownership. The compositor buffer is released only after PyroWave's
packetization call has waited for the GPU encode.

Failures keep the same rule ([encoder failure contract](ARCHITECTURE.md#transport-and-failure-boundaries)).
With the pinned C source, `encode_scaled` discards its command buffer without
submitting on error, and `compute_num_packets`/`packetize` wait on the queued
fence before any other failure, so every failed frame can release its source.
Missing device/API support (`NO_VULKAN`, `NOT_IMPLEMENTED`) and a generic error
after submission are terminal and end the session; per-frame argument, external
handle and memory failures drop the frame. PyroWave frames are independent, so a
dropped frame needs no IDR. Re-check this classification when the pin changes.

Capture is receiver-driven: one credit covers encode through socket-send
completion, while GPU consumption releases the source independently. Epoch
changes invalidate queued/racing captures. Encoded output storage is reused;
see [the pipeline contract](PIPELINE_OPTIMIZATION.md). Unused imports expire;
keep identity/layout validation and child-before-parent cleanup intact.

`pyrowave.rs` owns the PyroWave C API unsafe boundary. It loads a narrow handwritten
set of symbols, validates the exact ABI, owns device/encoder/image handles, and
destroys child resources before the library/device can unload. Other Vulkan and
native unsafe boundaries exist elsewhere in Pyroshine.

## Transport extension

Wire version 1 asks PyroWave for one contiguous encoded frame (up to 3 MiB) and
sends those bytes through the ordinary GameStream packetizer. Moonlight performs
its normal RTP/NvVideoPacket reassembly and gives the complete decode unit to
`pyrowave_decoder_push_packet()`. There is no per-shard PyroWave length prefix
or padding inside the encoded frame. Existing sequencing, encryption,
multi-block FEC, and frame boundaries remain unchanged. Every PyroWave frame is
independently decodable, so IDR requests resend the last complete PyroWave frame
and reference-frame invalidation has no codec state to modify.

## Validation matrix

The automated suite covers format independence, wire-v1 capability masks,
range rejection, dependency provenance, DMA-BUF identity, color selection, and
large-frame transport layout. Hardware validation still must cover, on each
supported driver/GPU:

1. 1080p60 and 4K60/120 where supported.
2. 4:2:0 SDR and HDR10.
3. 4:4:4 SDR and HDR10.
4. Full-range metadata and SDR/HDR mode changes.
5. Packet loss, FEC recovery, and oversized keyframe delivery.
6. Long-running bitrate, latency, VRAM, and imported-image cache stability.
7. A decoder built from the same authoritative fork.

`moonshine-bench` can exercise the format axes directly. Its
`--pyrowave-matrix` mode covers 1080p, 1440p, and 4K at 60/120/144 FPS for the
selected chroma/HDR configuration. For example:

```sh
moonshine-bench --codec pyrowave --chroma 444 --bit-depth 10 \
  --full-range --hdr --resolution 3840x2160 --fps 120 --bitrate 500000000 \
  --duration 30 -- your-test-application
```

Its report includes encoded frame-size percentiles, encoded and approximate
wire throughput, packets per frame, and separates compositor queueing, import,
conversion, submit, GPU/encode wait, packetization, send, and total server-side
latency. PyroWave's
GPU scaler is included in its submit/wait measurements; it does not use the
separate Pixelforge conversion stage.

For client diagnosis, a complete frame dropped before decode can indicate a busy
client frame mailbox, not server packet loss. Compare host transport evidence
with client decode/presentation statistics. V-Sync and refresh rate can limit
presentation; decoder initialization failures or device loss require compatibility
investigation. Use [reconnect checks](reconnect-validation.md) for mode changes.

Do not describe a mode as runtime-validated merely because its unit tests or
build passed. Record the host/client revisions, GPU/driver, modes and duration.
