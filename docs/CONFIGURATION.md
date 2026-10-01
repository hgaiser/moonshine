# config.toml reference

This reference covers every setting deserialized from `config.toml`. Defaults
and required fields follow the Rust configuration types in
[config.rs](../moonshine-core/src/config.rs) and their nested types.
Environment variables and client-negotiated stream parameters are separate
from this file.

## File location and loading

The CLI reads `$XDG_CONFIG_HOME/moonshine/config.toml` when `XDG_CONFIG_HOME` is
set and nonempty, otherwise `~/.config/moonshine/config.toml`. You can pass a
custom file as a positional argument:

```sh
pyroshine /absolute/path/config.toml
```

If the file does not exist, Pyroshine creates it with defaults. It reads the file
at startup; restart the server after edits. Invalid TOML or missing required
fields prevents startup. Unknown fields are not rejected by the current parser,
so check spelling carefully.

The packaged systemd unit explicitly passes `/home/<user>/.config/moonshine/config.toml`.
To use another location, run `sudo systemctl edit "pyroshine@$USER"` and add
an override (replace the path with the streaming user's file):

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/start-pyroshine.sh /absolute/path/config.toml
```

For the portable install use `/opt/pyroshine/start-pyroshine.sh` instead.
Then restart the service. Changing the config location does not automatically
move certificates or pairing state. Certificate defaults still use
`$HOME/.config/moonshine`; pairing state uses the data directory under
`moonshine` (`~/.local/share/moonshine` by default).

## TOML structure and defaults

Top-level keys must appear before any `[table]` or `[[array_of_tables]]` header.
A table header changes the scope of subsequent keys. Applications and scanners
are arrays of tables; repeat their headers to add entries.

Omitted top-level settings use defaults. Most nested tables also default their
individual fields, with one exception: if `[webserver]` is present, `port`,
`port_https`, `certificate`, and `private_key` are required. Only
`enable_pairing` can be omitted from that table.

If `application` is omitted, one Steam Big Picture entry is supplied. If
`application_scanner` is omitted, a Steam scanner is supplied using
`$HOME/.local/share/Steam` and
`["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"]`.
Explicit arrays replace those defaults. Use `application = []` or
`application_scanner = []` at the top of the file to disable either default list.

A minimal file with one explicit application and no scanner:

```toml
name = "Pyroshine"
application_scanner = []

[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
```

## Top-level settings

| Setting | Type / default | Effect |
| --- | --- | --- |
| `name` | string, `"Pyroshine"` | Host name shown to clients and advertised through discovery. |
| `address` | string, `"0.0.0.0"` | Bind address for web and stream listeners. `0.0.0.0` is IPv4; `::` enables dual-stack IPv4/IPv6. |
| `inhibit_sleep` | boolean, `true` | Ask logind to block suspend while streaming. Requires the shipped polkit rule and `pyroshine` group access; the packaged service grants that group. Failure logs a warning and does not prevent streaming. |
| `application` | array of tables, Steam entry | Static applications exposed to clients; see below. |
| `application_scanner` | array of tables, Steam scanner | Dynamically discover installed applications at startup and append them to the static list; see below. |
| `webserver` | table | HTTP/HTTPS ports, pairing, and TLS files. |
| `stream` | table | RTSP, timeout, video, audio, and control settings. |
| `compositor` | table | GPU, capture, HDR, window focus, and keyboard settings. |

## `[webserver]`

[Source](../moonshine-core/src/webserver/mod.rs). Omit the entire table to use
all defaults, or provide all four required fields when defining it:

```toml
[webserver]
port = 47989
port_https = 47984
enable_pairing = true
certificate = "$HOME/.config/moonshine/cert.pem"
private_key = "$HOME/.config/moonshine/key.pem"
```

| Setting | Type / default when table is omitted | Effect |
| --- | --- | --- |
| `port` | integer, `47989` | HTTP GameStream API and pairing PIN page port (TCP). Required in an explicit table. |
| `port_https` | integer, `47984` | HTTPS GameStream API port (TCP). Required in an explicit table. |
| `enable_pairing` | boolean, `true` | Allow new clients to pair. Set `false` after pairing to disable new pairings. |
| `certificate` | path string, `$HOME/.config/moonshine/cert.pem` | TLS certificate file, created if needed. Required in an explicit table. Supports `~` and environment-variable expansion. |
| `private_key` | path string, `$HOME/.config/moonshine/key.pem` | TLS private key file, created if needed. Required in an explicit table. Supports `~` and environment-variable expansion. Keep it private. |

Changing ports may require corresponding client and firewall changes. Keep
listeners accessible only over a trusted LAN or VPN.

## `[stream]`

[Source](../moonshine-core/src/session/stream/mod.rs).

| Setting | Type / default | Effect |
| --- | --- | --- |
| `port` | integer, `48010` | RTSP negotiation listener port (TCP). |
| `timeout` | nonnegative integer, `60` | Seconds without stream pings before the stream closes. |
| `video` | table | Video transport settings below. |
| `audio` | table | Audio transport settings below. |
| `control` | table | Control transport and gamepad settings below. |

### `[stream.video]`

[Source](../moonshine-core/src/session/stream/video/mod.rs).

| Setting | Type / default | Effect |
| --- | --- | --- |
| `port` | integer, `47998` | Video listener port (UDP). |
| `fec_percentage` | integer (0–255), `20` | Parity protection as a percentage of data packets in fixed mode; initial percentage in auto mode. Actual wire protection depends on packet counts and protocol limits. |
| `fec_mode` | string, `"fixed"` | `off` disables parity, including client-requested minimums; `fixed` uses the configured percentage; `auto` adjusts from client FEC status feedback. Without feedback, auto retains its initial value. |
| `fec_min_percentage` | integer (0–255), `0` | Lower bound for auto FEC. |
| `fec_max_percentage` | integer (0–255), `25` | Upper bound for auto FEC. Use a value at least as large as the minimum; the controller raises a smaller maximum to the minimum. |
| `pyrowave_queue` | string, `"auto"` | Prefer graphics for PyroWave. `"auto"` and `"graphics"` select graphics; `"compute"` explicitly requests compute with library fallback to graphics. All modes retain normal Vulkan queue priority. |
| `encrypt` | boolean, `false` | Enable AES-128-GCM video encryption when supported by client negotiation. |
| `log_stats` | boolean, `true` | Emit five-second capture, pipeline, transport, DMA-BUF, runtime/CPU/memory/fd summaries and swapchain feedback. `false` skips diagnostic accumulation and process sampling; benchmark statistics, operational warnings/errors, and separately enabled frame-spike logs remain available. |
| `log_frame_spikes` | boolean, `false` | Warn when a frame's encoding and packetization exceeds the frame budget. Useful for latency diagnostics. |
| `max_packet_size` | nonnegative integer, `0` | Cap the client-requested stream packet size in bytes. `0` disables the cap; caps below `200` are ignored with a warning. Smaller client requests are honored. |

To silence periodic streaming statistics, add this to the existing video table
and restart Pyroshine (set it to `true` to enable them again):

```toml
[stream.video]
log_stats = false
```

This setting controls diagnostic output and collection. It does not alter GPU
encoding, capture cadence, bitrate, FEC, or transport behavior. The optional WSI
layer's `MOONSHINE_WSI_LOG` filter and per-call TRACE logging remain separate.

Auto FEC clamps its initial `fec_percentage` to the configured minimum/maximum.
FEC adds bandwidth overhead; it does not replace a reliable network connection.

`max_packet_size` is a stream size, not an interface MTU. The UDP payload has
16 additional bytes of stream overhead. For example, an IPv4 path with MTU
1420 can use `1376` (1420 − 20 IP − 8 UDP − 16 stream overhead). IPv6 and other
tunnels have different overheads. See [PyroWave transport details](PYROWAVE.md).

### `[stream.audio]`

[Source](../moonshine-core/src/session/stream/audio/mod.rs).

| Setting | Type / default | Effect |
| --- | --- | --- |
| `port` | integer, `48000` | Audio listener port (UDP). |

Channel count, audio quality, resolution, FPS, bitrate, and video codec are
negotiated with the client; they are not additional `config.toml` settings.

### `[stream.control]`

[Source](../moonshine-core/src/session/stream/control/mod.rs).

| Setting | Type / default | Effect |
| --- | --- | --- |
| `port` | integer, `47999` | Control/input listener port (UDP). |
| `gamepad` | table | Virtual controller and Home-button settings below. |

### `[stream.control.gamepad]`

[Source](../moonshine-core/src/session/stream/control/input/gamepad.rs).

| Setting | Type / default | Effect |
| --- | --- | --- |
| `emulation` | string, `"auto"` | Virtual controller family: `auto`, `xbox`, `playstation`, or `nintendo`. Auto preserves recognized client families and uses Xbox for Steam/unknown kinds. Advanced motion, touch, and feedback features depend on the selected family. |
| `home_button` | table | Intentional Home/Guide shortcut below. |

### `[stream.control.gamepad.home_button]`

| Setting | Type / default | Effect |
| --- | --- | --- |
| `trigger` | optional string, unset | `disabled`, `hold_back` (legacy), or `back_start` (recommended). Unset preserves old configurations: nonzero `hold_ms` selects `hold_back`; zero disables mapping. |
| `hold_ms` | nonnegative integer, `0` | Hold the selected trigger this many milliseconds before emitting Guide. `0` disables synthetic mapping for every trigger. |
| `rumble_duration_ms` | nonnegative integer, `50` | Duration of the activation rumble pulse; `0` disables it. |
| `rumble_intensity` | number, `0.5` | Activation pulse strength from `0.0` to `1.0`. |
| `suppress_home` | boolean, `false` | Drop the client's physical Home/Guide button; remapped Home presses remain available. |

The global default remains disabled, so Select/Back can be held for any duration.
Physical Guide is already supported by Moonlight's controller button flags;
use it directly when available. `suppress_home` filters only physical Guide,
including when synthetic mapping is disabled, and never filters synthetic Guide.

With `back_start`, lone Back and Start presses pass through immediately. When a
packet first contains both, both are released/consumed and a one-shot hold timer
starts. Holding the complete chord through the threshold sends Guide and the
configured rumble pulse. Releasing either member releases Guide. Both members
remain consumed until both are released, including when cancelled before the
threshold; no synthetic Back/Start taps are replayed. Other buttons, including
DualSense Edge extended buttons, pass through unchanged. Each controller has
independent state, discarded on disconnect or session teardown.

This deliberately prioritizes immediate ordinary input: if Back or Start was
sent before its partner arrived, the game can see that earlier single-button
press. It cannot be retroactively erased. An intentional chord is consumed as
soon as both members are observed; it does not send the combination to gameplay.
Games using Back+Start themselves should use `disabled` or physical Guide.

**Migration:** existing configurations specifying only `hold_ms = 750` remain
legacy `hold_back`: Back is withheld, short release emits a 100 ms Back tap,
and a long hold emits Guide. To restore real Select holds while retaining a
shortcut, add `trigger = "back_start"` to that table. `trigger = "disabled"`
disables synthesis regardless of a retained nonzero threshold. Unknown trigger
names fail configuration parsing. Explicit policies are preserved on serialization.

Recommended example:

```toml
[stream.control.gamepad]
emulation = "auto"

[stream.control.gamepad.home_button]
trigger = "back_start"
hold_ms = 750
rumble_duration_ms = 50
rumble_intensity = 0.5
suppress_home = true
```

## `[compositor]`

[Source](../moonshine-core/src/session/compositor/mod.rs).

| Setting | Type / default | Effect |
| --- | --- | --- |
| `gpu` | optional string, automatic selection | Select a DRM render node using an absolute path (such as `/dev/dri/renderD128`), render-node name, or case-insensitive substring of its device `uevent` information (such as a PCI identifier). `MOONSHINE_RENDER_NODE`, if set, overrides this. |
| `capture_mode` | string, `"auto"` | `auto` directly exports a fullscreen DMA-BUF only when it represents the complete visible scene; `composited` forces GLES scene composition for compatibility diagnosis. |
| `hdr` | boolean, `true` | Allow HDR when both the GPU probe and client support it. Disabling it also stops advertising HDR support. |
| `steam_mode` | boolean, `true` | Enable Steam window filtering and use the Steam-controlled focus strategy, similar to Gamescope's `-e`. |
| `virtual_connector_strategy` | string, `"single_application"` | Focus policy when Steam mode is disabled: `single_application` chooses one highest-priority window; `steam_controlled` uses Steam's focus list; `per_app_id` splits focus by app ID; `per_window` splits it by window. |
| `keyboard` | table | XKB keyboard configuration below. |

See [compositor architecture](COMPOSITOR.md) for scene capture and Steam input behavior.

### `[compositor.keyboard]`

| Setting | Type / default | Effect |
| --- | --- | --- |
| `layout` | string, `"us"` | XKB layout name, for example `us` or `de`. |
| `variant` | string, `""` | XKB layout variant; empty uses the layout default. |
| `model` | string, `""` | XKB keyboard model; empty uses XKB's default. |
| `options` | optional string, unset | XKB options such as `caps:escape`. Omit to use no explicit options. |

## `[[application]]`

[Source](../moonshine-core/src/session/application.rs). Each entry needs a
`title` and a `command`; the other fields may be omitted.

| Setting | Type / default | Effect |
| --- | --- | --- |
| `title` | string, required | Application name shown to clients. Also used to derive its ID, so keep titles distinct and stable. |
| `command` | array of strings, required | Executable followed by arguments. Use a nonempty array and an absolute executable path. |
| `boxart` | optional path string, unset | Local cover image. Missing art is resolved automatically when possible. |
| `output_scale` | optional number, effective `1.0` | Wayland output scale for this application. Finite values from `0.25` through `8.0` are accepted; other values fall back to `1.0`. The physical video resolution stays at the client's requested size. |
| `pre_command` | array of command arrays, `[]` | Run in order before launching the app, via systemd `ExecStartPre`. A failed command can prevent launch. |
| `post_command` | array of command arrays, `[]` | Run after the session's application unit stops, via systemd `ExecStopPost`. Useful for cleanup. |
| `stdout` | optional string, effective `"null"` | systemd `StandardOutput` destination, e.g. `journal`, `file:/path`, or `append:/path`. |
| `stderr` | optional string, effective `"null"` | systemd `StandardError` destination. Set `journal` to capture errors. |
| `launch_timeout_secs` | nonnegative integer, `2` | Time allowed for the application to reach an active state after launch; separate from the wait for pre-commands. Increase for slow launchers. |

Commands are argument arrays rather than shell scripts. For shell syntax,
variable expansion, pipes, or redirection, explicitly run a shell, for example
`["/usr/bin/bash", "-c", "your script"]`. Do not assume `~` or `$HOME` expands
in arbitrary argument strings or boxart paths. Empty hooks and hooks whose
executables cannot be resolved are skipped. TLS paths and scanner source
paths explicitly support shell-style path expansion.

Applications inherit `MOONSHINE_CLIENT_WIDTH`, `MOONSHINE_CLIENT_HEIGHT`, and
`MOONSHINE_CLIENT_FRAMERATE`; these are runtime environment variables, not TOML
fields. See [tips](TIPS.md) for Gamescope and hooks.

```toml
[[application]]
title = "Plasma Desktop"
command = ["/usr/bin/startplasma-wayland"]
output_scale = 1.5
stdout = "journal"
stderr = "journal"
launch_timeout_secs = 10
```

Desktop sessions have [session and login-screen limitations](TIPS.md#run-a-desktop-environment-for-a-full-remote-desktop).

## `[[application_scanner]]`

[Source](../moonshine-core/src/app_scanner/mod.rs). Scanners run at server startup;
restart after installing games to refresh the list. Each entry requires `type`.

All scanner types accept `pre_command`, `post_command`, `stdout`, `stderr`, and
`launch_timeout_secs`, with the same defaults and behavior as applications.
Those values are copied to each discovered application. Scanners do not accept
`output_scale`, `title`, or `boxart` overrides.

| `type` | Source setting | Launch setting | Discovery |
| --- | --- | --- | --- |
| `steam` | `library`: required path string | `command`: required array; `{game_id}` is replaced in each argument | Installed games in the Steam installation and its libraries. |
| `lutris` | `pga_db`: path string, defaults to the user's data directory + `/lutris/pga.db` (usually `~/.local/share/lutris/pga.db`) | `command`: required array; `{slug}` is replaced | Installed entries from the Lutris database. |
| `heroic` | `config_dir`: path string, defaults to native Heroic config when present, otherwise an existing Flatpak Heroic config, otherwise the native path | `command`: required array; `{app_name}` and `{runner}` are replaced | Installed games across Heroic stores and sideloaded apps; excludes DLC. Native config is usually `~/.config/heroic`; Flatpak config is `~/.var/app/com.heroicgameslauncher.hgl/config/heroic`. |
| `desktop` | `directories`: required array of path strings | No `command` field; uses the desktop entry's `Exec` | Recursively scan `.desktop` files in the supplied directories. |

Scanner source paths support `~` and environment-variable expansion. Placeholder
substitution applies to scanner `command` arguments, not pre/post-command hooks.
Steam, Lutris, and Heroic use their available local cover art; the desktop scanner
can resolve icons. Scanner launchers must be installed separately.

The desktop scanner also accepts:

| Setting | Type / default | Effect |
| --- | --- | --- |
| `include_terminal` | boolean, `false` | Include entries marked `Terminal=true`. |
| `resolve_icons` | boolean, `true` | Resolve desktop entry icons into local boxart paths. |

Examples (add only the scanners you want; this list replaces the default scanner):

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.local/share/Steam"
command = ["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"]

[[application_scanner]]
type = "lutris"
command = ["/usr/bin/lutris", "lutris:rungame/{slug}"]
stdout = "journal"
stderr = "journal"

[[application_scanner]]
type = "heroic"
command = ["/usr/bin/heroic", "heroic://launch?appName={app_name}&runner={runner}"]

[[application_scanner]]
type = "desktop"
directories = ["$HOME/.local/share/applications", "/usr/share/applications"]
include_terminal = false
resolve_icons = true
```

For Flatpak Steam, see the [D-Bus wrapper recipe](TIPS.md#run-flatpak-steam-inside-pyroshines-compositor).

## Diagnostics outside config.toml

Use `pyroshine healthcheck` to inspect host capabilities. Logging is controlled
by `MOONSHINE_LOG`, and an explicit PyroWave library can be selected with
`MOONSHINE_PYROWAVE_LIBRARY`. For service environment changes, use a systemd
`[Service]` override with `Environment=...` and restart. Detailed library and
GPU validation guidance lives in [PYROWAVE.md](PYROWAVE.md); build and installation
instructions live in [CONTRIBUTING.md](../CONTRIBUTING.md).
