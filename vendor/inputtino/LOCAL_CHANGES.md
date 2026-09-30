# Local inputtino backend patch

Base: https://github.com/games-on-whales/inputtino at
`d28ec79eb63324e68d73a7de22bcb5ff0a6f6bf8` (also upstream HEAD when checked
2026-09-30). The native sources, build files, Rust bindings and upstream tests
are copied with their MIT license. This directory is a maintained local patch,
not a replacement upstream or an unpinned fork.

The public Rust `inputtino` dependency remains the upstream Git crate at that
revision. The workspace patches only `inputtino-sys` to this source tree.
Cargo.lock changes only the source of `inputtino-sys`; no dependency revision
was upgraded. Nix includes this directory in its source and no longer needs to
graft another copy of the upstream native library into the Cargo vendor tree.

Changes from upstream:

- `src/uhid/include/uhid/ps5.hpp`: name Edge bits 4–7 of `buttons[2]`.
- `src/uhid/include/uhid/dualsense_edge.hpp`: identity check and explicit,
  allocation-free mapping of the four existing paddle flags.
- `src/uhid/include/uhid/protected_types.hpp`: retain the selected Edge subtype.
- `src/uhid/joypad_ps5.cpp`: enable Edge bits only for Sony 054c:0df2;
  ordinary DualSense still ignores paddle flags. The existing complete-state
  reset clears released Edge buttons. No new input or feedback pipeline.
- `bindings/rust/inputtino-sys/build.rs`: watch native sources for local rebuilds.
- `CMakeLists.txt`, `tests/dualsense_edge.cpp`: optional hardware-independent
  tests of every combination, release, ordinary-button isolation and report layout.

The existing Bluetooth UHID descriptor, 63-byte common input report, headers,
CRC, feature replies and output handlers are retained. Edge uses the same common
input structure as standard DualSense in Linux and SDL, with the previously
unused bits in `buttons[2]` now populated. USB byte 10 / Bluetooth byte 11 contains
that byte. This backend continues exposing a virtual Bluetooth device regardless
of the physical client's connection transport.

Sources:

- [SDL2 canonical mapping](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/SDL_gamecontroller.c)
- [SDL2 PS5 report parser](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/hidapi/SDL_hidapi_ps5.c)
- [Packaged SDL3 mapping](https://github.com/libsdl-org/SDL/blob/ddba673cddfa79ce798eca5ba402e0f1a2a2a243/src/joystick/SDL_gamepad.c)
- [Linux hid-playstation](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-playstation.c)

When updating inputtino, compare these specific changes against upstream first.
If upstream implements the same mapping, remove this patch and update the pinned
Git dependency and Nix packaging together. Preserve `LICENSE` when redistributing.
