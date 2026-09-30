# Reconnect and stream reconfiguration validation

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

For every changed-mode reconnect, verify:

- the launched host application remains running;
- no black screen occurs and the first displayed frame is independently decodable;
- the Moonlight statistics/decoder report the newly requested resolution, FPS,
  codec, bit depth, chroma, HDR state, and bitrate as applicable;
- the compositor advertises the new resolution and refresh rate to the app;
- audio resumes, including a stereo/surround change when tested;
- cancelling or disconnecting afterward does not leave encoder, compositor, or
  UDP tasks running.

For the identical-mode reconnect, verify that logs contain
`Reconnect stream configuration unchanged; using fast resume path`, frame and
RTP counters restart at the client-visible epoch, and the first frame is an IDR
(or the equivalent independent PyroWave frame).

For a changed mode, verify that `Reconnect negotiation received` lists the
changed fields, the compositor log reports the new output mode when needed, and
the video/audio epoch recreation log appears before packets from the new epoch.
