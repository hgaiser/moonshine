[![CI](https://github.com/hgaiser/moonshine/workflows/Test/badge.svg)](https://github.com/hgaiser/moonshine/actions)

# Moonshine

Moonshine is a headless streaming server which implements the protocol used by [Moonlight](https://moonlight-stream.org/).
It is intended for streaming games from a server to a client, while receiving input (mouse, keyboard, controller) from the client.
This means you can play games on the client device, while rendering is done by the server.

## Requirements and limitations

1. **Linux**. Although this software should theoretically run on any Linux distribution, it is only tested on Arch Linux.
1. **systemd**. Moonshine uses `systemd-run` to launch applications in a systemd scope for reliable process cleanup.
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

You can start the server by running the user service:

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
jq
libc++
libevdev
libpulse
opus
rust
shaderc
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
   jq \
   libc++ \
   libevdev \
   libpulse \
   opus \
   rust \
   shaderc
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

Each application defined in the configuration is launched in a headless session using Moonshine's built-in compositor.
This ensures that the application runs independently of the host's desktop session.

In `config.toml` each application has the following information:

1. `title`. The title as reported in Moonlight.
1. `boxart` (optional). A path to the boxart (image) for this title.
1. `command`. A list of strings representing the command to run. The first entry is the executable, the remaining entries are the arguments.

Example:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
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
   1. Sunshine has a lot more features and wider software support. Moonshine currently only works on Linux.
   2. Moonshine uses its own built-in headless compositor (based on [Smithay](https://github.com/Smithay/smithay)) to run applications.
      This has a few benefits:
      - Moonshine isolates the streaming session from the host desktop session.
        This means that the host system can be used for other tasks while streaming games.
        Note that this does not allow multi-seat gaming using controllers, as these are not isolated.
        It might allow multi-seat gaming using keyboard and mouse since these input events are "injected" into the compositor session.
      - Moonshine streams applications without needing an active desktop session.
        This is especially useful for headless servers, i.e. without a graphical environment.
        This also means that no monitor (or HDMI dummy plug) needs to be connected to the GPU for Moonshine to work.

## Acknowledgement

This wouldn't have been possible without the incredible work by the people behind the following projects:

1. [Moonlight](https://moonlight-stream.org/), without it there would be no client for Moonshine.
2. [Sunshine](https://github.com/LizardByte/Sunshine), which laid a lot of the groundwork for the host part of the API.
3. [Inputtino](https://github.com/games-on-whales/inputtino), for a thorough implementation of input devices.
4. [magic-mirror](https://github.com/colinmarc/magic-mirror), for inspiration of using Vulkan and a Wayland compositor for headless streaming.

## TODO's

Below is a wishlist for improvements for Moonshine.
If you are interested in contributing, feel free to create an issue or send a message on the [Moonlight Discord](https://discord.com/invite/moonlight-stream-352065098472488960) server.

1. AV1 support.
1. HDR support.
1. 5.1 / 7.1 audio support.
