# Historical DualSense Edge validation

Recorded during September 2026 development. These results describe the tested
revision and environment; they do not establish current hardware acceptance.
See the [current guide](../DUALSENSE_EDGE.md).

## Results in this workspace

- Full Linux client qmake/make build passed; `moonlight --version` reports 6.1.0.
  Missing SDL2_ttf, Qt5 Quick modules and Vulkan headers were supplied in `/tmp`;
  installed system packages and repository renderer code were not changed.
- `cargo build --locked --offline --workspace` passed; host version is 0.16.7.
- All 14 host input tests passed, including the new Edge and remap cases.
- Client SDL normalization CTest passed with sdl2-compat 2.32.72.
- Controller protocol CTest passed in both the standalone repository and the
  exact client submodule checkout.
- Native inputtino report CTest passed; it also compiled the complete C/C++ backend.
- `cargo clippy --locked --offline -p moonshine-core -- -D warnings`, formatting,
  and tracked-diff whitespace checks passed.

Physical USB/Bluetooth, Steam Input and Nix build checks remain unperformed.

## Client dependency snapshot

The original guide recorded native SDL2 2.28.0 Edge support (absent in 2.26.0),
SDL3 revision `ddba673cddfa79ce798eca5ba402e0f1a2a2a243` with sdl2-compat
`release-2.32.70` in the Linux AppImage workflow, and sdl2-compat 2.32.72 in the
validation environment. Controller database revision
`8d9fefd7b810f2541f78cc7a8ccbd185bc84c7a5` had no Edge entry; SDL's built-in
HIDAPI mapping supplied the controls. Windows/macOS prebuilt dependencies were
from `moonlight-qt-deps` v18.1 and were not validated by the Linux build.
