# Vulkan image counts and XWayland bypass

The XWayland bypass replaces XCB/Xlib surfaces with Wayland surfaces. Mesa can
report a conservative legacy `minImageCount=4` while a FIFO mode query permits
three images. Path of Exile's native Vulkan renderer checks the legacy range
before creating its swapchain and can abort with `unsupported backbuffer image
count`. Disabling bypass selects the original XCB WSI and avoids those Wayland
limits. The previous `max(driver_minimum, 3)` policy could not reduce four and
also unnecessarily excluded ordinary two-image configurations.

The [GE-Proton reference](https://github.com/GloriousEggroll/proton-ge-custom/commit/d8522c3c2faa119f3adcfc2482c1a81d42c6c63e)
uses mode-specific limits and declares the host swapchain mode to avoid Mesa's
legacy allocation policy. Pyroshine applies that mechanism to bypass surfaces
without executable-name or engine-name matching.

## Negotiation

- Instance creation uses the loader's global extension enumeration before privately enabling
  `VK_KHR_get_surface_capabilities2`, `VK_EXT_surface_maintenance1`, and
  `VK_KHR_get_physical_device_properties2` (or using Vulkan 1.1 features queries).
  WSI platform extensions are also enabled only when available. None of these
  extensions is newly advertised by the layer. WSI instance entry points remain
  gated by the application's enabled extensions. The properties2/features2 KHR
  aliases follow the actual downstream enabled extensions: the Vulkan loader
  needs these aliases for core-query dispatch after private enablement.
  Global enumeration does not use the next instance layer's dispatch function
  with a null instance; that can crash layers such as Mesa device-select.
- Device creation checks downstream `VK_EXT_swapchain_maintenance1` support and
  queries `VkPhysicalDeviceSwapchainMaintenance1FeaturesEXT`. It enables that
  feature only when supported and `VK_KHR_swapchain` is enabled. An existing
  application feature struct is preserved, including an explicit false value.
  Failed extension/feature injection can retry the original create-info; other
  device errors propagate without a retry.
- Legacy capabilities and Capabilities2 without a present-mode input share the
  same adjustment. Only a bypass surface, with successfully enabled maintenance
  on every existing device for the queried physical device, can be relaxed. A
  legacy minimum above three becomes three only if a FIFO mode query accepts
  three (`min <= 3`, `max == 0 || max >= 3`). The legacy maximum is preserved:
  this is not a promise of exactly three allocated images. For a legacy minimum
  above four, FIFO must cover every newly admitted count up to that minimum;
  otherwise the range stays conservative.
- Queries before device creation, feature opt-outs, unavailable extensions,
  failed queries, native Wayland surfaces, and dynamically selected XCB fallback
  surfaces retain the driver's limits. An ordinary two-image surface stays two.
- Capabilities2 with `VkSurfacePresentModeEXT` input retains that mode's actual
  range, including a legitimate mailbox minimum of four. Other output pNext
  structures remain the application's and are filled by the ICD.
- Creation queries the requested mode using `VkPhysicalDeviceSurfaceInfo2KHR`,
  `VkSurfacePresentModeEXT`, and `VkSurfaceCapabilities2KHR`. If the requested
  minimum fits, the requested mode is retained. FIFO compatibility fallback is
  allowed only for a bypassed count newly admitted below the legacy minimum
  (normally three below four),
  when the requested mode's successful query excludes three and FIFO accepts
  it. An application-provided `VkSwapchainPresentModesCreateInfoEXT` is never
  replaced or duplicated; its explicit mode requirements remain authoritative.
- The ICD receives `VkSwapchainPresentModesCreateInfoEXT` with the effective
  mode. Dynamic FIFO forcing declares FIFO too, but only when
  `VkSurfacePresentModeCompatibilityEXT` reports compatibility and both ranges
  accept the application's count. Otherwise the existing limiter can select
  FIFO at creation, and limiter changes request recreation. Presentation only
  switches among declared modes, preserves application mode chains, and tracks
  the last mode submitted to the ICD for mixed batches. The compositor receives
  the same effective mode.

These requirements follow the [Vulkan WSI specification](https://github.khronos.org/Vulkan-Site/spec/latest/chapters/VK_KHR_surface/wsi.html)
and [swapchain maintenance proposal](https://github.khronos.org/Vulkan-Site/features/latest/features/proposals/VK_EXT_swapchain_maintenance1.html).
The EXT maintenance path requires driver support; KHR-only implementations keep
their original image-count behavior.

`MOONSHINE_WSI_MIN_IMAGE_COUNT` is deprecated and ignored with a warning.
Advertised requirements now follow the driver rather than a buffering preference.
No downstream code depended on this setting; the requested swapchain minimum is
not increased. Actual image counts are obtained with `vkGetSwapchainImagesKHR`
and used for diagnostics and swapchain feedback. No images are hidden and no
acquired image indices are remapped.

## Build and automated checks

From the repository root:

```sh
cargo fmt --all -- --check
cargo build --locked --release -p moonshine-wsi
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

The WSI suite includes the seven image-count cases, creation-mode consistency,
compatible FIFO switching, limiter fallback, and mocked ICD tests checking
extension/feature gates and the synchronous lifetime and structure types of
capability query chains. Workspace loopback socket tests require permission to
bind UDP sockets.

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

## Path of Exile acceptance

1. Install the rebuilt `target/release/libmoonshine_wsi.so` using the existing
   package/portable workflow or the [manual installation guide](../CONTRIBUTING.md#manual-installation-and-upgrade).
   Restart the game process to load it; a running process retains its old layer.
   Restart/reconnect the streaming session if installing the full server build.
2. Start Steam through an active Pyroshine stream. Select Vulkan in PoE and
   enable triple buffering. Remove `MOONSHINE_WSI_DISABLE_BYPASS=1` from launch
   options. For the acceptance run use exactly:

   ```text
   %command%
   ```

3. For a separate diagnostic run use:

   ```sh
   MOONSHINE_WSI_LOG=debug MOONSHINE_WSI_LOG_FILE=/tmp/poe-wsi.log %command%
   ```

   Look for `XWayland bypass active`, `vkCreateDevice (maintenance1=true)`,
   `per-mode capabilities`, and `surface capabilities`. On the affected driver,
   expect `driver minImageCount=4`, `advertised minImageCount=3`, and
   `compatibility_applied=true`. `mode_specific=true` queries can legitimately
   report different mode-specific limits. `swapchain negotiation` records
   `requested minImageCount=3`, requested/effective mode values, and declared
   modes. `swapchain allocation` distinguishes requested minimum from actual
   allocated count. Vulkan numeric modes: immediate=0, mailbox=1, FIFO=2.
4. Confirm PoE reaches gameplay, the stream remains functional, and
   `override_window_content` appears for its bypassed swapchain. A log saying
   only that surface bypass was created does not prove swapchain bypass stayed
   safe/active.
5. Test another Vulkan game; Steam overlay opening/closing; cursor composition;
   SDR, HDR10, and scRGB sessions; direct/composited transitions; limiter/FIFO
   toggles; present timing; and reconnect. Check native Wayland if it is enabled.
6. Repeat with the supported fallback:

   ```sh
   MOONSHINE_WSI_DISABLE_BYPASS=1 %command%
   ```

The release build and automated tests do not establish the PoE acceptance result.
A host/stream/game run must confirm it. Applications that reject conservative
capabilities before creating any Vulkan device remain a limitation of the safe
feature-enable gate. Explicit application mode lists are also not overridden to
force a three-image configuration.
