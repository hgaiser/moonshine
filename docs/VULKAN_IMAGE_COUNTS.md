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

Implementation is in `moonshine-wsi/src/{instance,device,surface,swapchain,image_count}.rs`;
compositor feedback is in `moonshine-core/src/session/compositor/gamescope_swapchain.rs`.
See [architecture](ARCHITECTURE.md) for presentation/capture ownership.

## Negotiation

### Instance and device gates

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

### Capability queries

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

### Creation and presentation

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

## Presentation topology and fallback

Bypass safety is evaluated before any positive top-level shortcut. In particular,
`_WINE_ALLOW_FLIP=0` always rejects bypass, even when the Vulkan XID is itself the
toplevel. An unnamed 1×1 override-redirect parent also rejects bypass, independently
of the flip property. Wine can render offscreen and GDI-blit onto a different
visible window; replacing that presentation surface can leave the stream black.
The rule uses presentation topology, never game/executable names. Ordinary safe
fullscreen windows retain bypass and automatic direct DMA-BUF export eligibility.

Size tolerance (2 pixels), position tolerance (1 pixel), and obscuring child
checks remain enforced. Child offsets accumulate in top-level coordinates;
XCB geometry is parent-relative. The rendering branch itself is excluded from
obstructions, while other visible children along the ancestry (and children of
the rendering window itself) are checked. Obscuring checks also apply to top-level windows. XCB
query-tree replies use the protocol's 32-byte header and 16-bit child count.
Missing topology or unavailable event monitoring conservatively selects XCB.

Each managed replacement retains a real XCB fallback surface. Capability,
format, and present-mode queries and swapchain creation select the same policy
path; fallback capabilities are not relaxed using Wayland image-count rules.
Fallback swapchains do not bind a compositor swapchain or call
`override_window_content`. The ICD's HDR color-space handling and existing
XWayland limitations still apply on fallback; this does not add HDR to Wine GDI.

A private XCB connection subscribes to structure/substructure and relevant
property events on the window ancestry and children. It never changes the
application's event mask or consumes its events. Presentation dispatches queued
events without blocking; policy queries occur at setup or after relevant changes,
not on unchanged frames. No timer polls, CPU readback, GPU copy, forced composition,
or extra frame queue is introduced. If `DISPLAY` cannot provide a usable event
connection for the application XID, bypass is conservatively rejected.

The ICD surface chosen at swapchain creation is immutable. An unsafe transition
removes the old protocol override and reports `VK_ERROR_OUT_OF_DATE_KHR` until
recreation, even if safety recovers in between; image acquisition also rejects
that retired chain. A now-safe XCB chain reports `VK_SUBOPTIMAL_KHR`. Recreating on another
ICD surface omits `oldSwapchain` from the downstream create-info; the application
still owns and destroys its old handle. Destroying an older protocol object
cannot clear a newer object's override on the same Wayland surface. ICD failures
are preserved. A stable policy produces no recreation requests.

DEBUG diagnostics log policy changes with reasons: `_WINE_ALLOW_FLIP=0`,
`Wine offscreen presentation parent`, `geometry size mismatch`,
`geometry position mismatch`, `obscuring child`, or
`X11 topology/event monitoring unavailable`. Surface creation's
`XWayland bypass active` means a replacement was allocated; verify swapchain
`bypass=true/false` and `override_window_content` to identify actual presentation.

## Proton fullscreen acceptance (Dave the Diver test case)

Install the rebuilt layer and restart the game process. Compare fullscreen and
borderless at the same stream settings. Use Steam launch options:

```sh
MOONSHINE_WSI_LOG=debug \
MOONSHINE_WSI_LOG_FILE=/tmp/dave-wsi.log \
%command%
```

Inspect the log:

```sh
rg 'XWayland bypass|vkCreateSwapchainKHR|override_window_content|swapchain negotiation|surface capabilities' /tmp/dave-wsi.log
```

An unsafe topology should show its rejection reason, `bypass=false`, plain XCB
capabilities/creation, and no override for that fallback chain. A safe fullscreen
window should retain `bypass=true` and its override. Switch between fullscreen,
borderless, and windowed repeatedly; verify recreation settles, video and input
continue, and no stale replacement covers the real window.

Run a separate A/B diagnostic with:

```text
MOONSHINE_WSI_DISABLE_BYPASS=1 %command%
```

Fullscreen broken normally but working with bypass disabled strongly implicates
WSI substitution. Also run with this server configuration:

```toml
[compositor]
capture_mode = "composited"
```

Composited still black while bypass-disabled works points to WSI/window
substitution. Composited working while automatic capture fails points instead to
direct-export eligibility; inspect compositor DEBUG capture-path transitions.
Restore `auto` after diagnosis. This diagnostic is not a permanent global fix.

Repeat with Steam overlay/notifications, controller focus and Steam Input,
DualSense/Edge and Nintendo emulation, active-mask hotplug and reconnect (indices
0 and 15), SDR/HDR, YUV 4:4:4, H.264/HEVC/AV1/PyroWave, and Path of Exile triple
buffering. These require an actual GPU, streaming client, and game installation;
unit tests do not establish game acceptance or performance.

## Build and automated checks

From the repository root:

```sh
cargo fmt --all -- --check
cargo build --locked --release -p moonshine-wsi
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

The WSI suite covers image-count policy cases, creation-mode consistency,
compatible FIFO switching, limiter fallback, and mocked ICD tests checking
extension/feature gates and the synchronous lifetime and structure types of
capability query chains. Workspace loopback socket tests require permission to
bind UDP sockets.

## Recorded validation

Dated results are preserved in the [historical report](reports/VULKAN_IMAGE_COUNTS.md).
They are evidence for those revisions, not a substitute for current acceptance.

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

## Presentation failures and temporary surface ownership

Limiter and bypass recreation hints replace only ICD `SUCCESS` or `SUBOPTIMAL`.
Negative aggregate results remain authoritative, and negative per-swapchain
results are never overwritten. All eligible swapchains in a batch receive their
hints; a mixed batch retains its driver failure even when other entries need
recreation. `OUT_OF_DATE` takes precedence over synthetic `SUBOPTIMAL`.

Bypass construction owns each temporary `wl_surface` with a rollback guard. Any
failure, including missing instance/dispatch or an ICD constructor error, sends
one protocol destructor and flushes. A successful Vulkan constructor transfers
the proxy to live surface ownership; ordinary teardown destroys the Vulkan
surface before destroying the protocol surface. XCB fallback remains available.
The mock ICD and in-process Wayland server tests run without GPU hardware.
