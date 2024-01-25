# Moonshine

Moonshine is a streaming server which implements the protocol used by [Moonlight](https://moonlight-stream.org/).
It is primarily intended for streaming games from the server to a client, while receiving input (mouse, keyboard, controller) from the client.
This means you can play games on the client device, while rendering takes place on the server.

## Requirements and limitations

1. **NVIDIA GPU**. Moonshine uses NvFBC to capture the desktop, which is a NVIDIA library for retrieving the latest buffer from the GPU. There is currently no plan to support other hardware.
1. **(Arch) Linux**. Although this software should theoretically run on any Linux distribution, it is only tested on Arch Linux. Likewise, it should be relatively simple to run this service on Windows, but at least the input (mouse / keyboard / gamepad) support won't work since that uses Linux specific libraries. Perhaps in the future, more OS's will be supported. For now the focus is on Arch Linux.
1. **Steam Deck / PS4 / PS5 controller**. Similarly, this project is only tested on the mentioned controllers. It works well in those cases, other controllers might work, they might not work.

## Installation

### Arch

The simplest method is to install through the AUR:

```sh
$ git clone https://aur.archlinux.org/moonshine-git
$ cd moonshine
$ makepkg -si
```

Or, simply `yay -S moonshine` if `yay` is installed.

Assuming the configuration is placed in `/etc/moonshine/config.toml`, you can start the server by starting the system service:

```sh
$ systemctl --user start moonshine
```

### Source

Alternatively, you can also compile directly from source.
The following dependencies are required:

```
alsa-lib
avahi
ffmpeg
gcc-libs
glibc
nvidia-utils
openssl
opus
```

On systems with `pacman` these can be installed with the following command:

```sh
$ sudo pacman -S \
    alsa-lib \
    avahi \
    ffmpeg \
    gcc-libs \
    glibc \
    nvidia-utils \
    openssl \
    opus
```

Then compile and run:

```sh
$ cargo run --release -- /path/to/config.toml
```

## Preparation

Communication with clients is handled over HTTPS.
The certificate for this communication is self-signed, which we need to create first:

```sh
$ cd cert
$ ./make-cert
```

This produces a `cert.pem` and `key.pem` file.
Place these somewhere (for example in `/etc/moonshine/`).

It is also recommended to modify the [`scripts/resolution`](./scripts/resolution) script and place it in `$HOME/.local/bin`.
Specifically the display output name (as reported by `xrandr`) and the default resolution need to be configured.
This will be used later to adjust resolution to the requested resolution when a stream starts.

## Configuration

A sample configuration file is provided in `config.toml`.
It is useful to go through this file to check if anything needs to be adjusted.
Mostly this comes down to the following:

1. (optional) Change the name of the server.
1. Check that the path to the certificate is correct.
1. Add applications that you want to run from the client (more on that below).

### Applications

It is important to note that each application that is defined in the config simply starts streaming the entire desktop.
It is the `run_before` part of an applications configuration that defines what to do when an application is started.
Most commonly this will be used to first change the resolution and then launch a game.
If no `run_before` is provided, then Moonshine will simply start to stream the desktop without launching anything.

In the `config.toml` file, each application has the following information:

1. `title`. The title as reported in Moonlight.
1. `boxart` (optional). A path to the boxart (image) for this title.
1. `run_before` (optional). A list of commands to execute before starting the stream for this application. Each command is itself a list. The first entry in the list is the executable to run, the remaining entries are the arguments. For example this will simply print `"Hello World"`:

   ```toml
   [[application]]
   title = "Test"
   run_before = [["/usr/bin/echo", "Hello", "World"]]
   ```

1. `run_after` (optional). Similar to `run_before`, but these commands are run after a stream has ended.

The following values are replaced in the commands, before they are executed:

1. `{width}` is replaced with the requested stream width in pixels.
1. `{height}` is replaced with the requested stream height in pixels.
1. Any environment variables, such as `$HOME`.

By combining the `run_before` and `run_after` configuration fields, we can change resolution and launch a game when the application starts and reset to the default resolution when the application ends.

A simple example is given in `config.toml`:

```toml
[[application]]
title = "Steam"
run_before = [
	["$HOME/.local/bin/resolution", "{width}", "{height}"],
	["/usr/bin/steam", "steam://open/bigpicture"],
]
run_after = [["$HOME/.local/bin/resolution"]]
```

This will first call the [`scripts/resolution`](./scripts/resolution) script in `$HOME/.local/bin/resolution` with the requested width and height as arguments.
This will cause the resolution to be changed to the resolution requested by the client.
The next command will open Steam in big picture mode.

### Application scanners

In addition to defining specific applications, it is also possible to define application scanners.
These scanners scan for applications on startup.
Currently, only a `steam` scanner is implemented.
This scanner searches for a Steam library, checks which games are installed in that library and adds applications with the configured `run_before` and `run_after` commands.

These commands have an additional template value that gets substituted when executed, the `{game_id}`.
This is replaced with the Steam game id.

The following application scanner will first change resolution, then open steam, then run a game. After running the application, the resolution is restored to its default value.

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.local/share/Steam"
run_before = [
	["$HOME/.local/bin/resolution", "{width}", "{height}"],
	["/usr/bin/steam", "steam://open/bigpicture"],
	["/usr/bin/steam", "steam://rungameid/{game_id}"],
]
run_after = [
	["$HOME/.local/bin/resolution"],
]
```

## FAQ

1. **How does this compare to [Sunshine](https://github.com/LizardByte/Sunshine)?** Both Moonshine and Sunshine fulfill the same goal. Moonshine has a much narrower focus on supported platforms. Sunshine attempts to support many different platforms and many different encoders. If your software / hardware is not supported by Moonshine, then you are likely better off using Sunshine. If you just want something to stream your games, you should probably also use Sunshine.

## Acknowledgement

This wouldn't have been possible without the incredible work by the people behind both [Moonlight](https://moonlight-stream.org/) and [Sunshine](https://github.com/LizardByte/Sunshine).
Thanks to their hard work it was possible for me to implement this protocol.

## TODO's

1. [ ] Document required setup for audio.
1. [ ] Document pairing process.
1. [ ] Move crates to their own repository and publish on crates.io.
1. [ ] Automatically create certificate when no certificate is found.
1. [ ] AV1 support.
1. [ ] Gyro support for controllers that support it.
1. [ ] Mouse scrolling support.
1. [ ] Change controller ID based on what the client registers (this should correctly show Xbox buttons in some games when using Xbox controllers, for example).
1. [ ] Web interface.
1. [ ] Configure Github Actions for testing.
