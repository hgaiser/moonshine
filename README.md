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
yay -S moonshine
```

To run Moonshine for your user:

1. **Enable user lingering**:
   ```sh
   sudo loginctl enable-linger $USER
   ```
   This allows Moonshine to run applications in the user's session even when the user is not logged in.

   If your user is always logged in when you want to stream, you can skip this step.

2. **Enable the service to start on boot and run immediately**:
   ```sh
   sudo systemctl enable --now moonshine@$USER
   ```

### Source

The following dependencies are required to build:

```sh
sudo pacman -S \
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
cargo run --release -- /path/to/config.toml
```

## Configuration

A configuration file is created automatically if the path you provide doesn't exist.
When using the AUR package, it defaults to `$XDG_CONFIG_HOME/moonshine/config.toml`.

### Pairing with a client

When you connect with Moonlight for the first time, it will show a PIN.
A notification will appear on the host that you can click to open the pairing page, or you can visit it manually at http://localhost:47989/pin .

You can also pair from the command line:

```sh
curl -X POST "http://localhost:47989/submit-pin" -d "uniqueid=0123456789ABCDEF&pin=<PIN>"
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

## Benchmarking

Moonshine ships a `bench` subcommand that runs the full host pipeline (compositor + capture + convert + encode) without any Moonlight client connected. Encoded packets are dropped on the floor; per-frame latency samples are aggregated and reported when the run ends. Useful for iterating on driver, power-management, or pipeline changes without a phone in hand.

```sh
moonshine /path/to/config.toml bench \
  --duration 30 --warmup 5 \
  --resolution 2560x1440 --fps 120 --hdr \
  --codec hevc \
  -- /path/to/your/runner.sh
```

Bench-specific flags:

| Flag | Default | Description |
| --- | --- | --- |
| `--duration <s>` | `30` | Seconds to record after warmup. |
| `--warmup <s>` | `2` | Discard the first N seconds (lets first-frame allocation, XWayland, shader compile spikes settle). |
| `--resolution WxH` | `1920x1080` | Stream resolution. |
| `--fps <n>` | `60` | Target frame rate. |
| `--bitrate <bps>` | `50000000` | Encoder target bitrate. |
| `--codec {h264,hevc,av1}` | `hevc` | Codec to encode with. |
| `--hdr` | off | Stream as BT.2020 PQ 10-bit HDR. |
| `--app <title>` |  | Launch an application from `[[application]]`. Mutually exclusive with the trailing `-- <cmd>` form. |
| `--gpu-stats-interval-ms <n>` | `100` | AMD-only sysfs sampler for the bench summary table. `0` disables. |

Either `--app <title>` or a trailing `-- <cmd>` is required so the harness has something to run inside the bench compositor.

The summary printed at the end shows per-stage latency percentiles (`channel_wait`, `import`, `convert`, `encode`, `packetize`, `send`) and a totals row, plus spike count and observed FPS. The GPU clock / busy table is AMD-only and skipped automatically on other vendors.

## Telemetry (OpenTelemetry)

Moonshine can ship per-frame traces and aggregated metrics over OTLP/gRPC to any compatible collector (Tempo, Jaeger, SigNoz, an `otelcol` passthrough, etc.). Telemetry is fully off by default and has zero overhead until an endpoint is configured. Pipeline branches that would emit spans are compiled out at runtime.

### Configuration

In `config.toml`:

```toml
[telemetry]
otlp_endpoint = "http://localhost:4317"   # set this to enable
service_name = "moonshine"                # optional
trace_mode = "outliers"                   # "none" | "outliers" | "static"
trace_sample_rate = 0.05                  # only used when trace_mode = "static"
metric_export_interval_ms = 10000
```

CLI flags override the config (useful for ad-hoc profiling without editing the file). They apply to both the long-running service and the `bench` subcommand:

| Flag | Description |
| --- | --- |
| `--otlp-endpoint <url>` | OTLP gRPC endpoint. Empty string disables telemetry even if config enables it. |
| `--trace-mode {none,outliers,static}` | Per-frame span emission mode. |
| `--trace-sample-rate <0.0–1.0>` | Static-mode sampling rate. Only consulted when `--trace-mode static`. |

### Trace modes

- **`none`**: no per-frame spans. Metrics still emit if the endpoint is set.
- **`outliers`** (default): only emit spans for frames that took longer than the frame budget. Catches spikes without the per-frame cost.
- **`static`**: emit spans for a deterministic percentage of frames (chosen via `--trace-sample-rate`). The bench subcommand defaults to `static 1.0` (full fidelity) since runs are short.

Sampling decisions are made on the host before the span is created, so rejected frames cost nothing.

### What gets emitted

**Traces:** one `frame.encode` span per sampled frame with attributes `codec`, `hdr`, `buffer_index`, `is_key_frame`, `encoded_bytes`, plus per-stage durations (`channel_wait_us`, `import_us`, `convert_us`, `encode_us`, `packetize_us`, `send_us`, `total_us`). Bench runs are wrapped in a `bench.session` parent span carrying the run summary as fields.

**Metrics:**

| Name | Kind | Notes |
| --- | --- | --- |
| `moonshine.frames` | counter | Frames encoded, tagged by `codec`/`hdr`. |
| `moonshine.spikes` | counter | Frames over the frame budget. |
| `moonshine.total_latency` | histogram (µs) | End-to-end host latency per frame. |
| `moonshine.stage_latency` | histogram (µs) | Per-stage latency, tagged by `stage`. |
| `moonshine.encoded_bytes` | histogram | Bytes per frame. |
| `moonshine.dmabuf.cache_size` | gauge | Resident DMA-BUF imports (leak indicator). |

A worked-example Grafana / Tempo / Prometheus / OTel-collector stack is provided under [`examples/observability`](examples/observability/). Run `docker compose up -d` from that directory and point Moonshine at `http://localhost:4317`.

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
