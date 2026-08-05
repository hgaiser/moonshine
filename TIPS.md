# Tips & Tricks

Practical recipes for getting the most out of Moonshine.
Each tip shows a real-world use of `pre_command` / `post_command` (or other configuration) to solve a common problem.
See the [Configuration](../README.md#configuration) section of the README for the full schema of these fields.

## Table of Contents

- [Close a desktop Steam before streaming Steam](#close-a-desktop-steam-before-streaming-steam)
- [Prevent the host from suspending while streaming](#prevent-the-host-from-suspending-while-streaming)
- [Run games in high-performance mode](#run-games-in-high-performance-mode)
- [Use Gamescope with the client's resolution](#use-gamescope-with-the-clients-resolution)
- [Run Flatpak Steam inside Moonshine's compositor](#run-flatpak-steam-inside-moonshines-compositor)
- [Run a desktop environment for a full remote desktop](#run-a-desktop-environment-for-a-full-remote-desktop)

## Close a desktop Steam before streaming Steam

### How to

Add a `pre_command` that shuts down any running desktop Steam before the stream starts, so Moonshine's instance becomes the primary one:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
pre_command = [
    ["/usr/bin/bash", "-c", "if pgrep -x steam >/dev/null; then steam -shutdown &>/dev/null; for i in $(seq 1 30); do ! pgrep -x steam >/dev/null && break; sleep 1; done; fi"],
]
```

This asks any running Steam to shut down and waits (up to ~30s) for it to exit before the streaming instance launches.

### Details

This is the recommended workaround from issue [#134](https://github.com/hgaiser/moonshine/issues/134).
Steam is single-instance per user, so when Moonshine launches Steam inside its own compositor while a desktop Steam is already running on the host, the `steam://` URL (for example "open big picture") is forwarded to the existing desktop instance instead of running inside Moonshine's compositor.
The result is that Big Picture opens on the host's physical desktop and the streaming session fails (Moonlight sees a 503 error).
Note that this closes your desktop Steam session when a stream starts.

## Prevent the host from suspending while streaming

### How to

Make sure your user is in the `moonshine` group: `sudo usermod -aG moonshine $USER`.
Moonshine then blocks sleep automatically for the duration of every stream — nothing else is required.
If you want to disable this, set `inhibit_sleep = false` at the top of your configuration:

```toml
name = "Moonshine"
inhibit_sleep = false
```

### Details

Moonshine asks logind (over D-Bus) to inhibit sleep when a session starts and releases it when the session ends.
This is the same thing `systemd-inhibit` does, but handled by Moonshine itself, so you do not have to wrap each application's `command` with it.
It requires the polkit rule shipped with Moonshine (`/usr/share/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules`) and membership in the `moonshine` group it is scoped to.
When installed from a package, both are set up for you (via the sysusers.d drop-in and the package's post-install step); if you installed Moonshine manually, create the group with `sudo groupadd --system moonshine` and copy that polkit rule file into place yourself.
If the user is not in the `moonshine` group (and has no active session), Moonshine logs a warning and streaming still works, but the host may suspend mid-stream.

## Run games in high-performance mode

### How to

Install the `gamemode` package for your distribution.
Then for each Steam game you want to optimize (or globally in Steam's launch options), set the game's launch option to:

```
gamemoderun %command%
```

Do **not** use `gamemoderun` with Steam anywhere in Moonshine's configuration, since that launches Steam itself under `LD_PRELOAD`, which leaks into Steam's child processes and can prevent the session from launching.

### Details

[GameMode](https://github.com/FeralInteractive/gamemode) applies a set of temporary system optimizations (CPU governor, I/O priority, scheduler tweaks, etc.) while a game is running, then reverts them when it exits.
GameMode consists of a daemon (`gamemoded`) and a client library; `gamemoderun` asks the daemon to enable optimizations for the process it launches.
The daemon is normally started automatically via D-Bus the first time a game requests it, so no manual step is needed beyond installing the package — you can verify it with `gamemoded -t`.

### Requirements

Make sure your user is in the `gamemode` group: `sudo usermod -aG gamemode $USER`.
Without it, `gamemoderun` will not be able to communicate with the daemon and will silently skip applying optimizations.

If the daemon is not installed or not running, `gamemoderun` simply launches the game without applying any optimizations.

## Use Gamescope with the client's resolution

### How to

Set the game's Steam launch option (or the app's `command`) to wrap the game in Gamescope, using the client's resolution from the environment:

```
/usr/bin/gamescope -f -W "$MOONSHINE_CLIENT_WIDTH" -H "$MOONSHINE_CLIENT_HEIGHT" -w "$MOONSHINE_CLIENT_WIDTH" -h "$MOONSHINE_CLIENT_HEIGHT" -r "$MOONSHINE_CLIENT_FRAMERATE" -- %command%
```

For supersampling, render above the output resolution — here 1.5x the client resolution:

```
/usr/bin/gamescope -f -W "$MOONSHINE_CLIENT_WIDTH" -H "$MOONSHINE_CLIENT_HEIGHT" -w $((MOONSHINE_CLIENT_WIDTH * 3 / 2)) -h $((MOONSHINE_CLIENT_HEIGHT * 3 / 2)) -r "$MOONSHINE_CLIENT_FRAMERATE" -- %command%
```

Moonshine sets `MOONSHINE_CLIENT_WIDTH`, `MOONSHINE_CLIENT_HEIGHT` and `MOONSHINE_CLIENT_FRAMERATE` on the environment of the launched application, so they are inherited by Gamescope, Steam and `%command%`.

### Details

- `-W`/`-H` set the output resolution; set them to the client's resolution so the stream is filled.
- `-w`/`-h` set the resolution the game actually renders at. Rendering below the output resolution upscales (bilinear or FSR); rendering above it downsamples for supersampling. You can play with the values to trade quality against performance — when they match the client resolution you get a 1:1 image.
- `-r` sets the refresh rate, e.g. from `MOONSHINE_CLIENT_FRAMERATE`.

Wrapping the game in Gamescope can also work around focus or rendering issues: Gamescope provides its own Wayland/X11 surfaces and input handling, so games that misbehave under Moonshine's compositor directly (unfocused windows, scaling artifacts, etc.) often behave correctly when run inside it.

Note that these environment variables are only set when the app is launched by Moonshine, so fall back to defaults (e.g. `${MOONSHINE_CLIENT_WIDTH:-2560}`) if you also run the same command outside a stream.

## Run Flatpak Steam inside Moonshine's compositor

### How to

Wrap the `command` with `dbus-run-session`:

```toml
[[application]]
title = "Steam Flatpak"
command = [
	"dbus-run-session", "--",
	"flatpak", "run",
	"com.valvesoftware.Steam",
	"steam://open/bigpicture",
]
```

For games launched through Steam's application scanner:

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam"
command = [
	"dbus-run-session", "--",
	"flatpak", "run",
	"com.valvesoftware.Steam",
	"steam://rungameid/{game_id}",
]
```

### Details

The root cause is in how Flatpak's portal infrastructure interacts with the desktop environment.
Flatpak uses the host's D-Bus session bus to communicate with portal backends. When setting up the sandbox, the portal backend tells Flatpak to expose the **host's** Wayland and display sockets, overriding the `WAYLAND_DISPLAY` and `DISPLAY` environment variables that Moonshine sets. This causes Steam to render on the host's physical desktop instead of inside Moonshine's headless compositor.

`dbus-run-session` spawns a fresh D-Bus daemon without the host's portal backend registered, so Flatpak cannot discover the host compositor through the portal and falls back to the environment variables inherited from Moonshine's systemd unit. Because the portal backend is responsible for both Wayland and audio setup, the `PULSE_SERVER` env var is also honored correctly without needing `--env=` overrides.

## Run a desktop environment for a full remote desktop

### How to

Add the desktop environment as a regular application. For example, to stream a full COSMIC desktop instead of a single game:

```toml
[[application]]
title = "COSMIC Desktop"
command = ["/usr/bin/start-cosmic"]
```

### Details

Launching a desktop environment as an application turns the stream into a full remote desktop: the compositor boots inside Moonshine's headless compositor, and you use the desktop's own keybindings to launch programs. Other compositors work the same way, e.g. `["/usr/bin/sway"]`.
