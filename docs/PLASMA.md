# Isolated headless Plasma 6 desktop

Moonshine exposes Plasma as a first-class application type. A default config
contains a `Desktop` entry; an explicit entry is:

```toml
[[application]]
title = "Desktop"
type = "desktop"
launch_timeout_secs = 10

[application.desktop]
environment = "plasma"
scale = 1.0
```

The `command` field is not used for desktop entries. Width, height, and refresh
pacing come from the connected Moonlight client. `scale` is accepted from 0.5
through 4.0.

## Architecture

```text
Moonlight mode/input
  -> Moonshine headless Smithay compositor and virtual input devices
  -> nested kwin_wayland_wrapper (client-sized window/output)
  -> official startplasma-wayland environment initialization
  -> official plasma_session classic startup
  -> kded6, ksmserver, plasmashell, autostart applications, Xwayland
```

KWin is started before `startplasma-wayland` and registers
`org.kde.KWinWrapper`. The official startup executable initializes KDE's
environment and selects classic startup through the private session bus.
`plasma_session` then detects the existing wrapper service and does not start a
second KWin. This preserves KWin as Plasma's real compositor while allowing
Moonshine to capture the nested output.

Every launch gets:

- a private `XDG_RUNTIME_DIR` with mode 0700;
- private config, cache, and data directories;
- a private D-Bus daemon created by `dbus-run-session`;
- a unique nested Wayland socket and a separate Xwayland instance;
- the existing Moonshine PulseAudio socket for streamed desktop audio;
- the existing Moonshine virtual keyboard, pointer, touch, pen, and gamepads.

The outer Moonshine Wayland socket is passed to KWin by absolute path before
the runtime directory is isolated. Consequently the nested desktop cannot
reuse a local session's Wayland socket or session bus. A local Plasma login may
continue simultaneously.

Plasma is launched in its official classic mode because systemd boot mode would
attach services to the host user manager. Stopping the Moonshine transient unit
kills the complete cgroup; KWin also uses `--exit-with-session`, and
`dbus-run-session` tears down the private bus. The managed launcher catches
SIGTERM, gives both layers up to 15 seconds to exit, reaps them, and only then
removes temporary runtime/config state; a stuck child is killed after that
deadline. Launch failure affects only that stream.

KWin's nested output is created at the requested width, height, and scale. The
Moonshine outer output supplies frame callbacks at the client-selected refresh
rate, which paces nested KWin. Xwayland is started by KWin for legacy desktop
applications.

## Dependencies

Required commands are `dbus-run-session`, `kwin_wayland_wrapper`,
`startplasma-wayland`, `plasma_session`, `plasmashell`, and `Xwayland`.
`moonshine healthcheck` reports missing components when a desktop entry is
configured. Deb/RPM metadata and the NixOS module include Plasma 6, KWin,
D-Bus, Xwayland, PipeWire, and the KDE desktop portal.

Typical packages:

- Arch: `plasma-workspace kwin xorg-xwayland dbus pipewire xdg-desktop-portal-kde`
- Debian/Ubuntu: `plasma-workspace kwin-wayland xwayland dbus-daemon pipewire xdg-desktop-portal-kde`
- Fedora: `plasma-workspace kwin-wayland xorg-x11-server-Xwayland dbus-daemon pipewire xdg-desktop-portal-kde`

## HDR and 4:4:4

Desktop capture uses the same codec-independent output pipeline as games, so a
Plasma session does not depend on PyroWave and may negotiate conventional
4:2:0, conventional 4:4:4, or PyroWave modes. 4:4:4 preserves desktop text and
UI edges when the chosen encoder profile is available.

HDR requires every layer—nested KWin, its Wayland backend, Moonshine's color
management protocol, compositor DMA-BUF format, selected encoder profile, and
client decoder/display—to remain HDR-capable. Moonshine does not force KWin to
claim HDR when that chain is unavailable. Treat Plasma HDR as unvalidated until
the runtime checklist proves actual BT.2020/PQ output on the target stack.

## Manual validation

On a host booted without a physical display or local graphical login:

1. Run `moonshine healthcheck` and resolve the Plasma 6 warning.
2. Connect Moonlight at 1920x1080@60 and launch Desktop.
3. Confirm KWin, plasmashell, panel, launcher, notifications, settings, and an
   X11 application appear and accept input.
4. Confirm desktop audio is streamed.
5. Disconnect and verify the transient service, nested KWin/Xwayland, and
   private D-Bus exit; repeat at least ten times.
6. Repeat at the client's native resolution/refresh and fractional scale.
7. Repeat while an independent local Plasma session is active.
8. Exercise H.264/HEVC/AV1 4:2:0, supported 4:4:4 profiles, and PyroWave.
9. Exercise SDR and, only on an end-to-end HDR-capable stack, HDR10.
