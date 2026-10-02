# Architecture overview

Pyroshine runs applications in isolated, headless Linux sessions and streams
video, audio and input to Moonlight-compatible clients. Start here for ownership
and lifecycle; use the linked guides for detailed contracts and acceptance tests.
Build/install/CI/release procedures live in [CONTRIBUTING.md](../CONTRIBUTING.md).

## Components and data flow

```text
CLI/startup (src/main.rs)
  ├─ config, host capability probes, TLS/client state, mDNS
  ├─ GameStream HTTP/HTTPS API ── launch/resume/cancel ── SessionManager
  └─ RTSP ── negotiated stream contexts / PLAY ───────── SessionManager
                                                         │
                                       application + headless compositor
                                                         │
application Vulkan presentation ── moonshine-wsi ── Wayland scene
                                                         │
                                               GBM/DMA-BUF capture
                                                         │
                                     Pixelforge or PyroWave encoder
                                                         │
                                          packetization → FEC → optional encryption → UDP

application audio → embedded PulseAudio-compatible server → Opus → UDP
client control/input → control stream → compositor input / Inputtino devices
client ← control feedback (HDR, rumble, motion/trigger requests)
```

Core module paths below are relative to `moonshine-core/src/`; WSI code lives
in `moonshine-wsi/src/`.

`moonshine-wsi` participates in Vulkan presentation; it is not the encoder or
session manager. Scene visibility belongs to the compositor. Encoding and
transport belong to `moonshine-core/src/session/stream/video/`. Control decoding
and virtual devices belong to `session/stream/control/`; focus and Wayland input targets
belong to `session/compositor/`. The audio server/encoder belong to `session/stream/audio/`.
Developer tools reuse these session interfaces rather than owning another stack.

## Startup and capability advertisement

`src/main.rs` loads config, scans applications, waits for the user D-Bus session,
and checks the host before wiring services. `--no-health-check` still probes GPU
capabilities; it does not bypass the DMA-BUF requirement. Advertised codec/HDR
support comes from actual encoder/profile probes and configuration, not merely
from finding a library or extension name.

`gpu.rs` verifies the selected Vulkan context against the opened GBM/EGL DRM
node using `VK_EXT_physical_device_drm`, with complete PCI identity as a fallback.
The compositor publishes a verified, shared `VideoContext` through the capture
channel before readiness. Conventional encoding, DMA-BUF import and PyroWave UUID
matching clone that context throughout the session and reconnects. Healthcheck uses
the same verification before profile probes, including with `--no-health-check`.
Unresolvable configuration or unknown/mismatched device identity advertises no
codec/HDR/DMA-BUF capability and prevents startup/session launch. No cross-device
capture-to-encoder path is validated or enabled. This identity check does not prove
that every application's client DMA-BUF format/modifier imports successfully;
normal import validation and terminal failure handling still apply. Applications
on a different GPU can use compositor composition if their EGL imports work;
that client-to-compositor transfer is separate from capture-to-encoder selection.

A missing/incompatible optional PyroWave library leaves conventional codecs
available. A negotiated PyroWave stream does not silently switch to another
codec or software encoder. See [PyroWave](PYROWAVE.md#capability-and-negotiation-extension).

## Session lifecycle and negotiation

`session/manager.rs` owns the single optional session; `session/mod.rs` builds
its `Initialized`, `Launched` and `Active` states. Application/compositor lifetime
and a client's stream epoch are different: a reconnect can retain the application
while resetting or replacing the encoders and transport state. Ownership,
cancellation and teardown follow [session ownership and shutdown](#session-ownership-and-shutdown).

| Step | Owner and contract |
| --- | --- |
| HTTP launch | Authenticated GameStream API initializes and launches the application/compositor |
| RTSP ANNOUNCE | Validates negotiated formats and numeric domains; for an active session pauses the live epoch, then publishes pending video/audio contexts |
| RTSP PLAY | After checking every prerequisite, constructs initial streams or commits a reconnect transition |
| Control `StartB` | Opens the persistent audio/video start latches; tools open the same latches through the manager |
| HTTP resume | Validates and publishes session keys and retains requested session parameters; RTSP remains authoritative for encoded stream properties |
| Unchanged reconnect | Pauses both streams, retains the video pipeline and Pulse sockets, resets client-visible media sequencing/encoder state, and acknowledges ordered transport activation before PLAY completes |
| Changed reconnect | Pauses both epochs, updates compositor output when needed, and commits negotiated video/audio resources before activating delivery |
| Cancel, application exit or session failure | One teardown stops the application unit and joins every worker; only then can the manager accept a later launch |

Audio uses the same Pause → producer reset → BeginEpoch barrier as video on
all reconnects, including identical settings. PING discovers a destination but
cannot activate delivery. PCM frames and audio packets carry the manager's
existing authorization generation; the encoder freezes key material at the
ordered boundary, recreates Opus state, and clears RTP/FEC state before transport
activation. Pulse clears queued PCM and resampler history before acknowledging
the sample boundary. Same-mode and duration-only resumes retain Pulse clients;
layout changes rebuild their output converters while preserving negotiated source
formats and sockets. Existing stereo sources remain stereo content when upmixed
into a surround sink until the application chooses a surround source format.

The authenticated controlling peer owns input through the existing ControlPeers
tracker. Disconnect or authorization-generation replacement closes its feedback
receiver, orders compositor key/button releases and touch/pen/text cancellation,
and waits until every virtual controller is neutralized (buttons, sticks,
triggers, touchpad contacts, angular rate), its pending Home/Guide transition and
activation pulse are cancelled, and its feedback route is revoked. Device
lifetime is separate from ownership: the virtual controllers stay plugged in, so
a retained game does not see an unplug (which also made Steam raise its overlay
and force composited capture after every reconnect). Native callbacks hold an
owner-switchable route (`control/input/ownership.rs`), never a peer's channel;
feedback produced while unowned is dropped. The next peer's first input for a
slot claims it, re-enables motion reports and replays the device's current LED
and per-trigger effect state; rumble is never replayed. An arrival with a
different virtual identity (family or Edge subtype), a controller missing from
the client's active mask, or session teardown destroys the device. The slot
mutex serializes Home/Guide timers. Only an active peer's disconnect performs
cleanup; a delayed disconnect from a replaced generation cannot release new
input or revoke the new owner.

RTSP access requires an existing session context established through the
GameStream lifecycle. Do not move launch authentication into the streaming hot
path or treat possession of an RTSP socket as pairing authorization.

Each authenticated `/launch` or `/resume` starts an authorization generation
(`session/authorization.rs`) owned by the session manager: the paired client's
normalized address plus fresh Sunshine session identifiers. RTSP accepts only
that address, and ANNOUNCE/PLAY commit only within the generation that issued
them. Media PINGs must come from that address and, for clients announcing
Moonlight's `ML_FF_SESSION_ID_V1`, echo the generation's ping payload; source
ports stay free for NAT. The control stream admits peers by address and connect
data, but dispatches only the peer that authenticated with the HTTPS-delivered
AES-GCM key, with a per-generation replay window. Without session-ID support,
clients sharing one address are distinguished only by the control key; media
endpoint discovery then relies on address alone.

Accepted HTTP, HTTPS and RTSP connections run in bounded, cancellable tasks
(`ingress.rs`) that hold a global shutdown delay token, with TLS handshake,
request-header and RTSP framing deadlines. Pairing approval is a loopback-only
operator action; first pairing cannot rely on a paired client certificate.
Pairing trust changes serialize under the persistent state owner and publish only
after atomic replacement and sync. Pending transactions have finite capacity and
a deadline, with generation-scoped waiter cleanup. Revocation drains HTTPS
launch/resume/cancel operations and tears down the active session; see
[Security administration](SECURITY_ADMINISTRATION.md) for legacy associations and
recovery policy.

Launch, resume and ANNOUNCE values are validated against shared numeric
domains (`session/negotiation.rs`, `VideoStreamContext::validate`) before the
manager pauses, rekeys or reconfigures anything; a rejected request leaves the
working stream unchanged. The domains are consumer limits (nonzero timing,
representable extents, one-datagram shards including the encryption prefix,
32-bit Vulkan Video rate control, implemented audio durations), not quality caps.

Session keys (`session/keys.rs`) are validated at the HTTPS boundary and
published as material with a server-owned generation; consumers detect key
changes by generation, never by the client's `rikeyid`. AES-GCM nonce counters
for video and host control messages belong to the key bytes through the
manager's process-lifetime key ledger, so packetizer recreation, reconfigure,
reconnect, key-ID-only changes or a later session reusing the key continue the
same counters. A video epoch that negotiated encryption either encrypts every
shard or emits none (nonce exhaustion retires the key instead of wrapping).
Audio uses the protocol's fixed AES-CBC IV (`rikeyid` plus RTP sequence).

A pending ANNOUNCE is not the active encoder configuration. Keep pending and
active contexts separate until PLAY, and preserve epoch barriers so old packets
or captured frames cannot enter the new stream. Changed audio layout/duration
can require capture-server reconfiguration as well as a new encoder. Validate
both directions of mode changes with [reconnect checks](reconnect-validation.md).

## Session ownership and shutdown

The manager's lifecycle is explicit; absence of state never means idle:

| State | Meaning |
| --- | --- |
| `Idle` | No session and no owned resources. The only state that accepts `/launch`. |
| `Live` | A session record (epoch, stop manager, authoritative context, live stream contexts, application unit, start latches) plus either the owned state or one in-flight transition that checked it out. HTTP/RTSP see the context in both cases. |
| `Stopping` | One teardown task owns everything. The session is not reported, its keys, pending contexts and authorization are retired, and a replacement launch waits (bounded) for completion. |

**Transitions** (initialize, launch, PLAY start/resume, active ANNOUNCE pause)
validate every prerequisite under the manager mutex before moving anything,
then run in a manager-owned task with the mutex released. The caller only
awaits the result: an HTTP timeout or dropped RTSP connection does not drop the
work. Each transition holds a completion token of its session and is wrapped
in the session's cancellation, so a stop cancels it at any await and teardown
waits until it has handed back what it checked out. A result commits only if
the session epoch and transition id are still current and the session is not
stopping; otherwise it goes to that epoch's teardown. A failed transition
cannot leave half-applied state: it starts a deterministic full teardown.
Duplicate, premature or stale requests (PLAY without a current-generation
ANNOUNCE, a second PLAY or launch, ANNOUNCE/PLAY during another transition) are
rejected without touching the retained application or streams.

The compositor owns the session's XWayland process. Smithay neither signals nor
reaps it and calloop never frees a loop whose sources hold loop handles, so the
compositor identifies the child it forked, holds a pidfd for it, and at teardown
closes its Wayland client, waits (2 s, then `SIGKILL` via the pidfd, 1 s), lets
the WM's X11 source observe the closed connection, and releases the display lock
and sockets before its worker guard drops. A process that survives is reported
and keeps the session `Stopping`, so the teardown deadline fails terminally
instead of reporting `Idle`. A retained-session reconnect does not touch it.

**Workers** (compositor, video pipeline thread and packet task, audio encoder
and packet task, PulseAudio server, control stream, gamepad thread) register a
`lifecycle::WorkerGuard` *before* they are spawned and drop it last, after
their sockets, threads, GPU objects and frames. The session stop manager's
completion therefore means every worker exited and released what it owned; a
failed spawn drops the guard and stops the session. Stream workers wait for
`StartB` through a persistent `lifecycle::StartLatch`: it may open before,
during or after workers wait, duplicate opens are no-ops, and a stop before
`StartB` cancels the wait and releases the worker's socket.

**Teardown** has a single owner per session, started by user cancel, a worker or
application exit (via a per-session watchdog that only hands over, so nothing
aborts the cleanup), a failed transition, or service shutdown. In order, it:
stops the application unit while the compositor and audio still serve it
(unless a transition was in flight, which is cancelled first); triggers the
session stop; drops the owned state; waits for every worker and transition;
drops state handed back by transitions; stops the unit if not already done;
then reports `Idle`. The unit is recorded before a launch starts, so a launch
cancelled after systemd accepted the unit is still stopped. `Application` drop
never blocks; stopping the unit is an awaited, bounded D-Bus job.

**Deadlines.** Application stop is bounded to 6 s and worker exit to the rest
of a 16 s end-to-end teardown deadline (`SESSION_TEARDOWN_DEADLINE`). An
unsuccessful unit stop is logged and does not wedge the manager (the next
launch also replaces a leftover unit). Exceeding the deadline is terminal: the
session stays `Stopping`, new sessions are refused, and the service shuts down
for its supervisor to restart it. Service shutdown (SIGTERM/SIGINT) holds its
completion until the session teardown, application included, has finished or
failed within that same deadline.

Tests: `session/manager/lifecycle_tests.rs` drives the manager through a fake
backend with barriers and fault injection at every transition await; stream
workers have socket-release tests in their modules.

## Capture and native resource ownership

Two independent signals matter:

- **Capture admission** says whether the pipeline can accept another scene.
- **Buffer consumption** says whether the GPU has finished reading its source.

Releasing a source buffer must not create a second network admission credit.
Conversely, paced sending must not retain a source buffer after GPU consumption.
A third property, **descriptor ownership**, is independent of both: every
`ExportedFrame` holds a `SourceLease` (a strong Smithay `Dmabuf` reference) to
its pool slot or client buffer, and hands out plane fds only borrowed from
itself. A frame that is queued, being imported or being read therefore keeps
its descriptors open and unrecycled even after the compositor retires a pool or
exits; imports take their own references (duplicated fds, imported memory)
before the frame is dropped. `consumed` is set only when no GPU work can still
read the source.
Epoch invalidation must reject old completions without replenishing new demand.
See [capture pipeline](PIPELINE_OPTIMIZATION.md) for the precise handoff contract.

Direct export is allowed only when the source represents the entire visible
scene. Composition adds a GLES pass and completion wait before DMA-BUF export;
that wait currently protects ownership. Preserve cursor/overlay visibility and
input focus independently; [COMPOSITOR.md](COMPOSITOR.md) explains their interaction.

DMA-BUF caches validate open-file identity and the complete image layout, not
just a numeric fd or buffer index. At the PyroWave C API boundary, duplicated fds
transfer ownership to the native importer, and child handles must die before
parent device/library resources. Conventional Vulkan import also needs cleanup
for partial allocation failures. These assumptions are part of correctness,
including shutdown and output-mode changes.

## Transport and failure boundaries

Video packetization preserves Moonlight framing, sequencing and representable
FEC metadata. FEC covers plaintext shards; optional encryption follows per shard.
PyroWave pacing uses capture time and rebases after backpressure instead of
sending catch-up bursts. Raw UDP `WouldBlock` retries must participate in Tokio
readiness clearing. See [PyroWave transport](PYROWAVE.md#transport-pacing-and-diagnostics)
and [runtime diagnostics](LONG_SESSION_PERFORMANCE.md).

Native/GPU failures must preserve cleanup and stop unsafe reuse; retrying a lost
Vulkan device indefinitely cannot restore that device. Both encoder backends
report frame failures through `pipeline/failure.rs`: the failed stage, a
recovery (`DropFrame`, `RequestIdr` when the encoder may have referenced a frame
the client never receives, or `Terminal`) and whether the source's GPU reads are
not submitted, completed or unknown. Only the first two release the source for
reuse; a conventional conversion failure first waits for the device to idle to
establish completion. Device/API loss is terminal; a recoverable failure that
repeats for 300 frames and 5 s without a success is escalated. A terminal
failure ends the pipeline, which stops the session. Diagnostics report the first
failure and at most one summary per 5 s. A successful compile,
loader probe or loopback benchmark does not prove presentation, client decode,
Steam behavior or physical-link performance.

## Where to go next

- [Compositor](COMPOSITOR.md): scene eligibility, cursor intent, Steam focus/input.
- [Capture pipeline](PIPELINE_OPTIMIZATION.md): admission, completion and bounded work.
- [PyroWave](PYROWAVE.md): dependency maintenance, wire formats, color, FEC and GPU matrix.
- [Vulkan WSI](VULKAN_IMAGE_COUNTS.md): dispatch/extension gates and image-count negotiation.
- [DualSense Edge](DUALSENSE_EDGE.md): native report mapping and hardware acceptance.
- [Benchmarking](BENCHMARKING.md), [diagnostics](LONG_SESSION_PERFORMANCE.md) and
  [reconnect validation](reconnect-validation.md): measurement and acceptance.

Current guides describe implementation contracts. [Historical reports](reports/README.md)
preserve dated evidence and rejected experiments; they are not current validation
claims. Keep guides synchronized when those contracts change, and record new
measurements with revision, hardware, workload and unperformed checks.
