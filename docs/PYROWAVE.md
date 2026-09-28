# PyroWave and explicit video formats

Moonshine treats codec, chroma sampling, bit depth, transfer function, color
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

## Capability and negotiation extension

The normal `ServerCodecModeSupport` bits retain their existing meanings. The
PyroWave-aware extension adds:

| Bit | Mode |
| ---: | --- |
| `0x00800000` | PyroWave, 4:2:0, 8-bit intermediate |
| `0x01000000` | PyroWave, 4:2:0, 10-bit intermediate |
| `0x02000000` | PyroWave, 4:4:4, 8-bit intermediate |
| `0x04000000` | PyroWave, 4:4:4, 10-bit intermediate |

The server advertises a bit only after loading API 0.7.0, matching PyroWave to
the selected Vulkan adapter, confirming external-memory interoperability, and
creating the corresponding chroma encoder. Conventional codec bits are likewise
set only after creating the exact Pixelforge pixel-format/bit-depth profile.

A compatible client selects the following ANNOUNCE SDP attributes:

| Attribute | Values |
| --- | --- |
| `x-nv-vqos[0].bitStreamFormat` | `0` H.264, `1` HEVC, `2` AV1, `3` PyroWave |
| `x-ss-video[0].chromaSamplingType` | `0` 4:2:0, `1` 4:4:4 |
| `x-moonshine-video[0].bitDepth` | `8` or `10` |
| `x-nv-video[0].dynamicRangeMode` | `0` SDR, `1` HDR10 |
| `x-nv-video[0].encoderCscMode` bit 0 | `0` limited, `1` full range |

Legacy clients need no new attributes and retain the established SDR 8-bit or
HDR10 10-bit defaults. Unsupported combinations are rejected with RTSP 415;
Moonshine does not silently fall back to another chroma, depth, or codec.

PyroWave's current scaled RGB API always produces full-range YCbCr, so only
full-range PyroWave modes are advertised. Conventional codecs support either
range according to the negotiated VUI. Adding limited-range PyroWave requires a
clean output-range control in the fork's scaler; lying in bitstream metadata is
not an acceptable workaround.

PyroWave has floating-point decoded samples rather than a coded 8/10-bit
profile. Moonshine's bit-depth selection controls the scaler's R8 or R16
intermediate planes; HDR10 always uses the R16 path with BT.2020/PQ metadata.

## Bitrate and bandwidth

Moonshine passes the client's requested bitrate to conventional encoders. For
PyroWave, whose current API exposes a per-frame maximum rather than a CBR
target, Moonshine derives that maximum from `bitrate / frame_rate`, with a
64 KiB floor for codec overhead and complex frames. The value is logged when
the encoder starts. There is no separate low bitrate cap for 4:4:4 or HDR.

Actual bandwidth is content- and codec-dependent, so resolution alone does not
produce an honest fixed estimate. At the same quality target, 4:4:4 generally
needs more data than 4:2:0, HDR/R16 can need more than SDR/R8, 120 Hz allows
twice as many frames as 60 Hz, and 4K contains four times as many pixels as
1080p (1440p contains about 1.78 times as many). Size the requested bitrate and
network headroom accordingly, then measure the real workload. GameStream adds
the configured FEC percentage plus RTP/NvVideoPacket, optional encryption, and
the four-byte PyroWave packet-length prefix.

## GPU path and ownership

The production data path is:

```text
application -> Moonshine compositor GBM image -> DMA-BUF fd
            -> PyroWave-owned Vulkan device on the same physical GPU
            -> imported VkImage in GENERAL layout -> GPU scaler/color transform
            -> GPU wavelet encode -> persistently mapped encoded bitstream
```

There is no CPU framebuffer readback or CPU RGB/YUV conversion. Moonshine must
copy the compressed bitstream to host memory to send it over UDP. Imported
images are cached by DMA-BUF open-file description plus dimensions, modifier,
format, offsets, and strides. File descriptors are duplicated because the C API
takes ownership. The compositor buffer is released only after PyroWave's
packetization call has waited for the GPU encode.

`pyrowave.rs` is the only unsafe boundary. It loads a narrow handwritten set of
C symbols, validates the exact ABI, owns every device/encoder/image handle, and
destroys child resources before the library/device can unload.

## Transport extension

PyroWave first divides a frame into independently decodable codec packets. Each
codec packet maps to exactly one GameStream data shard and is never split or
coalesced. The payload following the ordinary GameStream frame header is:

```text
u32 little-endian codec_packet_length
codec_packet[codec_packet_length]
zero padding to the negotiated shard payload size
```

The corresponding client removes the four-byte length and padding, then calls
`pyrowave_decoder_push_packet()` once per recovered data shard. Existing
Moonlight RTP/NvVideoPacket sequencing, encryption, multi-block FEC, and frame
boundaries remain unchanged. Every PyroWave frame is independently decodable,
so IDR requests resend the last complete PyroWave frame and reference-frame
invalidation has no codec state to modify.

## Validation matrix

The automated suite covers format independence, profile masks, range rejection,
dependency provenance, DMA-BUF identity, color selection, and codec-packet
transport boundaries. Hardware validation still must cover, on each supported
driver/GPU:

1. 1080p60 and 4K60/120 where supported.
2. 4:2:0 SDR 8-bit and R16-intermediate SDR.
3. 4:4:4 SDR 8-bit and R16-intermediate SDR.
4. 4:2:0 HDR10 and 4:4:4 HDR10.
5. Packet loss with FEC recovery and partial-frame decode.
6. Long-running bitrate, latency, VRAM, and imported-image cache stability.
7. A decoder built from the same authoritative fork.

`moonshine-bench` can exercise the format axes directly. For example:

```sh
moonshine-bench --codec pyrowave --chroma 444 --bit-depth 10 \
  --full-range --hdr --resolution 3840x2160 --fps 120 --bitrate 500000000 \
  --duration 30 -- your-test-application
```

Its report separates compositor queueing, import, conversion, submit,
GPU/encode wait, packetization, send, and total server-side latency. PyroWave's
GPU scaler is included in its submit/wait measurements; it does not use the
separate Pixelforge conversion stage.

Do not describe a mode as runtime-validated merely because its unit tests or
build passed.
