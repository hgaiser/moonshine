# GPU selection and WSI failure remediation — 2026-10-02

Addresses CFG-002, BUG-004 and BUG-005 from the production readiness review.

## Device path and dependency decision

Inspected the Cargo-locked Pixelforge v0.9.1 source (`2ee4d7a`,
`src/vulkan.rs`): the builder exposes application metadata, validation and
required codecs, but no device or DRM-node selector. Its suitability loop chooses
an independently enumerated Vulkan device. No nonexistent selector was added and
no dependency pin, patch set, build helper or Nix dependency source was changed.

The existing automatic DRM-node preference, configured `compositor.gpu` matching,
and `MOONSHINE_RENDER_NODE` precedence remain in `find_render_node`. Healthcheck
no longer substitutes the first discovered GPU after a selection failure. EGL and
Vulkan/profile probing require that selected accessible capture path.

`gpu.rs` verifies the actual opened capture file's device number against Vulkan
DRM render properties. Drivers without those properties must provide a complete
PCI domain/bus/device/function matching `/sys/dev/char/<major>:<minor>/device`.
Names, vendor/product IDs, and enumeration indexes are never accepted as proof.
Unknown or mismatched identities fail with both identities and instructions to
align capture configuration or Vulkan loader/ICD selection. Capability probing,
including the `--no-health-check` path, then advertises no codec/HDR/DMA-BUF support.

The compositor verifies before publishing readiness. The capture channel retains
the verified `VideoContext`; conventional encode, Vulkan DMA-BUF import and
PyroWave UUID adapter matching all use clones of it, including reconnects and
changed stream profiles. No per-frame identity queries or new locks were added.
The context remains owned by session capture/encoding resources, not a global cache.

Cross-device capture-to-encoder operation is explicitly **unvalidated and
rejected**. Previously accidental cross-GPU operation is not a supported transfer
contract: import success alone cannot characterize every modifier or transfer
cost. An explicit capture override does not reorder Pixelforge's Vulkan devices.
This verification approach intentionally fails mismatches instead of inventing a
selector API or modifying the pinned dependency. Supporting those combinations
later requires a maintained selector/interop change and multi-GPU validation.
Client DMA-BUF ingress/direct export still uses existing per-buffer import checks;
this is not a claim that all buffers from every application GPU are compatible.

## WSI contracts

Presentation aggregates preserve a real negative ICD result. If an otherwise
successful/suboptimal aggregate has a negative per-swapchain result, that failure
also wins. Synthetic hints replace only SUCCESS/SUBOPTIMAL entries, process the
entire batch, and choose OUT_OF_DATE over a synthetic SUBOPTIMAL. Dynamic present
mode dispatch, explicit application chains and compositor mode reporting retain
their existing gates; bypass retirement remains independent of error reporting.

Temporary bypass surfaces have a scoped protocol-destructor guard. Construction
failure, missing dispatch/instance and unwinding before success destroy once and
flush. A successful ICD constructor transfers ownership to the existing live
surface record; ordinary teardown destroys Vulkan before the Wayland proxy.
XCB fallback remains intact. The constructor helper used by production is also
used by the protocol-accounting tests.

## Automated results

Passed:

- `cargo test --workspace --all-features --offline`: 392 core + 51 WSI tests,
  including all existing dispatch/image-count tests. Executed with local socket
  access; restricted sandbox execution blocks socket-based tests.
- `cargo clippy --workspace --all-features --all-targets --offline -- -D warnings`.
- `cargo fmt --all -- --check`.
- `RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --workspace --all-features --offline`.
- Changelog consistency check, eight changelog tooling tests, `git diff --check`.

Mock ICD coverage exercises the real queue presentation interception with
SUCCESS, SUBOPTIMAL, DEVICE_LOST, OUT_OF_HOST_MEMORY and OUT_OF_DEVICE_MEMORY;
both FIFO toggle directions; null and mixed per-swapchain results; and dynamic
FIFO mode changes without unnecessary recreation. A separate policy test combines
limiter and bypass hints with driver failures.

An in-process Wayland server counts actual create/destroy protocol requests.
Twenty retry rounds across seven rollback stages (140 failures) leave zero live
surfaces after each attempt, including failures before/after flush, after queue
migration, missing instance/dispatch and constructor errors. Twenty successful
transfers preserve one live surface until explicit owner teardown, then return to
zero. These are mocked constructor/real protocol tests, not real ICD WSI tests.

Identity tests cover opposite DRM enumeration, conflicting PCI hints,
PCI fallback, unknown identity, full PCI address parsing and regular-file
rejection. A healthcheck regression ensures an unavailable capture path cannot
advertise codecs, HDR or DMA-BUF support.

## Hardware probes

Available outside the device-restricted sandbox: one AMD Radeon RX 9070 XT,
RADV GFX1201, driver 26.2.4, Vulkan 1.4.354, `/dev/dri/renderD128`, DRM `226:128`,
PCI `0000:03:00.0`. The ICD prints its nonconformance/testing warning; these
results apply to this installed driver only.

Passed standalone healthcheck with complete temporary configs using ephemeral
ports (the existing service already owns its normal ports):

- automatic GPU selection;
- explicit `/dev/dri/renderD128`;
- `MOONSHINE_RENDER_NODE=/dev/dri/renderD128` overriding an invalid configured node.

An invalid configured `/dev/dri/renderD999` without the override fails GPU,
Vulkan, codec and DMA-BUF checks instead of probing another GPU. The automatic
probe logs the matching DRM/PCI/UUID identity. Successful profile creation:
H.264 4:2:0 8-bit; HEVC/AV1 4:2:0 8/10-bit; PyroWave 4:2:0 and 4:4:4 8/10-bit.
EGL HDR capability passed. Conventional 4:4:4 profiles were not advertised by
this driver. Profile creation does not establish a live encoded-stream result.

Passed with actual opt-in execution:

```sh
MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core --offline \
  gpu_import_after_source_owner_teardown_and_fd_reuse -- --nocapture
MOONSHINE_TEST_PYROWAVE=1 \
  MOONSHINE_PYROWAVE_LIBRARY=/usr/lib/libpyrowave-shared.so.0 \
  cargo test -p moonshine-core --offline ffi_loads_pinned_api -- --nocapture
```

The GPU import test now verifies and propagates the capture context, checks
physical/logical handle identity, and performs real GBM DMA-BUF imports after
source-owner teardown and fd reuse. The installed library passes the expected
PyroWave ABI check; a clean pinned dependency build was not performed.

## Remaining hardware/client matrix

Not performed: two GPUs with reversed enumeration; supported cross-device
transfer/cost measurement (no such mode enabled); live H.264/HEVC/AV1/PyroWave
streaming; HDR/4:4:4 rendering at the client; native Wayland/XWayland real-ICD
presentation; live limiter changes and bypass transitions; disconnect/reconnect
with changed and unchanged modes. Those require the documented client/reconnect
matrix and a multi-GPU test host. Automated checks and encoder creation probes
must not be treated as those live acceptance results.
