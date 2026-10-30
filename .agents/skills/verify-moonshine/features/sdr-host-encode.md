# SDR H.264 host encode

## Sub-features

Headless compositor startup, visible frame capture, conversion to the encoder's SDR input, hardware H.264 encoding, packetization, and local UDP send.

## How to get to it (user POV)

A Moonlight user selecting an SDR H.264 host stream needs encoded frames from the server. The synthetic benchmark exercises the upstream host path without a Moonlight client.

## Driving it with the smoke harness

From the repository root, build `moonshine-bench`, run `python3 tools/verification/verify_video.py doctor`, then `python3 tools/verification/verify_video.py smoke`. Preserve `report.json` and the raw log. The result must be `PASS` with `frames > 0`.

## Gotchas

The executable `vkcube` must be installed and able to render through the headless compositor. If no application frames reach the compositor, benchmark startup can succeed without actual encoding; the wrapper deliberately treats that as failure. The host must support Vulkan Video H.264 encoding. No Moonlight decoding is tested.
