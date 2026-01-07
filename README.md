# Moonshine

Moonshine is a streaming server which implements the protocol used by [Moonlight](https://moonlight-stream.org/).
It is primarily intended for streaming games from the server to a client, while receiving input (mouse, keyboard, controller) from the client.
This means you can play games on the client device, while rendering takes place on the server.

## Requirements and limitations

1. **NVIDIA GPU**. Moonshine currently assumes an NVIDIA GPU for its encoding pipeline. Ideally in the future this pipeline is adjusted to use Vulkan Video Extensions so that more hardware can be supported. In theory it should be easy to adjust the pipeline for other hardware encoders, but this is not implemented at the time of writing.
1. **GStreamer**. Moonshine uses [GStreamer](https://gstreamer.freedesktop.org/) for encoding video streams. To reduce latency, a fix is required in GStreamer, which is available in [this fork](https://gitlab.freedesktop.org/hgaiser/gstreamer/-/tree/nvh264enc-add-max-num-ref-frames).
1. **Gamescope**. Moonshine uses [Gamescope](https://github.com/ValveSoftware/gamescope) in headless mode to run and stream content. This means that Moonshine is independent from whatever runs on the host system (X11, Wayland, etc). This also means you can run Moonshine and stream games, while using the host system for other tasks. Currently this requires a few fixes in Gamescope, which are available in [this fork](https://github.com/hgaiser/gamescope/tree/moonshine).
1. **(Arch) Linux**. Although this software should theoretically run on any Linux distribution, it is only tested on Arch Linux. Windows is currently not supported.
1. **Moonlight v6.0.0 or higher**. Older versions are untested and might not work.

## Installation

### Arch

The simplest method is to install through the AUR:

```sh
$ git clone https://aur.archlinux.org/moonshine
$ cd moonshine
$ makepkg -si
```

Or, simply `yay -S moonshine` if `yay` is installed.

You can start the server by starting the user service:

```sh
$ systemctl --user start moonshine
```

### Source

Alternatively, you can also compile directly from source.
The following dependencies are required:

```
avahi
clang
cmake
gcc-libs
glib2
glibc
gstreamer
gst-plugins-base-libs
jq
libc++
libevdev
libpulse
openssl
opus
rust
```

On systems with `pacman` these can be installed with the following command:

```sh
$ sudo pacman -S \
   avahi \
   clang \
   cmake \
   gcc-libs \
   glib2 \
   glibc \
   gstreamer \
   gst-plugins-base-libs \
   jq \
   libc++ \
   libevdev \
   libpulse \
   openssl \
   opus \
   rust
```

Then compile and run:

```sh
$ cargo run --release -- /path/to/config.toml
```

## Configuration

A configuration file is generated if the provided path does not exist.
By default it will be created in `$XDG_CONFIG_HOME/moonshine/config.toml` if you are using the AUR package.
It is possible to add applications that you want to run (more on that below).

### Client pairing

When a client attempts to pair through Moonlight, they are presented with a PIN number.
A notification will appear on the host (assuming your desktop environment supports notifications) that you can use to automatically go to the page where your PIN number can be filled in.
Alternatively you can navigate to the following URL on the host:

```
http://localhost:47989/pin
```

Or, you can also do this in commandline:

```sh
$ curl "http://localhost:47989/submit-pin?uniqueid=0123456789ABCDEF&pin=<PIN>"
```

Where `<PIN>` should be replaced with the actual PIN number.

### Applications

Each application defined in the configuration is executed within a [gamescope](https://github.com/ValveSoftware/gamescope) session.
This ensures that the application runs in a headless environment, independent of the host's desktop session.

In the `config.toml` file, each application has the following information:

1. `title`. The title as reported in Moonlight.
1. `boxart` (optional). A path to the boxart (image) for this title.
1. `command`. A list of strings representing the command to run. The first entry is the executable, the remaining entries are the arguments. This command is executed within a `gamescope` session.
1. `enable_steam_integration` (optional). Whether to enable Steam integration for this application. Defaults to `false`.

Example:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
enable_steam_integration = true
```

### Application scanners

In addition to defining specific applications, it is also possible to define application scanners.
These scanners scan for applications on startup.
Currently, only a `steam` scanner is implemented.
This scanner searches for a Steam library, checks which games are installed in that library and adds applications with the configured `command`.

The command has an additional template value that gets substituted when executed, the `{game_id}`.
This is replaced with the Steam game id.

The following application scanner will run the game through Steam:

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.local/share/Steam"
command = ["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"]
```

## FAQ

1. **How does this compare to [Sunshine](https://github.com/LizardByte/Sunshine)?**
   There are two main differences between Sunshine and Moonshine:
   1. Sunshine has a lot more features and wider hardware support.
      It supports AMD and Intel GPUs, as well as Windows as a host OS.
   2. Moonshine uses Gamescope for running applications in a headless environment.
      This has a few benefits:
      - It isolates the streaming session from the host desktop session.
        This means that the host system can be used for other tasks while streaming games.
        Note that this does not allow multi-seat gaming using controllers, as these are not isolated.
        It might allow multi-seat gaming using keyboard and mouse since these input events are "injected" into the Gamescope session.
      - It allows running applications that require a graphical environment without needing to have an active desktop session.
        This is especially useful for headless servers, i.e. without a graphical environment.
        This also means that no monitor (or HDMI dummy plug) needs to be connected to the GPU for Moonshine to work.

## Acknowledgement

This wouldn't have been possible without the incredible work by the people behind both [Moonlight](https://moonlight-stream.org/) and [Sunshine](https://github.com/LizardByte/Sunshine).

A special shoutout to @ABeltramo for implementing [`inputtino`](https://github.com/games-on-whales/inputtino) and helping with the controller input implementation!

## TODO's

Below are improvements intended for Moonshine.
If you are interested in contributing, feel free to create an issue or send a message on the [Moonlight Discord](https://discord.com/invite/moonlight-stream-352065098472488960) server.

1. [x] Investigate replacing input handling with [inputtino](https://github.com/games-on-whales/inputtino) for better support.
1. [ ] Replace openssl with [rustls](https://crates.io/crates/rustls).
1. [x] Investigate replacing ffmpeg with gstreamer as it seems to have better Rust support.
1. [ ] Replace `xdg-desktop-portal` with some hardware agnostic frame capture (at the time of writing it seems this does not exist).
1. [ ] Replace NVENC with [Vulkan Video Extensions](https://www.khronos.org/blog/khronos-finalizes-vulkan-video-extensions-for-accelerated-h.264-and-h.265-encode).
1. [x] AV1 support.
1. [ ] HDR support.
1. [ ] 5.1 / 7.1 audio support.
1. [x] Gyro support for controllers that support it.
1. [x] Change controller ID based on what the client registers (this should correctly show Xbox buttons in some games when using Xbox controllers, for example).
1. [x] Web interface https://github.com/hgaiser/moonshine/issues/4 .
1. [ ] Reject clients based on provided certificate.
