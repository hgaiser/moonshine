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

## Client and dependency boundary

Client-side SDL/HIDAPI must expose the four normalized paddle slots. Custom
controller mappings or disabling HIDAPI can omit them; the client warns when a
slot is unavailable. Verify the actual SDL version and USB/Bluetooth report
handling in the client build. Host tests cannot establish Windows/macOS client
support. Historical dependency versions are in the [recorded report](reports/DUALSENSE_EDGE.md#client-dependency-snapshot).

Pyroshine retains the pinned upstream Rust Inputtino API and patches only its
native `inputtino-sys` backend. See [local patch maintenance](../vendor/inputtino/LOCAL_CHANGES.md)
before changing identity/report layouts. Client normalization, wire flags, host
routing, virtual HID identity and Steam Input must agree end to end.

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
   `[stream.control.gamepad] emulation = "auto"`. Ensure Steam can open the virtual
   hidraw node; the shipped `60-pyroshine.rules` covers UHID devices.
2. Connect the physical Edge over USB, preferably using its default hardware
   profile for isolation checks. On the client run `lsusb -d 054c:0df2` and an SDL
   controller test utility. Confirm all four normalized paddle events match the
   table and return to released. Repeat later over Bluetooth; `lsusb` does not
   list Bluetooth devices, so use SDL's reported VID/PID or `udevadm info`.
3. Start Pyroshine with DEBUG logging (`MOONSHINE_LOG=moonshine_core=debug`), connect the
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

Automated tests/builds cannot establish physical USB/Bluetooth delivery, Steam's
identification, feedback or reconnect behavior. Perform the acceptance checks on
a host with uinput/UHID/hidraw access and Steam. Build Nix packaging separately
in a Nix environment. Record revisions, platforms and checks not performed.

## Recorded validation

Dated results are preserved in the [historical report](reports/DUALSENSE_EDGE.md).
They are evidence for those revisions, not a substitute for current acceptance.
