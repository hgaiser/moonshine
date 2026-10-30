# HEVC and AV1 host encoding

## Sub-features

Codec initialization, encode submission, bitstream readback, and frame statistics for HEVC and AV1.

## How to get to it (user POV)

Moonlight clients request a negotiated video codec supported by their host GPU. A server that advertises a codec must produce actual encoded frames with it.

## Driving it with the smoke harness

Run `python3 tools/verification/verify_video.py doctor` followed by separate commands:

```sh
python3 tools/verification/verify_video.py smoke --codec hevc
python3 tools/verification/verify_video.py smoke --codec av1
```

Retain a distinct report per codec. A codec unsupported by the host fails the run; record it as **unsupported on this hardware**, not as proof of a code regression.

## Gotchas

The host may support H.264/HEVC but not AV1. Do not silently replace a codec that fails with one that works. A positive host frame count does not validate client decoding or bitstream conformance.
