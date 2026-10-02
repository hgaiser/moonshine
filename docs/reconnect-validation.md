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

Before client testing, `moonshine-bench --reconnect-cycles 100` and `--cycles 100`
([lifecycle cycles](BENCHMARKING.md#lifecycle-cycles)) exercise the same
backend transitions on the host without a client.

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

During disconnect, verify `Control peer disconnected; paused media delivery and released input for
resume` when ENet reports the active peer disconnect. A UDP PING discovers an
endpoint but cannot bypass a paused epoch. Only PINGs from the client of the
latest launch/resume are accepted (with its `X-SS-Ping-Payload` when the client
supports session IDs), so a delayed PING from a previous connection or another
host cannot redirect the resumed stream. A resume also disconnects the previous
generation's control peer without pausing the new epoch. RTSP ANNOUNCE pauses audio and video for every
reconnect; PLAY waits for ordered encoder/key/sample/FEC reset and transport activation
before completing, including identical-mode resumes. Dropped paused batches must
still release PyroWave's send completion/capture credit.

Repeat PyroWave → PyroWave, PyroWave → HEVC, HEVC → HEVC and HEVC →
PyroWave → PyroWave while the same game remains running. Inspect client frame
loss, presentation cadence and visible micro-stutter, as well as average host FPS.

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

Teardown is ordered and complete ([session ownership](ARCHITECTURE.md#session-ownership-and-shutdown)):

- cancel during application start (including a launch that outlives the HTTP
  timeout), before `StartB`, during a reconnect and while streaming; each must log
  `Session stopped; ready for a new session.`, leave
  `systemctl --user status moonshine-session.service` inactive and free the
  video/audio/control UDP ports, and an immediate relaunch must succeed;
- quit the application while cancelling: one teardown, one unit stop;
- a duplicate or early PLAY is rejected and leaves the retained application running;
- a reconnect whose reconfiguration fails (for example an HDR mode the
  compositor cannot apply) ends the session, application included, rather than
  continuing with partially changed settings;
- stop the service during streaming: it exits only after the unit stopped, within
  the 16 s teardown deadline.

Unit tests of reconnect decisions and epoch barriers do not prove remote-client
resume. Include relevant [compositor checks](COMPOSITOR.md#validation-and-runtime-checks)
and [GPU matrix](PYROWAVE.md#validation-matrix), reporting hardware/client coverage.

## Audio and controlling-peer input

For same-mode resume, repeat with unchanged keys and fresh keys. Play recognizable
PCM before/after the disconnect. Verify silence after Pause acknowledgment even
when PING discovers the next endpoint; the first new audio must use its current
key, channel layout, 5/10 ms duration and restarted sequence/FEC group. Change
quality, stereo/5.1/7.1 and duration independently and together. Pulse application
connections must remain open, with no application restart. Existing source streams
are converted into the new sink layout; applications may choose a new source
format when they next negotiate playback.

Disconnect while holding a key and modifier, mouse button, touch and pen contact,
queued clipboard text, each stick/trigger and controller button. No input may remain
active after the cleanup acknowledgment. Disconnect immediately before a Home/Guide
hold deadline and during its activation rumble; no old shortcut may fire. Reconnect
with the same controller and a different subtype. Check Edge extra buttons, ordinary
DualSense, Xbox and Nintendo controllers over USB/Bluetooth, and Steam Input where
supported. Verify rumble, LED, motion requests and adaptive-trigger feedback on the
current client. Deliver a late old-peer disconnect while the new peer holds input;
it must leave that input intact. Controller devices are neutralized but stay
enumerated at ownership loss: check `/proc/bus/input/devices` keeps the same
`Moonshine` entries across a resume, that the server logs `Gamepad bound to the
controlling peer` rather than `Gamepad N connected`, and that Steam raises no
overlay (no `direct_reject_overlay` in the capture diagnostics). Reconnecting a
different subtype must still recreate the device, and a controller the new client
does not report must disappear.

For a full stop, run several launch → stream → quit cycles and confirm
`pgrep -P "$(pgrep -x pyroshine)" -x Xwayland` returns nothing and no new
`/tmp/.X*-lock` remains after each `Session stopped; ready for a new session.`;
a resume keeps the same XWayland PID and display.

Automated coverage and remaining hardware checks for Batch 4 are recorded in
[the remediation report](reports/RECONNECT_AUDIO_INPUT_2026-10-02.md).
