# PyroWave architecture

Pyroshine treats codec, chroma sampling, bit depth, transfer function, color
primaries, matrix, range, and HDR state as independent negotiated properties.
The conventional H.264, HEVC, and AV1 paths continue to use Pixelforge and
Vulkan Video; PyroWave is a separate codec backend and never impersonates one
of those codecs.

## Pinned dependency

The only supported PyroWave source is:

- repository: `https://github.com/karsyboy/pyrowave`
- branch: `master` (the build uses the detached revision below, not the moving branch)
- revision: `e344479d6c0439e346c788a918ad5645713f7573`
- C API: 0.7.0
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
the library must expose exactly C API 0.7.0.

Two inherited video options are particularly useful on unusual networks:
`stream.video.max_packet_size` caps a client's requested packet size, and
`stream.video.log_frame_spikes = true` emits warnings for frames that exceed
their time budget. Leave both at their defaults unless troubleshooting a known
MTU or latency problem.

## Capability and negotiation extension

The normal `ServerCodecModeSupport` bits retain their existing meanings. The
PyroWave-aware wire-v1 extension uses orthogonal chroma and HDR bits:

| Bit | Mode |
| ---: | --- |
| `0x00800000` | PyroWave 4:2:0 |
| `0x01000000` | PyroWave 4:4:4 |
| `0x02000000` | PyroWave HDR10 (with either advertised chroma mode) |

The server advertises a chroma bit only after loading API 0.7.0, matching
PyroWave to the selected Vulkan adapter, confirming external-memory
interoperability, and creating the corresponding SDR encoder. It advertises the
shared HDR bit only when every advertised chroma mode also passes its 10-bit
probe. Conventional codec bits are likewise set only after creating the exact
Pixelforge pixel-format/bit-depth profile.

PyroWave encoding requires a hardware Vulkan device. Software Vulkan devices
are rejected explicitly, and the selected GPU is logged when the encoder starts;
there is no CPU encoder or fallback to a conventional codec.

The DESCRIBE response contains `a=x-ss-pyrowave.version:1`, and a PyroWave
ANNOUNCE must echo the same attribute. This prevents a client and server with
different private framing rules from accidentally selecting the codec.

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
target, Pyroshine derives that maximum from `bitrate / frame_rate`, aligned
down to a 32-bit word exactly as wire-v1 clients do. Valid budgets range from
1 KiB to just under 3 MiB. The value is logged when the encoder starts. There
is no separate low bitrate cap for 4:4:4 or HDR.

Moonlight's conventional default is intentionally sublinear above 60 FPS
because predictive codecs amortize detail through time. That model made
PyroWave's bytes per frame decrease at high refresh rates. The PyroWave fork
therefore uses a separate bytes-per-frame model, starting at 400,000 bytes for
3840x2160 4:2:0 SDR and scaling with pixel count. The codec repository's
objective/subjective evaluation found roughly a 15-20% 4:4:4 bitrate penalty
at equal luminance quality and uses a preliminary ~20% HDR allowance, so this
model conservatively applies 20% for each axis rather than scaling by raw sample
count.
Those constants are quality-oriented tuning seeds and should be calibrated with
objective and subjective codec measurements. FPS is applied only afterward:
`bitrate = frame_bytes * fps * 8`. A manual bitrate disables Moonlight's
default-tracking behavior and remains authoritative.

Moonlight's `autoAdjustBitrate` preference is settings UX, not a live
congestion controller: it recalculates the default after resolution, FPS,
codec, chroma, or HDR changes. PyroWave codec bitrate is not reduced in response
to FEC feedback. Adaptive FEC is the first response to changing loss; an
end-to-end capacity estimator would be required before safely adding live
PyroWave bitrate reduction.

Actual bandwidth is content- and codec-dependent, so resolution alone does not
produce an honest fixed estimate. At the same quality target, 4:4:4 generally
needs more data than 4:2:0, HDR/R16 can need more than SDR/R8, 120 Hz allows
twice as many frames as 60 Hz, and 4K contains four times as many pixels as
1080p (1440p contains about 1.78 times as many). Size the requested bitrate and
network headroom accordingly, then measure the real workload. GameStream adds
FEC parity, RTP/NvVideoPacket headers, and optional encryption. Diagnostics and
the benchmark distinguish encoded payload bitrate from approximate wire
bitrate. For a frame that cannot fit the requested protection into four
Reed-Solomon blocks, Pyroshine selects the largest lower integer percentage
that Moonlight can decode exactly. A request for 20% can therefore become
roughly 8-9% rather than abruptly becoming unprotected. Zero FEC is used only
when no protected layout is representable.

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

There is no CPU framebuffer readback or CPU RGB/YUV conversion. Pyroshine must
copy the compressed bitstream to host memory to send it over UDP. Imported
images are cached by DMA-BUF open-file description plus dimensions, modifier,
format, offsets, and strides. File descriptors are duplicated because the C API
takes ownership. The compositor buffer is released only after PyroWave's
packetization call has waited for the GPU encode. If synchronous encode falls
behind, the integration releases stale queued compositor frames and encodes the
newest one instead of emitting a catch-up burst. Encoded output storage is
reused, and the send stage applies backpressure until the batch has been handed
to the UDP socket rather than accumulating frames in the packet channel.

`pyrowave.rs` is the only unsafe boundary. It loads a narrow handwritten set of
C symbols, validates the exact ABI, owns every device/encoder/image handle, and
destroys child resources before the library/device can unload.

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

On Moonlight Qt PyroWave, `frames dropped by client frame queue`
means a complete reassembled encoded frame arrived while the client's single
pending-frame mailbox was still occupied; it is counted before decode. The
mailbox intentionally retains the newest frame to bound latency. PyroWave uses
Moonlight direct submit to avoid an additional 15-frame decode-unit queue. The
render thread takes the newest complete frame, submits decode, and then waits
for presentation capacity so GPU decode can overlap the swapchain wait.

V-Sync remains a normal Moonlight user preference. If a high-refresh PyroWave
stream is unexpectedly presentation-limited, test with Moonlight V-Sync
disabled; this selects the lowest-latency present mode supported by the Vulkan
driver. This is a troubleshooting step, not a universal requirement. At 120 fps
the decode-and-present path still has only 8.33 ms per frame, and a 60 Hz
display cannot present 120 unique frames. Failure to initialize the decoder, an
invalid decode result, or Vulkan device loss indicates a compatibility problem
rather than a pacing preference.

Do not describe a mode as runtime-validated merely because its unit tests or
build passed.
