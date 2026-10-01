# Reconnect and stream reconfiguration validation

## Setup

Use the intended installed server/WSI/PyroWave build and record host/client
revisions, GPU/driver and starting settings. Confirm initial launch has audio,
video, correct mode metadata and working input before testing resume. For the
negotiation/epoch contract see [ARCHITECTURE.md](ARCHITECTURE.md#session-lifecycle-and-negotiation).

## Mode matrix

For each row, launch the first mode, confirm video and audio, disconnect the
Moonlight client without cancelling the host session, select the second mode,
and reconnect to the still-running session.

| Initial stream | Reconnected stream |
| --- | --- |
| 1920x1080 SDR H.264 | 3840x2160 SDR H.264 |
| 3840x2160 SDR HEVC | 3840x2160 HDR HEVC |
| 3840x2160 HDR HEVC | 3840x2160 HDR AV1 |
| PyroWave 4:2:0 8-bit SDR | PyroWave 4:4:4 10-bit HDR |
| Any supported mode | The identical mode |
| Any supported mode | Same codec/resolution/HDR with a different bitrate |
| Any supported mode | Same resolution with a different FPS |
| Stereo | Supported 5.1/7.1 audio layout |

Repeat reversible mode changes in both directions, including HDR back to SDR
and surround back to stereo. Where the client exposes them, also vary packet
size/encryption and exercise codec/chroma/bit-depth changes independently.

## Acceptance

For every changed-mode reconnect, verify:

- the launched host application remains running;
- no black screen occurs and the first displayed frame is independently decodable;
- the Moonlight statistics/decoder report the newly requested resolution, FPS,
  codec, bit depth, chroma, HDR state, and bitrate as applicable;
- the compositor advertises the new resolution and refresh rate to the app;
- audio resumes, including a stereo/surround change when tested;
- final cancellation stops encoder, compositor and UDP tasks and releases input
  devices/resources; a new launch then succeeds.

A client disconnect without cancellation is intentionally allowed to retain the
host application/session for resume. Check retained-session behavior separately
from final teardown; disconnect alone does not imply every host task must exit.

For the identical-mode reconnect, verify that logs contain
`Reconnect stream configuration unchanged; using fast resume path`, frame and
RTP counters restart at the client-visible epoch, and the first frame is an IDR
(or the equivalent independent PyroWave frame).

For a changed mode, verify that `Reconnect negotiation received` lists the
changed fields, the compositor log reports the new output mode when needed, and
the video/audio epoch recreation log appears before packets from the new epoch.

## Epoch and failure checks

During changed-mode negotiation, verify old frames/packets do not appear after
new-epoch activation. Repeat disconnect/reconnect several times and inspect
resource counts for accumulating buffers, imports, fds or input devices. Check
input release/recreation and HDR metadata alongside visible video/audio.

Exercise cancellation while waiting for negotiation/start and during active
sending. Unsupported negotiations must fail cleanly rather than silently change
codec/format; confirm the host can recover through the normal lifecycle. Record
which failure paths were automated, exercised live or left unperformed.

Unit tests of reconnect decisions and epoch barriers do not prove remote-client
resume. Include relevant [compositor checks](COMPOSITOR.md#validation-and-runtime-checks)
and [GPU matrix](PYROWAVE.md#validation-matrix), reporting hardware/client coverage.
