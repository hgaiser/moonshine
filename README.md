[![CI](https://github.com/hgaiser/moonshine/workflows/Test/badge.svg)](https://github.com/hgaiser/moonshine/actions)

# Moonshine 🌙

Moonshine lets you stream games from your PC to any device running [Moonlight](https://moonlight-stream.org/).
Your keyboard, mouse, and controller inputs are sent back to the host so you can play games remotely as if you were sitting in front of it.

## Features

- **Isolated streaming sessions**: Each stream runs in its own compositor, completely separate from your desktop environment. Your host PC can still be used for other things while you stream.
- **No monitor required**: Works on headless servers — no HDMI dummy plug needed.
- **Hardware video encoding**: H.264, H.265, and AV1 encoding using the GPU.

> ⚠️ **AV1 Warning**: AV1 encoding is experimental and has issues on NVIDIA GPUs that cause frame sizes to grow over time ([see issue](https://github.com/nvpro-samples/vk_video_samples/issues/217)). This should be fixed in driver version 595.44.3.0. Until then, stick with H.264 or H.265.
- **HDR support**: True 10-bit HDR streaming for supported games.
- **Full input support**: Mouse, keyboard, and gamepad (including motion, touchpad, and haptics).
- **Audio streaming**: Stereo and surround sound (5.1/7.1) with low-latency Opus encoding.

## Requirements

1. **Linux only**. Tested on Arch Linux, but it's been reported to work on other Linux distributions too.
1. **systemd**. Required for launching and managing application processes. Almost all modern Linux distributions include it by default.
1. **A GPU with Vulkan video encoding**. NVIDIA RTX, AMD RDNA2+, or Intel Arc.
1. **Moonlight v6.0.0 or higher**. Compatibility with older versions or unofficial ports is not guaranteed.

## Installation

### Arch

The simplest method is to install through the AUR using:

```
$ yay -S moonshine`
```

Start the server with:

```sh
$ systemctl --user start moonshine
```

### Source

The following dependencies are required to build:

```sh
$ sudo pacman -S \
   avahi \
   clang \
   cmake \
   gcc-libs \
   glibc \
   libevdev \
   libpulse \
   make \
   opus \
   pkg-config \
   rust \
   shaderc \
   vulkan-headers \
   wayland
```

Then compile and run:

```sh
$ cargo run --release -- /path/to/config.toml
```

## Configuration

A configuration file is created automatically if the path you provide doesn't exist.
When using the AUR package, it defaults to `$XDG_CONFIG_HOME/moonshine/config.toml`.

### Pairing with a client

When you connect with Moonlight for the first time, it will show a PIN.
A notification will appear on the host that you can click to open the pairing page, or you can visit it manually:

```
http://localhost:47989/pin
```

You can also pair from the command line:

```sh
$ curl -X POST "http://localhost:47989/submit-pin" -d "uniqueid=0123456789ABCDEF&pin=<PIN>"
```

### Adding applications

Each application runs in its own isolated streaming session. Add them to `config.toml` like this:

```toml
[[application]]
title = "Steam"
boxart = "/path/to/steam.png"  # optional
command = ["/usr/bin/steam", "steam://open/bigpicture"]
```

- `title`: The name shown in Moonlight.
- `boxart` (optional): Path to a cover image.
- `command`: The command to run. First entry is the executable, the rest are arguments.

### Application scanners

Scanners automatically detect installed applications so you don't have to add them manually.

**Steam scanner** — finds all installed Steam games:

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.local/share/Steam"
command = ["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"]
```

**Desktop scanner** — finds applications from `.desktop` files:

```toml
[[application_scanner]]
type = "desktop"
directories = [
  "$HOME/.local/share/applications",
  "/usr/share/applications",
]
include_terminal = false
resolve_icons = true
```

## FAQ

1. **How does this compare to [Sunshine](https://github.com/LizardByte/Sunshine)?**
   - Sunshine supports more platforms and has more features overall. Moonshine is Linux-only.
   - Moonshine runs each streaming session in its own isolated environment, separate from your desktop. This means your host PC stays usable while you stream, and it works without an active desktop session.

## Security

Moonshine is **not designed for use on public networks**.
The underlying GameStream protocol has limitations that mean traffic is not fully encrypted at the application level.

If you need to stream over the internet, use a VPN such as [Tailscale](https://tailscale.com/), [WireGuard](https://www.wireguard.com/), or [ZeroTier](https://www.zerotier.com/).

**Do not expose Moonshine ports directly to the internet.**

## Acknowledgement

This wouldn't have been possible without the incredible work by the people behind the following projects:

1. [Moonlight](https://moonlight-stream.org/), without it there would be no client for Moonshine.
2. [Sunshine](https://github.com/LizardByte/Sunshine), which laid a lot of the groundwork for the host part of the API.
3. [Inputtino](https://github.com/games-on-whales/inputtino), for a thorough implementation of input devices.
4. [magic-mirror](https://github.com/colinmarc/magic-mirror), for inspiration of using Vulkan and a Wayland compositor for headless streaming.
