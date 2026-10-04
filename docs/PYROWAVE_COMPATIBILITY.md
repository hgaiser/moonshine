# PyroWave compatibility and bandwidth calibration

Native wire-v1 has exactly one production path: the pinned C API 0.8.0 encoder
returns a contiguous frame; the existing GameStream packetizer applies sequencing,
FEC and encryption; Moonlight reassembles a complete decode unit; the native
decoder validates and consumes that frame. No record adapter, partial delivery,
length prefix, padding, second encoder, or framing inference enters this path.

## Setup negotiation

PyroWave uses `bitStreamFormat:3`. DESCRIBE adds these attributes:

```sdp
a=x-ss-pyrowave.version:1
a=rtpmap:99 PYROWAVE/90000
a=x-ss-pyrowave.bitstream:186f0393
a=x-ss-pyrowave.dialects:native-wire-v1 record-framed
a=x-ss-pyrowave.profiles:420-sdr8 420-hdr10 444-sdr8 444-hdr10
```

The profile list contains only modes enabled by startup capability checks. The
bitstream ID names a verified block-format family, independently of C API ABI
or repository revision. The native source and Granite pins remain unchanged.

Native ANNOUNCE declares version `1`, optionally the explicit `native-wire-v1`
dialect and matching bitstream ID. Older native clients remain compatible.
Record ANNOUNCE declares feature bit `pyrowaveFeatures:1`, or the documented
`pyrowaveAdaptiveFec:0/1` capability, without claiming native version 1.
The updated client also sends `x-ss-pyrowave.dialect:record-framed` and the
bitstream ID. Nonary release/6.1.0-vrr18 predates those explicit echoes and is
recognized by its record capability markers. User-agent strings are irrelevant.

Unknown revisions, native versions other than 1, duplicate/empty attributes,
and contradictory dialect markers are rejected with RTSP 400 and a text reason.
Conventional codec requests bypass PyroWave dialect negotiation.

The original Nonary RTSP 400 occurred because the PyroWave-only ANNOUNCE guard
required version 1 while Nonary sent record capabilities. Accepting those
capabilities now selects a real transport contract, rather than bypassing the
version check and sending an unclassified payload.

## Record compatibility boundary

Records use the same native encoder and packetizer. A CPU scan is performed
only in record mode to describe actual record starts and the critical coarse
prefix. Short-header bytes 6–7 carry its leading packet count, and NV extraFlags
bit `0x80` marks shards that actually begin records. Straddling records are valid;
the host introduces no padding or extra GPU copies. Metadata precedes existing
FEC generation and encryption. The configured native FEC policy is reused;
compatibility does not add parity on a healthy network.

The receiving compatibility adapter strips validated optional padding, excludes
lost records, and admits partial frames only with an intact critical prefix.
Zero-parity record frames can expire after 1 ms without new data, only after
their final data packet actually arrived. Missing tails await a successor;
duplicates do not prolong deadlines. Native wire-v1 never uses this mechanism.
The native decoder clears absent coefficients and receives the compacted frame.
No legacy length-prefixed transport is implemented or inferred.

## Color capability translation

| Profile | Native HTTP requirement | Record HTTP requirement |
| --- | --- | --- |
| SDR 8-bit 420 | `0x00800000` | `0x00800000` |
| SDR 8-bit 444 | `0x01000000` | `0x01000000` |
| HDR10 10-bit 420 | 420 plus `0x02000000` | 420 plus `0x02000000` |
| HDR10 10-bit 444 | 444 plus `0x02000000` | 444 plus `0x04000000` |

Pyroshine adds the HDR444 record alias only when native HDR444 is supported.
Internal formats remain typed independent axes. Only full-range 8-bit SDR and
10-bit HDR10 are supported by the current scaled encoder; contradictory depth,
range, or chroma requests are rejected. SDR uses BT.709/sRGB; HDR uses BT.2020/PQ.

Nonary's scaled encoder leaves sequence-header color bits at zero, including
HDR. Record sessions therefore normalize default metadata from negotiated SDP
before native validation. Already matching metadata is accepted; conflicting
metadata is rejected. Native metadata is never rewritten.

## Authenticated calibration

Paired HTTPS `/serverinfo` advertises `PyroWaveBandwidthProbeBytes=33554432` and
`PyroWaveHostLinkMbps`. HTTP and unpaired clients do not receive probe discovery.
`GET /pyrowave-bandwidth-probe` checks the normal paired certificate fingerprint,
returns 32 MiB of fixed `0x50` bytes, and uses a reusable 64 KiB chunk. No client
address is accepted as a reflection target. Responses use no-store and a fixed
Content-Length. One probe may be active globally; another receives 429. A session
present at admission receives 409; unsupported hosts return 404. The response
holds its admission permit until completion/drop and stops producing chunks
after 15 seconds. Normal TLS/connection lifetime controls still apply when a
peer stops reading; this is not an independent socket deadline.

Routed physical, active, full-duplex Ethernet speed is determined from the
route's source interface and Linux sysfs; bridges, VPNs, loopback, Wi-Fi, and
unreadable speeds remain 0/unknown. Never use the fastest installed adapter.

The client runs an explicit warm-up and three measured pinned HTTPS downloads,
uses the slowest result and lowest known physical host/client capacity, and
reserves 20%. Decimal Kbps calculations use checked/wide integer arithmetic.
941 Mbps measured on 1 GbE yields 752.8 Mbps; 1000 Mbps yields 800 Mbps. There is
no 400/500 Mbps cap. This calibrates network throughput, not GPU/decode capacity
or a guarantee of loss-free UDP. Applying a recommendation is a user action.

## Validation limits

Setup fixtures, packet/FEC/encryption tests, native byte-preservation tests,
reconnect dialect changes, and real loopback HTTPS authentication/admission tests
are automated. Bidirectional GPU codec tests compare all four profiles against
self-decode; the runner in the authoritative codec repository also tests 1080p,
1440p and 4K budgets at 50–800 Mbps and 60/120 FPS. Equal decoded samples establish
block-format compatibility for those inputs, not live network/display validation.

Real clients must still exercise the [reconnect matrix](reconnect-validation.md)
and [GPU/display matrix](PYROWAVE.md#validation-matrix), including loss, reordering,
HDR luminance and sustained gigabit traffic. Do not mark cross-fork streaming as
runtime-validated merely because codec fixtures and protocol tests pass.
