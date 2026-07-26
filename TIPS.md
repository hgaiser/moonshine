# Tips & Tricks

Practical recipes for getting the most out of Moonshine.
Each tip shows a real-world use of `pre_command` / `post_command` (or other configuration) to solve a common problem.
See the [Configuration](../README.md#configuration) section of the README for the full schema of these fields.

## Table of Contents

- [Close a desktop Steam before streaming Steam](#close-a-desktop-steam-before-streaming-steam)
- [Prevent the host from suspending while streaming](#prevent-the-host-from-suspending-while-streaming)
- [Run games in high-performance mode](#run-games-in-high-performance-mode)

## Close a desktop Steam before streaming Steam

### How to

Add a `pre_command` that shuts down any running desktop Steam before the stream starts, so Moonshine's instance becomes the primary one:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
pre_command = [
    ["/usr/bin/bash", "-c", "/usr/bin/steam -shutdown >/dev/null 2>&1 || true; for i in $(seq 1 30); do pgrep -x steam >/dev/null 2>&1 || exit 0; sleep 1; done"],
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

Add your user to the `moonshine` group so Moonshine is allowed to hold a sleep inhibitor: `sudo usermod -aG moonshine $USER`.
Log out and back in (or reboot) for the new group membership to take effect.
In your application's `command`, wrap the real command with `systemd-inhibit`:

```toml
[[application]]
title = "Steam"
command = [
    "/usr/bin/systemd-inhibit", "--what=sleep", "--mode=block", "--who=Moonshine",
    "/usr/bin/steam", "steam://open/bigpicture",
]
```

`systemd-inhibit` holds the block until the wrapped process exits, so no separate `post_command` is needed.

### Details

When installed from a package, Moonshine sets up the `moonshine` group and a polkit rule (`/usr/share/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules`) for you; if you installed Moonshine manually, create the group with `sudo groupadd --system moonshine` and copy that polkit rule file into place yourself.
`moonshine@$USER.service` runs as a system service without a logind session, so logind treats its inhibit request as unprivileged and silently rejects a block-type sleep inhibitor.
The polkit rule grants the `moonshine` group the right to acquire that inhibitor, which is why joining the group is required.

## Run games in high-performance mode

### How to

Install the `gamemode` package for your distribution.
Wrap your application's `command` with `gamemoderun` so the optimizations run for the whole streaming session and are cleaned up automatically when it exits.

```toml
[[application]]
title = "Steam"
command = ["gamemoderun", "/usr/bin/steam", "steam://open/bigpicture"]
```

For individual games launched via the Steam scanner you can do the same on the scanner's `command`:

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.local/share/Steam"
command = ["gamemoderun", "/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"]
```

### Details

[GameMode](https://github.com/FeralInteractive/gamemode) applies a set of temporary system optimizations (CPU governor, I/O priority, scheduler tweaks, etc.) while a game is running, then reverts them when it exits.
GameMode consists of a daemon (`gamemoded`) and a client library; `gamemoderun` asks the daemon to enable optimizations for the process it launches.
The daemon is normally started automatically via D-Bus the first time a game requests it, so no manual step is needed beyond installing the package — you can verify it with `gamemoded -t`.
If the daemon is not installed or not running, `gamemoderun` simply launches the game without applying any optimizations.
