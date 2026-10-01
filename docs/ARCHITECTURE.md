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

A missing/incompatible optional PyroWave library leaves conventional codecs
available. A negotiated PyroWave stream does not silently switch to another
codec or software encoder. See [PyroWave](PYROWAVE.md#capability-and-negotiation-extension).

## Session lifecycle and negotiation

`session/manager.rs` owns the single optional session; `session/mod.rs` represents
its `Initialized`, `Launched` and `Active` states. Application/compositor lifetime
and a client's stream epoch are different: a reconnect can retain the application
while resetting or replacing the encoders and transport state.

| Step | Owner and contract |
| --- | --- |
| HTTP launch | Authenticated GameStream API initializes and launches the application/compositor |
| RTSP ANNOUNCE | Validates negotiated formats and stores pending video/audio contexts |
| RTSP PLAY | Constructs initial streams, or commits a reconnect transition |
| Control `StartB` | Opens the audio/video start gates; tools can trigger them through manager notifications |
| HTTP resume | Updates session keys and retains requested session parameters; RTSP remains authoritative for encoded stream properties |
| Unchanged reconnect | Pauses delivery, resets client-visible video sequencing, requests an independently decodable first frame and acknowledges ordered transport activation before PLAY completes |
| Changed reconnect | Pauses affected epochs, updates compositor output when needed, and recreates affected video/audio resources |
| Cancel, application exit or session failure | Session shutdown releases application, stream tasks and native resources; the manager can accept a later launch |

RTSP access requires an existing session context established through the
GameStream lifecycle. Do not move launch authentication into the streaming hot
path or treat possession of an RTSP socket as pairing authorization.

A pending ANNOUNCE is not the active encoder configuration. Keep pending and
active contexts separate until PLAY, and preserve epoch barriers so old packets
or captured frames cannot enter the new stream. Changed audio layout/duration
can require capture-server reconfiguration as well as a new encoder. Validate
both directions of mode changes with [reconnect checks](reconnect-validation.md).

## Capture and native resource ownership

Two independent signals matter:

- **Capture admission** says whether the pipeline can accept another scene.
- **Buffer consumption** says whether the GPU has finished reading its source.

Releasing a source buffer must not create a second network admission credit.
Conversely, paced sending must not retain a source buffer after GPU consumption.
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
Vulkan device indefinitely cannot restore that device. A successful compile,
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
