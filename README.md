# Moonshine

Moonshine is a streaming server which implements the protocol used by [Moonlight](https://moonlight-stream.org/).
It is primarily intended for streaming games from the server to a client, while receiving input (mouse, keyboard, controller) from the client.
This means you can play games on the client device, while rendering takes place on the server.

## Requirements and limitations

1. **NVIDIA GPU**. Moonshine uses NvFBC to capture the desktop and NVENC for video encoding, both are NVIDIA specific libraries and require an NVIDIA GPU. The goal is to support more hardware in the future, while maintaining a single and therefore simple pipeline. See the todo's at the bottom for more information.
1. **(Arch) Linux**. Although this software should theoretically run on any Linux distribution, it is only tested on Arch Linux. Windows is currently not supported. It should be relatively simple to add Windows compatibility, but at least the input (mouse / keyboard / gamepad) and audio won't work since these use Linux specific libraries. Perhaps in the future, more OS's will be supported (contributions are welcome). For now the focus is on Arch Linux.
1. **Steam Deck / PS4 / PS5 controller**. Similarly, this project is only tested on the mentioned controllers. Your mileage may vary with other controllers.
1. **Moonlight v5.0.0 or higher**. Older versions are untested and might not work.

## Installation

### Arch

The simplest method is to install through the AUR:

```sh
$ git clone https://aur.archlinux.org/moonshine-bin
$ cd moonshine
$ makepkg -si
```

Or, simply `yay -S moonshine-bin` if `yay` is installed.

You can start the server by starting the user service:

```sh
$ systemctl --user start moonshine
```

### Source

Alternatively, you can also compile directly from source.
The following dependencies are required:

```
avahi
cuda
ffmpeg
gcc-libs
glibc
libpulse
nvidia-utils
openssl
opus
```

On systems with `pacman` these can be installed with the following command:

```sh
$ sudo pacman -S \
    avahi \
    cuda \
    ffmpeg \
    gcc-libs \
    glibc \
    libpulse \
    nvidia-utils \
    openssl \
    opus
```

Then compile and run:

```sh
$ cargo run --release -- /path/to/config.toml
```

## Configuration

A configuration file is generated if the provided path does not exist.
By default it will be created in `$XDG_CONFIG_HOME/moonshine/config.toml` if you are using the AUR package.
It is possible to add applications that you want to run (more on that below).

There is also a [resolution](./scripts/resolution) script provided which automatically changes the resolution to the requested resolution.
Note that this file should be modified to refer to the correct display and standard resolution.

The default configuration assumes this `resolution` script is placed in `$HOME/.local/bin`.
If you want to use this functionality, you should copy this script in that location:

```sh
$ mkdir ~/.local/bin
$ curl -Lo ~/.local/bin/resolution https://github.com/hgaiser/moonshine/raw/main/scripts/resolution
```

And modify the values to match your setup.

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

It is important to note that each application that is defined in the config simply starts streaming the entire desktop.
It is the `run_before` part of an applications configuration that defines what to do when an application is started.
Most commonly this will be used to first change the resolution and then launch a game or application.
If no `run_before` is provided, then Moonshine will simply start to stream the desktop without changing resolution or launching anything.

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

A simple example is given below:

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

When the stream has ended, the resolution is returned to the standard resolution by calling the `resolution` script without any arguments.

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

1. **How does this compare to [Sunshine](https://github.com/LizardByte/Sunshine)?** Both Moonshine and Sunshine fulfill the same goal. Moonshine has a much narrower focus on supported platforms. Sunshine attempts to support many different platforms. If your software / hardware is not supported by Moonshine, then you might want to try Sunshine.

    In terms of efficiency, playing the same 7 minute video and recording the average CPU and memory usage (using `ps -p $(pgrep sunshine) $(pgrep moonshine) -o %cpu,%mem,cmd`, on an Intel i9-12900K, 3440x1440 resolution, 60FPS, 51Mbps max bitrate) gives the following results:

    ```
    %CPU %MEM CMD
    19.5  0.7 /usr/bin/sunshine

    %CPU %MEM CMD
    6.5  0.5 /usr/bin/moonshine
    ```

## Acknowledgement

This wouldn't have been possible without the incredible work by the people behind both [Moonlight](https://moonlight-stream.org/) and [Sunshine](https://github.com/LizardByte/Sunshine).

## TODO's

Below are improvements intended for Moonshine.
If you are interesting in contributing, feel free to create an issue or send a message on the [Moonlight Discord](https://discord.com/invite/moonlight-stream-352065098472488960) server.

1. [ ] Replace openssl with [rustls](https://crates.io/crates/rustls).
1. [ ] Investigate replacing ffmpeg with gstreamer as it seems to have better Rust support.
1. [ ] Replace NvFBC with DRM-KMS for hardware agnostic frame capture (however at the time of writing it seems NVIDIA cards do not support this through their proprietary NVIDIA driver).
1. [ ] Replace NVENC with [Vulkan Video Extensions](https://www.khronos.org/blog/khronos-finalizes-vulkan-video-extensions-for-accelerated-h.264-and-h.265-encode). This only really makes sense if NvFBC is replaced as well, otherwise there is still a vendor lock-in.
1. [ ] AV1 support.
1. [ ] HDR support.
1. [ ] 5.1 / 7.1 audio support.
1. [ ] Gyro support for controllers that support it.
1. [ ] Change controller ID based on what the client registers (this should correctly show Xbox buttons in some games when using Xbox controllers, for example).
1. [x] Web interface https://github.com/hgaiser/moonshine/issues/4 .
1. [ ] Reject clients based on provided certificate.
