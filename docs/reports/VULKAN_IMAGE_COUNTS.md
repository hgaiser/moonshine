# Historical WSI validation

Recorded during September 2026 development. These results describe the tested
revision and environment; they do not establish current hardware acceptance.
See the [current guide](../VULKAN_IMAGE_COUNTS.md).

## 0.16.10 dispatch regression verification

The 0.16.9 extension gate withheld properties2/features2 KHR pointers when the
layer privately enabled their extension. The loader could then route a core
query to a null pointer, crashing Steam's GPU process and forcing software
compositing in Big Picture. These aliases now follow downstream enablement.
Instance extension discovery also uses the loader's global enumeration entry
point, avoiding a null-instance dispatch lookup in Mesa device-select.

Regression tests call both formerly missing aliases, cover active and degraded
instances with and without application enablement, and verify that private
surface-capabilities enablement still leaves its application entry point gated.
The private-properties test fails with the original 0.16.9 gate.

On the RX 9070 XT with RADV/Mesa 26.2.3, an isolated Vulkan 1.0 instance with no
application extensions reproduced a SIGSEGV with 0.16.9. The 0.16.10 release
library completed both core queries with Mesa device-select enabled and disabled.
This probe enabled extension injection but used an unavailable compositor socket;
it validates loader initialization and dispatch, not frame presentation.

All 237 workspace Rust tests, eight changelog tests, formatting, all-targets
Clippy, documentation with warnings denied, and the workspace release build
passed. The candidate server's host healthcheck passed with ephemeral ports in
a temporary config, including H.264, HEVC, AV1, PyroWave SDR/HDR profiles and
DMA-BUF support. The installed PyroWave C API load test also passed.
An end-to-end Big Picture FPS comparison remains unperformed; the installed
service and Steam session were not replaced during validation.

## Validation recorded on 2026-09-30

- Release WSI build, formatting, workspace clippy (all targets/features,
  warnings denied), and workspace documentation (warnings denied) passed.
- All 230 Rust unit tests passed: 195 core tests and 35 WSI tests. The first
  sandboxed run denied UDP binds for four core tests; the full rerun with
  loopback sockets permitted passed. Eight changelog tests and release-note
  consistency checks also passed.
- A separate non-presenting Wayland probe on RADV/Mesa 26.2.3 (RX 9070 XT)
  measured desktop legacy minimum 3, mailbox minimum 4, and FIFO/immediate
  minimum 3, all with unbounded maxima. Explicit three-image FIFO and immediate
  swapchains each allocated three images. The same probe passed through the
  release layer in degraded mode using a temporary manifest, without installing
  it or changing Steam.
- The recorded Steam/Pyroshine `wayland-1` socket was absent. The affected active
  bypass surface and PoE were therefore not exercised. Desktop probe values do
  not establish the affected surface's legacy minimum or successful game launch.
  Overlay, HDR/SDR, direct presentation, cursor, limiter, and reconnect acceptance
  checks below remain pending.
