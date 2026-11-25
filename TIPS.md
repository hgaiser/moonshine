# Tips & Tricks

Recipes for common problems, one for each section.
See the [Configuration](README.md#configuration) section of the README for the configuration fields.

## Table of Contents

- **Steam and launchers**
  - [Close a desktop Steam before streaming Steam](#close-a-desktop-steam-before-streaming-steam)
  - [Close a desktop Lutris before streaming a Lutris game](#close-a-desktop-lutris-before-streaming-a-lutris-game)
  - [Run Flatpak Steam inside Moonshine's compositor](#run-flatpak-steam-inside-moonshines-compositor)
  - [Run Flatpak Bottles inside Moonshine's compositor](#run-flatpak-bottles-inside-moonshines-compositor)
- **Games**
  - [Run games in high-performance mode](#run-games-in-high-performance-mode)
  - [Use Gamescope with the client's resolution](#use-gamescope-with-the-clients-resolution)
- **Host**
  - [Prevent the host from suspending while streaming](#prevent-the-host-from-suspending-while-streaming)
  - [Run a desktop environment for a full remote desktop](#run-a-desktop-environment-for-a-full-remote-desktop)
- **Troubleshooting and tools**
  - [Debug a failing application](#debug-a-failing-application)
  - [Watch per-frame timings from another program](#watch-per-frame-timings-from-another-program)
  - [Open the control panel through a reverse proxy](#open-the-control-panel-through-a-reverse-proxy)
  - [Pair a client from the command line](#pair-a-client-from-the-command-line)

## Steam and launchers

### Close a desktop Steam before streaming Steam

#### How to

Add a `pre_command` that shuts down a desktop Steam and waits (up to 30 seconds) until it exits:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
steam_integration = true
pre_command = [
    ["/usr/bin/bash", "-c", "if pgrep -x steam >/dev/null; then /usr/bin/steam -shutdown &>/dev/null; for i in $(seq 1 30); do ! pgrep -x steam >/dev/null && break; sleep 1; done; fi"],
]
```

#### Details

Steam allows only one instance for each user. If Steam runs on your desktop, Moonshine's launch goes to that instance, and Big Picture opens on the desktop instead of in the stream ([#134](https://github.com/hgaiser/moonshine/issues/134)).
This closes your desktop Steam when a stream starts.

### Close a desktop Lutris before streaming a Lutris game

#### How to

Lutris has no command to quit, so stop its process before the stream starts:

```toml
[[application_scanner]]
type = "lutris"
command = ["/usr/bin/lutris", "lutris:rungame/{slug}"]
pre_command = [
    ["/usr/bin/bash", "-c", "pkill -f 'python[^ ]* [^ ]*bin/[l]utris'; for i in $(seq 1 30); do ! pgrep -f 'python[^ ]* [^ ]*bin/[l]utris' >/dev/null && break; sleep 1; done"],
]
```

The same `pre_command` works on an `[[application]]` that runs a Lutris game.

#### Details

Lutris allows only one instance too ([#233](https://github.com/hgaiser/moonshine/issues/233)). If Lutris is open on your desktop, the game starts on the desktop, outside the stream.
Lutris runs as `python3 /usr/bin/lutris` (or `/app/bin/lutris` in the Flatpak), so `pgrep -x lutris` does not find it.
The pattern matches the Python interpreter followed by the Lutris script. The `[l]` keeps it from matching the `bash -c` command that contains it.

### Run Flatpak Steam inside Moonshine's compositor

#### How to

Wrap the `command` with `dbus-run-session`:

```toml
[[application]]
title = "Steam Flatpak"
command = [
    "dbus-run-session", "--",
    "flatpak", "run", "com.valvesoftware.Steam",
    "steam://open/bigpicture",
]
steam_integration = true
```

For the Steam application scanner:

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam"
command = [
    "dbus-run-session", "--",
    "flatpak", "run", "com.valvesoftware.Steam",
    "steam://rungameid/{game_id}",
]
```

#### Details

Flatpak asks the desktop portal on your D-Bus session bus how to set up the sandbox. The portal gives it the Wayland and X11 sockets of your desktop, not the `WAYLAND_DISPLAY` and `DISPLAY` that Moonshine sets, so Steam opens on your desktop.
`dbus-run-session` starts a new session bus without the portal, so Flatpak uses Moonshine's environment.

### Run Flatpak Bottles inside Moonshine's compositor

#### How to

Wrap the commands with `dbus-run-session`, as for [Flatpak Steam](#run-flatpak-steam-inside-moonshines-compositor):

```toml
[[application_scanner]]
type = "bottles"
command = [
    "dbus-run-session", "--",
    "flatpak", "run", "--command=bottles-cli", "com.usebottles.bottles",
    "run", "-b", "{bottle}", "--program-id", "{program_id}",
]
umu_command = [
    "dbus-run-session", "--",
    "flatpak", "run", "--command=bottles-cli", "com.usebottles.bottles",
    "umu", "run", "--game", "{umu_game}",
]
```

#### Details

The cause is the same as for Flatpak Steam: without `dbus-run-session`, the game opens on your desktop.

## Games

### Run games in high-performance mode

#### How to

Install the `gamemode` package and add your user to the `gamemode` group: `sudo usermod -aG gamemode $USER`.
Then set the Steam launch option of a game (or the global launch option) to:

```
gamemoderun %command%
```

Do **not** use `gamemoderun` on Steam itself in Moonshine's configuration. It preloads a library into Steam and all its child processes, which can stop the session from launching.

#### Details

[GameMode](https://github.com/FeralInteractive/gamemode) applies temporary system optimizations (CPU governor, I/O priority, scheduler) while a game runs.
Without the `gamemoded` daemon or the `gamemode` group, `gamemoderun` starts the game without optimizations and shows no error. Check the daemon with `gamemoded -t`.

### Use Gamescope with the client's resolution

#### How to

Set the Steam launch option of the game to:

```
moonshine-gamescope -- %command%
```

`moonshine-gamescope` ships with Moonshine. In a stream, it runs the game in fullscreen Gamescope at the resolution and frame rate of the client. Outside a stream, it runs the game as it is, so the same launch option also works on the desktop.

- Extra Gamescope arguments go before the `--`, for example `moonshine-gamescope --hdr-enabled -- %command%`.
- For an application's `command`, use `["moonshine-gamescope", "--", "/path/to/game"]`.
- If the command is not found, use its full path: `/usr/bin/moonshine-gamescope` for the packages, or `/opt/moonshine/bin/moonshine-gamescope` for the tarball.

To change the Gamescope command, write it out yourself. Moonshine sets `MOONSHINE_CLIENT_WIDTH`, `MOONSHINE_CLIENT_HEIGHT` and `MOONSHINE_CLIENT_FRAMERATE` for the applications it launches. For example, this renders at 1.5 times the client resolution (supersampling):

```
/usr/bin/gamescope -f -W "$MOONSHINE_CLIENT_WIDTH" -H "$MOONSHINE_CLIENT_HEIGHT" -w $((MOONSHINE_CLIENT_WIDTH * 3 / 2)) -h $((MOONSHINE_CLIENT_HEIGHT * 3 / 2)) -r "$MOONSHINE_CLIENT_FRAMERATE" -- %command%
```

#### Details

Gamescope can also fix games that have focus or scaling problems directly in Moonshine's compositor, because it gives them its own window and input handling.
X11 games (including Proton games) show inside Gamescope. Native Wayland games may not.

## Host

### Prevent the host from suspending while streaming

#### How to

The packaged service does this for you: Moonshine keeps the computer awake during each stream.
To turn this off, turn off "Keep the computer awake while streaming" in the control panel, or set `inhibit_sleep = false` at the top of `config.toml`.

If you run Moonshine outside the service (for example with `cargo run`), add your user to the `moonshine` group:

```sh
sudo usermod -aG moonshine $USER
```

#### Details

Moonshine asks logind for a sleep inhibitor when a stream starts, and releases it when the stream ends. This is what `systemd-inhibit` does.
Moonshine runs without a login session, so logind refuses unless polkit allows it. The polkit rule that comes with Moonshine (`50-moonshine-inhibit-sleep.rules`) allows it for processes in the `moonshine` group.
The service unit runs Moonshine with this group (`SupplementaryGroups=moonshine`), so your user does not have to be a member. Polkit gets the groups of the process from the D-Bus daemon (dbus-daemon 1.14 or later, or dbus-broker 23 or later).
The packages create the group and install the rule. For a build from source, see [Building from source](README.md#building-from-source).
If Moonshine cannot get the inhibitor, it logs a warning, and streaming still works. `moonshine healthcheck` shows the result in its "Sleep inhibit" check. Run from a terminal, this check fails if your user is not in the group, even when the service works.

### Run a desktop environment for a full remote desktop

#### How to

Add the desktop environment as an application. For example, for COSMIC:

```toml
[[application]]
title = "COSMIC Desktop"
command = ["/usr/bin/start-cosmic"]
env = { COSMIC_BACKEND = "winit" }
```

Other compositors work the same way, for example `["/usr/bin/sway"]`.

#### Details

- Leave `steam_integration` off. With it on, only Steam windows get focus and keyboard input.
- Run the nested compositor as a Wayland client, not as an X11 client. Moonshine gives a Wayland window the resolution of the stream, but it keeps an X11 window at the size the window asks for. The X11 backend of COSMIC asks for 1280x800, so the desktop stays at that size. `COSMIC_BACKEND = "winit"` makes COSMIC use its Wayland backend.

## Troubleshooting and tools

### Debug a failing application

#### How to

Send the output of the application to the systemd journal. By default, Moonshine discards it:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
steam_integration = true
stdout = "journal"
stderr = "journal"
```

Then read the log of the session unit. Add `-f` to follow it while you launch again:

```sh
journalctl --user -u moonshine-session.service -e
```

#### Details

`stdout` and `stderr` are also available on `[[application_scanner]]` tables.
They accept every systemd `StandardOutput`/`StandardError` value, for example `file:/path/to/log.txt` or `append:/path/to/log.txt`.

### Watch per-frame timings from another program

#### How to

Turn on the statistics socket in your configuration and restart Moonshine:

```toml
[stats]
socket = "$XDG_RUNTIME_DIR/moonshine/stats.sock"
```

Then read it, for example with `socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/moonshine/stats.sock`.

#### Details

The socket sends one JSON object per line: `stream_started`, then one `frame` for each encoded frame with the time of each pipeline stage in microseconds, then `stream_stopped`.
Only the user that runs Moonshine can open the socket. [`src/stats_socket.rs`](src/stats_socket.rs) describes the format.

### Open the control panel through a reverse proxy

#### How to

Add the name of the proxy to `allowed_hosts`, and add the address of the proxy to `allowed_networks`:

```toml
[ui]
allowed_networks = ["127.0.0.1/32", "::1/128", "192.168.1.10/32"]
allowed_hosts = ["moonshine.example.com"]
```

#### Details

The control panel only answers to an IP address, `localhost` or the name of this computer. `allowed_hosts` adds more names.

The control panel then sees the address of the proxy, not the address of the browser. So anyone who can reach the proxy can open the control panel. Put authentication on the proxy, or only make it reachable from networks you trust.

A reverse proxy forwards the page, but not the browser stream (UDP). The browser connects to this computer directly for the stream, at the address that the proxy connected to. If the browser must use another address or name, set it in `stream_host`:

```toml
[ui]
stream_host = "gaming-pc.example.com"
```

The browser must reach UDP port 47990 at that address.

### Pair a client from the command line

#### How to

When Moonlight shows a PIN, send it to Moonshine:

```sh
curl -X POST "http://localhost:47989/submit-pin" -d "pin=<PIN>"
```

#### Details

The PIN goes to the pairing request that waits for one. If no request waits, Moonshine answers 404. If several requests wait, it answers 409: then add the `uniqueid=<id>` of one of them (the pairing page and the log show it).

This does the same as the Clients page of the control panel or the page at http://localhost:47989/pin . It is useful on a host without a browser, for example over SSH.
