# Batch 4: audio and controlling-peer reconnect ownership

Date: 2026-10-02. Findings: STAB-006 and BUG-002 from the
[production readiness review](PRODUCTION_READINESS_REVIEW_2026-10-01.md).

## Changes and boundaries

Every reconnect ANNOUNCE pauses audio and video, and every reconnect PLAY
commits an audio context, even when settings and keys are unchanged. Audio's
ordered transport channel now has a persistent pause barrier: PING may discover
an endpoint but only the producer's BeginEpoch enables delivery. Duplicate
pauses retain discovery made while paused. An in-progress send finishes before
Pause acknowledgment; subsequent old packets are dropped.

PCM frames, packets and endpoints use the session manager's existing authorization
generation. Pulse acknowledges a sample boundary after clearing buffered PCM,
resampler history and its spare mixed frame. The encoder then snapshots keys,
recreates Opus, clears sequence/FEC state, drops queued frames and rejects later
old-generation frames. It acknowledges completion only after transport activation.
A live key-watch update cannot rekey a partial old FEC group. Pulse sockets stay
open for same-mode, duration and layout changes. Layout changes rebuild output
converters using the existing client source formats; existing stereo content is
upmixed with empty surround channels until the application negotiates a new source.
High-quality surround bitrate is constrained by the negotiated duration and a
1400-byte packet budget, including RTP/FEC headers and worst-case CBC padding.
Moonlight’s AudioStream.c uses a 1400-byte receive buffer; the previous high-quality
7.1/10 ms target needed 2560 payload bytes and could not be delivered intact.
The stream mappings and quality policy remain unchanged; at 10 ms the effective
Opus bitrate is bounded to 1,088,000 bits/s.

ControlPeers remains the authority for input ownership. Only active-peer disconnect
or authorization-generation replacement resets input. Feedback receiver replacement
retires queued feedback and native callbacks before input cleanup. The compositor
orders held-key/modifier and mouse releases, clears pending text, cancels touch and
releases pen proximity/buttons/tip. Gamepad cleanup acknowledges after neutralizing
buttons/sticks/triggers and destroying slots under the timer's mutex. Closed feedback
ownership suppresses Home/Guide deadline activation; cancellation removes its deadline.
New arrivals recreate the exact supported subtype/capabilities and bind fresh rumble,
LED, motion and adaptive-trigger callbacks. The retained application is unaffected.

## Automated validation

- `cargo test --workspace --all-features`: 411 tests passed (366 core, 45 WSI).
  Local UDP/ENet and Pulse socket tests require execution outside the socket-restricted
  sandbox; the initial sandbox run failed socket creation, then the permitted run passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-features -- -D warnings`: passed.
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --all-features`: passed.
- Changelog consistency and its 8 Python unit tests: passed.
- `git diff --check`: passed.
- `cargo machete`: unavailable in this environment; no dependencies changed.

New/extended regressions cover:

| Contract | Automated evidence |
| --- | --- |
| Pause → PING → old packet, duplicate pause, old/new endpoints | Real loopback UDP transport test; delivery stays blocked until BeginEpoch; delayed old-generation packets remain blocked after activation |
| Unchanged resume with unchanged keys and rekey | Real Opus worker test; first new ciphertext matches an independently initialized encoder and the epoch key/IV |
| Key/FEC/sequence transitions | Partial old FEC group, key-watch update before boundary, restarted RTP sequences and FEC base sequence/parity indices |
| Quality/channel/duration changes | Stereo quality, 5.1/7.1, 5/10 ms and return to stereo; new-epoch packet payloads checked against independent Opus output |
| Queued old/new PCM patterns | Late nonzero old-generation PCM with wrong frame size cannot affect new silence; playback reset clears resampler history; converter test discards old PCM and preserves source format |
| Capture/sample boundary and Pulse retention | Real Unix-socket Pulse worker remains connected across unchanged/duration/layout transitions; new capture generation and frame length checked |
| Keyboard/modifier/mouse ownership | Input handler queues releases for held A/Ctrl/left mouse and waits for compositor acknowledgment; fresh input then owns a clean key set |
| Touch/pen/text | Pointer and text events precede the ordered compositor reset command; production reset cancels contacts, pen proximity and pending text. Physical compositor effects require live validation |
| Home/Guide timers | Cancellation removes pending deadlines and prevents activation beyond expiry; native slot destruction is serialized with timer advancement |
| Peer replacement and late disconnect | Real authenticated loopback ENet replacement/disconnect emits one cleanup per owner; ControlPeers rejects late old-peer disconnect without revoking new ownership |
| Controller identity and feedback | Existing subtype/Edge/emulation tests and authorized feedback routing pass; new slots use unchanged subtype constructors with fresh feedback channels |

The local transport tests delay old work at the producer/channel boundary; they do
not emulate kernel UDP congestion. Ordered send completion before Pause acknowledgment
is enforced by the single packet-handler task.

## Remaining live validation

No `/dev/uinput`, `/dev/uhid` or GPU device was available to this task. Actual
DualSense Edge/DualSense/Xbox/Nintendo USB/Bluetooth input, Steam Input/device
reappearance, rumble/LED/motion/adaptive feedback and touch/pen seat effects were
not tested. End-to-end Moonlight audio continuity, application retention and
same-mode resume latency also require a running host/client. No pinned PyroWave
FFI/GPU integration run was performed for this audio/input batch.

Use the expanded [reconnect validation matrix](../reconnect-validation.md), especially
held analog/button inputs, pending text, just-before-deadline disconnect, same/different
controller arrivals and a delayed old-peer disconnect while new input is held.
