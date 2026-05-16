# Telemetry and a local OpenTelemetry stack for moonshine

Moonshine can optionally ship per-frame traces and aggregated metrics over OTLP/gRPC to any compatible collector (Tempo, Jaeger, SigNoz, an `otelcol` passthrough, etc.). Telemetry is fully off by default and has zero overhead until an endpoint is configured. Pipeline branches that would emit spans are compiled out at runtime.

This directory also ships a self-contained Grafana + Tempo + Prometheus stack wired through an OTel collector, suitable for local profiling without setting up a full observability pipeline.

## Configuration

In `config.toml`:

```toml
[telemetry]
otlp_endpoint = "http://localhost:4317"   # set this to enable
service_name = "moonshine"                # optional
trace_mode = "outliers"                   # "none" | "outliers" | "static"
trace_sample_rate = 0.05                  # only consulted when trace_mode = "static"
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
- **`outliers`** (default): only emit spans for frames that took longer than the frame budget. Catches spikes without the per-frame cost. `trace_sample_rate` has no effect in this mode.
- **`static`**: emit spans for a fixed fraction of frames (set by `trace_sample_rate`). Sampling is deterministic — based on a per-session monotonic frame counter so the keep set is uniformly distributed across the run. The bench subcommand defaults to `static 1.0` (full fidelity) since runs are short.

Sampling decisions are made on the host before the span is created, so rejected frames cost nothing.

## What gets emitted

**Traces** (Tempo):

- One `frame.encode` span per sampled frame, with per-stage timings recorded as attributes on the span: `channel_wait_us`, `import_us`, `convert_us`, `encode_us`, `packetize_us`, `send_us`, `total_us`, plus `codec`, `hdr`, `buffer_index`, `is_key_frame`, `encoded_bytes`.
- Bench runs are wrapped in a `bench.session` parent span carrying the run summary as fields.

**Metrics** (Prometheus, exported via OTLP):

| Name | Kind | Notes |
| --- | --- | --- |
| `moonshine.frames` | counter | Frames encoded, tagged by `codec`/`hdr`. |
| `moonshine.spikes` | counter | Frames over the frame budget. |
| `moonshine.total_latency` | histogram (µs) | End-to-end host latency per frame. |
| `moonshine.stage_latency` | histogram (µs) | Per-stage latency, tagged by `stage`. |
| `moonshine.encoded_bytes` | histogram | Bytes per frame. |
| `moonshine.dmabuf.cache_size` | gauge | Resident DMA-BUF imports. |

## Local stack

The stack in this directory is useful for:

- Watching streaming-session latency live (per-frame traces, per-stage histograms, DMA-BUF importer cache size)
- Catching slow-burn regressions like "host latency creeps from 5ms to 55ms over 10 min" before users hit them
- Comparing benchmark runs (`moonshine bench`) against real-game sessions on the same dashboard

From this directory:

```sh
docker compose up -d
```

Then point moonshine at `http://localhost:4317` (the `[telemetry]` config above), or via CLI:

```sh
moonshine --otlp-endpoint http://localhost:4317 ~/.config/moonshine/config.toml
```

Same flag works with the bench harness:

```sh
moonshine --otlp-endpoint http://localhost:4317 ~/.config/moonshine/config.toml \
  bench --duration 600 --warmup 10 --codec hevc --resolution 2560x1440 --fps 120 --hdr \
  --app "Cyberpunk Benchmark"
```

Open Grafana at <http://localhost:3000> (anonymous-admin enabled). The "Moonshine Streaming Pipeline" dashboard is provisioned automatically.

## Bringing your own collector

Everything in this directory is provided as a worked example. Drop the `otelcol`, `tempo`, `prometheus`, and `grafana` services into whatever observability stack you already run, or point moonshine at your existing OTLP endpoint and ignore this whole directory.

## Cleanup

```sh
docker compose down -v   # -v removes the named volumes (clears history)
```
