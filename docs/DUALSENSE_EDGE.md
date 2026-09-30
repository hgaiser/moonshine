# Native DualSense Edge streaming

The client identifies Sony VID `054c`, PID `0df2`, keeps the PlayStation family,
and sends `LI_CCAP_DUALSENSE_EDGE` (`0x0200`) in the existing 16-bit arrival
capability field. A PS device with four paddle mappings alone is insufficient:
third-party PlayStation controllers can have extra buttons too. Explicit identity
avoids relabeling those controllers as Sony Edge.

Pyroshine's `auto` and `playstation` emulation preserve this subtype. The virtual
controller uses inputtino's existing PS5 UHID backend with Sony `054c:0df2` and
native Edge report bits. A standard PS arrival uses `054c:0ce6`. Explicit `xbox`
or `nintendo` emulation still selects that configured family.

Old clients retain their previous behavior. Old hosts ignore the new capability
and receive the same PS family and normal controller input. Packet types, lengths,
and button-field encoding are unchanged. The extension adds one capability
constant to moonlight-common-c; it does not add packets or a HID tunnel.

## Mapping

SDL2's paddle names are not left-to-right numbering. Keep its canonical mapping:

| Physical control | SDL2 normalized button | Moonlight flag | `buttons[2]` | Linux event |
| --- | --- | --- | --- | --- |
| Left rear paddle | PADDLE2 | `0x020000` | bit 6 (`0x40`) | BTN_TRIGGER_HAPPY3 |
| Right rear paddle | PADDLE1 | `0x010000` | bit 7 (`0x80`) | BTN_TRIGGER_HAPPY4 |
| Left Fn | PADDLE4 | `0x080000` | bit 4 (`0x10`) | BTN_TRIGGER_HAPPY1 |
| Right Fn | PADDLE3 | `0x040000` | bit 5 (`0x20`) | BTN_TRIGGER_HAPPY2 |

These bits share the standard DualSense common report layout. The existing
Bluetooth framing and CRC are retained, along with touch, motion, battery,
rumble, LED and adaptive-trigger handling. See the precise local dependency
changes and authoritative sources in [inputtino notes](../vendor/inputtino/LOCAL_CHANGES.md).

## SDL requirements

Native SDL2 2.28.0 contains Edge identification and extra-button handling;
2.26.0 does not. The Linux AppImage workflow uses SDL3 at
`ddba673cddfa79ce798eca5ba402e0f1a2a2a243` with sdl2-compat `release-2.32.70`;
that mapping has the same normalized order. Linux system builds use pkg-config's
SDL2. The validation environment reports sdl2-compat 2.32.72.

The controller database is pinned at
`8d9fefd7b810f2541f78cc7a8ccbd185bc84c7a5`; it has no Edge-specific entry.
SDL's built-in HIDAPI mapping supplies the controls, so no database mapping is
added. Prefer that driver and grant the client access to the physical hidraw
node. Custom mappings or disabling HIDAPI can omit controls; the client emits
an arrival warning when any of the four paddle slots is unavailable.

SDL's enhanced report parser handles both physical USB and Bluetooth. The client
already enables PS5 enhanced Bluetooth reports via its rumble hint, and its
adaptive-trigger output accepts the PS5 SDL type used for Edge. Windows/macOS
prebuilt dependencies come from `moonlight-qt-deps` v18.1, rather than source in
this checkout; their installed SDL versions must be checked during platform
validation. Do not claim those platforms tested from a Linux build.

## Automated checks

From the client repository:

```sh
cmake -S tests -B /tmp/edge-client-tests
cmake --build /tmp/edge-client-tests
ctest --test-dir /tmp/edge-client-tests --output-on-failure
```

This covers VID/PID detection, the shared production button map, ordinary buttons,
and an SDL virtual device advertising four paddle buttons with isolated press
and release events. The virtual mapping fixture uses the authoritative SDL2 raw
button order; it does not replace physical HID/USB/Bluetooth testing.

From moonlight-common-c (also available inside the client's submodule):

```sh
cmake -S . -B /tmp/edge-protocol-tests -DCONTROLLER_PROTOCOL_TESTS=ON
cmake --build /tmp/edge-protocol-tests
ctest --test-dir /tmp/edge-protocol-tests --output-on-failure
```

Wire tests check packed sizes, capability byte order, older capability masks,
and both button fields, including all four high bits and release.

From Pyroshine:

```sh
cargo build --locked --workspace
cargo test --locked -p moonshine-core session::stream::control::input
cargo clippy --locked -p moonshine-core -- -D warnings
cargo fmt --all -- --check
cmake -S vendor/inputtino -B /tmp/edge-inputtino-tests \
  -DBUILD_TESTING=OFF -DINPUTTINO_EDGE_TESTS=ON
cmake --build /tmp/edge-inputtino-tests
ctest --test-dir /tmp/edge-inputtino-tests --output-on-failure
```

The native report test covers all 16 Edge-button combinations, releases, standard
DualSense suppression, ordinary-button isolation, common report size and offsets.
Rust tests cover standard/Edge/unknown metadata, emulation policy and high-bit
packet decoding, and preservation/release of Edge bits through hold-to-Home timers. Native sources are rebuilt when the local patch changes.

## Physical and Steam acceptance procedure

1. Install the new client and host with the protocol header update. Keep the
   host's existing uinput/UHID/hidraw permissions and modules configured. Set
   `[gamepad] emulation = "auto"`. Ensure Steam can open the virtual hidraw node;
   the shipped `60-pyroshine.rules` covers UHID devices.
2. Connect the physical Edge over USB, preferably using its default hardware
   profile for isolation checks. On the client run `lsusb -d 054c:0df2` and an SDL
   controller test utility. Confirm all four normalized paddle events match the
   table and return to released. Repeat later over Bluetooth; `lsusb` does not
   list Bluetooth devices, so use SDL's reported VID/PID or `udevadm info`.
3. Start Pyroshine with DEBUG logging (`RUST_LOG=moonshine_core=debug`), connect the
   stream, and inspect arrival/creation diagnostics. They must show PS, Edge=true,
   the supported-button mask containing `0x000f0000`, capability `0x0200`, and
   virtual `054c:0df2`. Enable SDL application DEBUG logging in a diagnostic client
   build to see the client's full arrival record. No per-input INFO logs are added.
4. On the host inspect the virtual HID device. UHID devices do not appear in
   `lsusb`; use these instead:

   ```sh
   cat /proc/bus/input/devices
   ls /sys/bus/hid/devices/*:054C:0DF2.*
   # Substitute the discovered path and event device:
   cat /sys/bus/hid/devices/0005:054C:0DF2.XXXX/uevent
   udevadm info --attribute-walk --name=/dev/input/eventN
   evtest /dev/input/eventN
   ```

   With a kernel containing Edge-specific hid-playstation input support,
   `evtest` should expose the four BTN_TRIGGER_HAPPY codes in the table. Check
   values 1 on press and 0 on release, including simultaneous presses. Older
   kernels may recognize the Edge PID without exposing these four evdev codes;
   verify driver support rather than treating missing evdev buttons as proof
   that raw HID bits are absent. Steam's hidraw view must be checked separately.
5. Open Steam's controller settings on the host. It must explicitly identify a
   **DualSense Edge**. Open its Steam Input layout and verify both rear paddles
   and both Fn buttons are available as four independent native controls.
6. Bind each extra control to a different action. Press/release each individually
   and in combinations. Check that only assigned actions fire and none sticks.
7. Check Cross/Circle/Square/Triangle, D-pad, bumpers, analog triggers, stick
   clicks, Create, Options, Home, touchpad click/coordinates, mute, gyro,
   accelerometer and battery. Test rumble, RGB LED and adaptive effects using a
   Steam/game test that already supports those features.
8. Disconnect/reconnect the physical device, replace it with a standard DualSense
   in the same slot, then restore the Edge. Check Sony `054c:0ce6` for the standard
   device and `054c:0df2` for Edge. Changed arrival metadata must destroy the prior
   UHID device before recreation. Identical arrivals reuse the slot.
9. Disconnect/reconnect the stream; verify stale devices disappear and the correct
   identity returns. Repeat with multiple controllers and with manual PlayStation
   emulation. Explicit Xbox/Nintendo modes should retain their configured policy.
10. Repeat steps 2–9 using Bluetooth, then repeat baseline checks with a standard
    DualSense, Xbox and Nintendo controller. Also exercise new-client/old-host and
    old-client/new-host combinations for baseline compatibility.

## Validation boundary

The development environment has no `/dev/input` or `/dev/uhid`, physical Edge,
or interactive host Steam session. Automated tests and builds cannot establish
Steam's actual identification, hardware report delivery, feedback behavior, or
reconnect lifecycle. Those acceptance checks remain required before declaring
native end-to-end support validated. Nix packaging is updated but must be built
in a Nix environment.

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
