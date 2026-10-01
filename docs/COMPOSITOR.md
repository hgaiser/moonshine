# Scene capture, cursor lifetime, and Steam input

Pyroshine captures the visible compositor scene independently of the selected
keyboard, pointer, and Steam controller input targets. Virtual controller
emulation selects the device family; Steam Input routing and compositor focus
determine how applications receive that input.

## Ownership and frame handoff

The embedded Smithay compositor owns the visible scene and input focus, not
codec negotiation or packet transport. `frame.rs` carries the exported DMA-BUF,
color/HDR metadata and consumption signal to the video pipeline. Capture demand
is separately generation-tagged; see [capture admission](PIPELINE_OPTIMIZATION.md#capture-admission-and-presentation).
Service buffer releases and input even when capture is blocked or the scene is
clean. [Architecture](ARCHITECTURE.md) explains session and stream ownership.

## Configuration

Both additions are optional and preserve automatic defaults:

```toml
[compositor]
capture_mode = "auto" # auto | composited

[stream.control.gamepad]
emulation = "auto" # auto | xbox | playstation | nintendo
```

`auto` capture directly exports a fullscreen DMA-BUF only when it represents the
whole visible output. `composited` uses the existing GLES output DMA-BUF pool for
compatibility diagnosis. There is no forced direct mode.

Automatic gamepad emulation preserves Xbox, PlayStation/PS5, and Nintendo/Switch
families. Steam/Valve (`LI_CTYPE_STEAM = 0x04`), unknown, and future kinds use the
Xbox compatibility target. Forced policies choose the requested virtual family.
The existing motion, touch, battery, feedback, hotplug, and active-mask paths
remain in place; advanced features depend on the virtual family selected.

## Cursor source of truth

There is no inactivity timer. An active visible cursor remains visible until
client cursor state changes. Mouse/pen use can activate the initial fallback;
controller events do not activate it and cannot change application cursor intent.
Pointer activity cannot undo an explicit `Hidden` request.

The pinned Smithay revision is
[`0ff00983b6007257a7a161a4fe8b14a778e2ac8f`](https://github.com/Smithay/smithay/tree/0ff00983b6007257a7a161a4fe8b14a778e2ac8f).
Its [`wl_pointer.set_cursor` handler](https://github.com/Smithay/smithay/blob/0ff00983b6007257a7a161a4fe8b14a778e2ac8f/src/wayland/seat/pointer.rs#L421)
reports a custom `Surface` or `Hidden` for an accepted client request. Its
[pointer replacement](https://github.com/Smithay/smithay/blob/0ff00983b6007257a7a161a4fe8b14a778e2ac8f/src/input/pointer/mod.rs#L141)
and [leave handling](https://github.com/Smithay/smithay/blob/0ff00983b6007257a7a161a4fe8b14a778e2ac8f/src/input/pointer/mod.rs#L823)
also report `default_named()` as a framework fallback. Pyroshine does not
advertise `wp_cursor_shape_manager_v1`; a default named callback is therefore
not sufficient evidence to activate an untouched startup cursor. Other images
activate cursor state, while fallback resets retain existing activation.

A destroyed custom cursor falls back to the default image only for an already
active cursor. Destroying an older, replaced surface cannot replace the current
image. Image/hotspot requests, cursor commits, movement, and destruction mark the
scene dirty. Existing output damage tracking and static-screen keepalives remain.
If cursor-shape protocol support is added later, explicit named requests must be
distinguished from Smithay's fallback callbacks.

## Scene and input decisions

`can_direct_scanout_scene()` consumes classified special-window state and cursor
visibility, then checks the actual top visible source. Cursor, Steam overlays,
notifications, external overlays, dropdowns, decorations, scaling, popup trees,
and independently visible subsurfaces require composition. An opaque fullscreen
source can cover mapped background windows without making them independently
visible. Buffer dimensions, lifetime, crop, scale, rotation, position, and the
existing DMA-BUF checks still apply. Eligibility performs no X11 queries and
allocates no window list. Current color management passes through pixel data and
metadata; no new color conversion or SDR intermediate is introduced.

### WSI and transparency

Both swapchain protocols convey Vulkan composite-alpha intent. The
compositor now honors `VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR` for the declared root
image in direct eligibility and GLES blending, including alpha-capable HDR
buffers. Child surfaces retain their own transparency. See the
[Vulkan definition](https://github.khronos.org/Vulkan-Site/spec/latest/chapters/VK_KHR_surface/wsi.html).

### Steam classification and input

Steam classification has one rule: `STEAM_OVERLAY != 0` is interactive when it
spans the virtual output width or requests `STEAM_INPUT_FOCUS`; otherwise it is a
passive notification. Property and geometry events update this classification.
There is no second fixed-pixel-width detector. Classified layers paint above the
game, its decorations, and dropdowns with their cached window opacity.

Passive notifications are excluded from primary focus, dropdown selection, and
pointer hit testing. Mapping, updating, or unmapping one changes the video scene
but does not reapply unchanged input targets. Compositor-owned root properties
are written only when their desired values change, including focusable lists.
An interactive overlay routes pointer input before WSI routing. Mode 2 keeps the
keyboard on the game; closing the overlay restores the game target and unified
focused-app contract. These match the inspected
[gamescope focus logic](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp).

DEBUG logs report classification, cursor, virtual-device creation, and capture
path transitions (`direct`, `direct_override`, `composited`); rendering itself
does not emit per-frame INFO messages.

## Color and output changes

Direct and composited paths must preserve the source format and color metadata.
`sRGB`, BT.2020/PQ and scRGB linear frames are distinct: scRGB requires gamut/PQ
conversion in the encoder, while PQ input is already transfer-encoded. Do not
replace them with a generic HDR boolean or insert an SDR intermediate.
Output-mode changes retire pools until their buffers are consumed; coordinate
resolution/refresh/HDR changes with the session epoch rather than resizing only
the scene. See [PyroWave ownership](PYROWAVE.md#gpu-path-and-ownership) and
[reconnect validation](reconnect-validation.md).

## Validation and runtime checks

Unit tests cover cursor activation/idle/hide, real Wayland resource replacement
and destruction, scene extras and automatic restoration, capture configuration,
cropping/scaling/rotation, Steam classification transitions, notification focus
and dropdown exclusion, unchanged focus-contract suppression, and controller
kind/policy parsing. They do not prove Steam or game behavior on hardware.

On a GPU host, use DEBUG logging to inspect a fullscreen workload.
Confirm `direct` or `direct_override` with an explicitly
hidden cursor and no extra content. Check GPU utilization and frame latency.
Expose the cursor, leave the mouse idle for more than three seconds, and navigate
Grim Dawn using only a controller. Confirm visible cursor composition persists,
then explicit application hide immediately restores direct eligibility.

Hold controller input while a notification appears, updates, and disappears.
Confirm the notification is captured and game input remains uninterrupted, with
no focus-contract rewrites. Open/close Steam overlay and small interactive menus;
confirm visibility, intended pointer/controller routing, mode-2 keyboard split,
and immediate restoration. Repeat with Steam Input enabled and disabled to
separate Steam routing from virtual-device delivery. Test arrival, duplicate
arrival, update before arrival, active-mask removal, and reconnect at indices
0 and 15, plus native PS motion/touch/rumble and forced policies.

Repeat with H.264, HEVC, AV1, PyroWave, HDR, YUV 4:4:4, output scaling, and a static
screen. Confirm HDR render format/color metadata remain intact and no CPU capture,
extra frame queue, or GPU-to-CPU transfer appears. These checks require a GPU
host with `/dev/dri`; unit tests alone cannot validate the runtime paths.

The implementation is in `compositor/{capture,cursor,focus,handlers,input,mod,
state,x11_focus,gamescope_swapchain}.rs` and
`stream/control/input/{gamepad,mod}.rs`, under `moonshine-core/src/session/`.
