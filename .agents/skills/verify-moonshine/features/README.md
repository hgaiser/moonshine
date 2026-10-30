# Moonshine host video verification feature map

A feature is verified only when the corresponding **real host** run exits successfully and retains a report with positive encoded-frame count. The tests under `tools/verification` prove only the harness's parsing and verdict behavior.

| Feature | Procedure | Observable outcome |
| --- | --- | --- |
| [SDR H.264 host encode](sdr-host-encode.md) | Default smoke | Encoded frames from the compositor; no client required |
| [HEVC and AV1 codec initialization](codec-coverage.md) | Per-codec smoke | Frames encoded using each explicitly requested codec |
| [HDR session mode](hdr-session.md) | HEVC with `--hdr` | Host pipeline accepts HDR session mode; actual HDR pixels require an HDR source |

**Not covered:** pairing, stream startup from Moonlight, webOS app controls, client decode/display, HDR10 metadata reaching a TV, input or audio playback, network latency. These need different integration procedures. Don't mark them verified based on host encoding alone.
