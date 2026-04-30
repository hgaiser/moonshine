# Local OpenTelemetry stack for moonshine

A self-contained Grafana + Tempo + Prometheus stack wired through an OTel
collector. Useful for:

- Watching streaming-session latency live (per-frame traces, per-stage
  histograms, DMA-BUF importer cache size)
- Catching slow-burn regressions like "host latency creeps from 5ms to
  55ms over 10 min" before users hit them
- Comparing benchmark runs (`moonshine bench`) against real-game
  sessions on the same dashboard

## Usage

From this directory:

```sh
docker compose up -d
```

Then point moonshine at `http://localhost:4317` either by editing
`config.toml`:

```toml
[telemetry]
otlp_endpoint = "http://localhost:4317"
trace_sample_rate = 0.05      # optional; 1% of normal frames + all spikes
```

…or via the global CLI flag:

```sh
moonshine --otlp-endpoint http://localhost:4317 ~/.config/moonshine/config.toml
```

The same flag applies to the bench harness:

```sh
moonshine --otlp-endpoint http://localhost:4317 ~/.config/moonshine/config.toml \
  bench --duration 600 --warmup 10 --codec hevc --resolution 2560x1440 --fps 120 --hdr \
  --app "Cyberpunk Benchmark"
```

Open Grafana at <http://localhost:3000> (anonymous-admin enabled). The
"Moonshine Streaming Pipeline" dashboard is provisioned automatically.

## What gets emitted

**Traces** (Tempo):

- One `frame.encode` span per encoded frame, with attributes:
  - `codec` (h264/hevc/av1), `hdr` (true/false), `buffer_index`
  - `channel_wait_us`, `import_us`, `convert_us`, `encode_us`,
    `packetize_us`, `send_us`, `total_us`, `encoded_bytes`,
    `is_key_frame`
- Sampled by `trace_sample_rate` (default 1%); spike frames can be
  forced into the sampled set by callers (TODO).

**Metrics** (Prometheus, exported via OTLP):

- `moonshine.frames` — counter, frames encoded, tagged by `codec`/`hdr`
- `moonshine.spikes` — counter, frames over frame budget
- `moonshine.total_latency` — histogram, end-to-end host latency (µs)
- `moonshine.stage_latency` — histogram, per-stage latency, tagged by
  `stage` (channel_wait/import/convert/encode/packetize/send)
- `moonshine.encoded_bytes` — histogram, bytes per frame
- `moonshine.dmabuf.cache_size` — gauge, count of resident DMA-BUF
  imports (the leak indicator from the 2026-04-29 incident)

## Bringing your own collector

Everything in this directory is provided as a worked-example. Drop the
`otelcol`, `tempo`, `prometheus`, and `grafana` services into whatever
observability stack you already run; or point moonshine at your existing
OTLP endpoint and ignore this whole directory.

## Cleanup

```sh
docker compose down -v   # -v removes the named volumes (clears history)
```
