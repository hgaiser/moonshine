# HDR session encoding mode

## Sub-features

HDR session configuration, 10-bit encoder input selection and frame delivery through the host pipeline.

## How to get to it (user POV)

A Moonlight user selects HDR when their host and client support HDR10 and the application provides HDR content.

## Driving it with the smoke harness

After building and running the doctor:

```sh
python3 tools/verification/verify_video.py smoke --codec hevc --resolution 3840x2160 --fps 60 --bitrate 150000000 --hdr
```

Confirm a nonzero encoded-frame count and retain the log and report. This checks only that the pipeline can run with the HDR session setting.

## Gotchas

The default `vkcube` source is ordinarily SDR. Setting `--hdr` does **not** establish that the source rendered PQ/BT.2020, that HDR metadata was emitted, or that a real display enabled HDR mode. Report the stronger user-visible HDR condition as **unverified** until an HDR-rendering source and a real Moonlight client are exercised. Do not turn this into an HDR pass/fail assertion in generic CI.
