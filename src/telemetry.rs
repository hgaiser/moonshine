//! OpenTelemetry integration for moonshine.
//!
//! Two signals are exported when `[telemetry] otlp_endpoint` is set in the
//! config (or `--otlp-endpoint` is passed to the bench harness):
//!
//! - **Traces**: per-frame `frame.encode` root span with child spans for each
//!   pipeline stage (`channel_wait`, `import`, `convert`, `encode`,
//!   `packetize`, `send`). Useful for debugging individual outliers — when
//!   a spike fires you can pull up the trace and see which stage exploded.
//!   Tail-sampled by default (keep all frames > frame_budget, sample 1% of
//!   normal frames) so a 120 fps session doesn't drown the collector.
//!
//! - **Metrics**: pre-aggregated histograms / gauges / counters exported on
//!   a fixed cadence (default 10s). Cheap, full-fidelity, perfect for
//!   dashboards and alerts. The histograms are the same percentiles the
//!   bench text report shows; metrics let you watch them trend over hours
//!   instead of computing them once per bench run.
//!
//! Hot path is never blocked: spans are batched + flushed by a background
//! tokio task, metrics collected via lock-free instruments. If the
//! collector goes away, exports drop on the floor and moonshine keeps
//! streaming.

use opentelemetry::{
	global,
	metrics::{Counter, Gauge, Histogram, Meter},
	trace::TracerProvider as _,
	KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
	metrics::{PeriodicReader, SdkMeterProvider},
	runtime,
	trace::{Sampler, TracerProvider},
	Resource,
};
use opentelemetry_semantic_conventions::resource as semres;
use std::sync::OnceLock;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Global trace-mode snapshot, populated by `init()` and read by hot
/// paths that need to branch on it (the video pipeline). Defaults to
/// `Outliers` when telemetry was never initialized — the right choice
/// when nothing's listening (cheap), and a sensible fallback when
/// somebody's listening and didn't pick.
static TRACE_MODE: OnceLock<TraceMode> = OnceLock::new();

/// Read the process-global trace mode. Cheap (`OnceLock` load).
pub fn trace_mode() -> TraceMode {
	*TRACE_MODE.get().unwrap_or(&TraceMode::Outliers)
}

/// What to emit on the trace channel for video-pipeline frames.
///
/// Metrics (counters / histograms / gauges) are always emitted when
/// telemetry is enabled — they're cheap and pre-aggregated. Tracing is
/// the volume-heavy signal; this knob controls how much we send.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TraceMode {
	/// No frame-level traces. Compositor/session-level spans (and the
	/// `bench.session` span) still flow when telemetry is enabled, but
	/// per-frame work generates nothing. Lowest overhead.
	None,
	/// Emit a `frame.encode` span on every frame and let the sampler
	/// decide. The `f64` is the keep-rate (0.0–1.0). Easy to reason
	/// about and gives evenly-distributed samples, but pays the
	/// allocation/record cost on every frame even when the sampler
	/// drops the span. Useful for steady-state load profiling.
	Static(f64),
	/// Emit a `frame.encode` span ONLY when the frame's total latency
	/// exceeds the frame-rate budget (i.e. it's already a spike). All
	/// emitted spans are sampled. Zero per-frame allocation in the
	/// happy path, full debug detail when something is actually wrong.
	/// Recommended default for production sessions.
	Outliers,
}

impl TraceMode {
	/// Sampler ratio to install on the OTel TracerProvider.
	///
	/// Client code (the video pipeline) does its own sampling decision
	/// per frame — see `pipeline::run_encoding_loop` — so the SDK
	/// sampler is set to `1.0` here for any mode that emits at all.
	/// That way every span we *do* hand to the layer is kept; we don't
	/// double-sample. Sessions / compositor spans inherit this.
	pub fn sampler_ratio(self) -> f64 {
		match self {
			TraceMode::None => 0.0,
			TraceMode::Static(_) | TraceMode::Outliers => 1.0,
		}
	}
}

/// Configuration for the OTel pipeline. Constructed from `[telemetry]` in
/// the config file, or from bench-harness CLI flags.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
	/// OTLP/gRPC endpoint URL (e.g. "http://localhost:4317"). When `None`,
	/// telemetry is disabled — no spans created, no metrics collected,
	/// zero overhead beyond a couple of dead-code branches.
	pub otlp_endpoint: Option<String>,

	/// Optional service name override (default: "moonshine").
	pub service_name: Option<String>,

	/// What kind of trace data to emit on the per-frame hot path. See
	/// `TraceMode` for semantics. Default: `Outliers`.
	pub trace_mode: TraceMode,

	/// Metrics export interval. Defaults to 10s (Prometheus convention).
	pub metric_export_interval: Duration,
}

impl Default for TelemetryConfig {
	fn default() -> Self {
		Self {
			otlp_endpoint: None,
			service_name: None,
			trace_mode: TraceMode::Outliers,
			metric_export_interval: Duration::from_secs(10),
		}
	}
}

/// Held by main(). Drops the OTel pipelines on shutdown so spans/metrics
/// in the batch buffer get flushed. Call `force_flush()` explicitly
/// before returning from main if the program tends to exit faster than
/// the BatchSpanProcessor's scheduled-delay can drain the queue
/// (bench-mode, short tests).
pub struct TelemetryGuard {
	tracer_provider: Option<TracerProvider>,
	meter_provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
	/// Synchronously drain pending spans and metrics through their
	/// exporters. Useful at end of bench mode where the batch processor
	/// would otherwise lose the last few seconds.
	pub fn force_flush(&self) {
		if let Some(tp) = &self.tracer_provider {
			for r in tp.force_flush() {
				if let Err(e) = r {
					tracing::warn!("OTel: tracer flush error: {e}");
				}
			}
		}
		if let Some(mp) = &self.meter_provider {
			if let Err(e) = mp.force_flush() {
				tracing::warn!("OTel: meter flush error: {e}");
			}
		}
	}
}

impl Drop for TelemetryGuard {
	fn drop(&mut self) {
		if let Some(tp) = self.tracer_provider.take() {
			let _ = tp.shutdown();
		}
		if let Some(mp) = self.meter_provider.take() {
			let _ = mp.shutdown();
		}
	}
}

/// Build the resource attributes attached to every export. Using OTel
/// semantic conventions where possible so dashboards from other Rust
/// services can reuse the same field names.
fn build_resource(service_name: &str) -> Resource {
	Resource::new([
		KeyValue::new(semres::SERVICE_NAME, service_name.to_string()),
		KeyValue::new(semres::SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
		KeyValue::new(
			"moonshine.host",
			hostname::get()
				.ok()
				.and_then(|h| h.into_string().ok())
				.unwrap_or_default(),
		),
	])
}

/// Initialize OTel + bridge moonshine's existing `tracing` spans into it.
/// Returns a guard that must be held alive for the program lifetime.
///
/// When `cfg.otlp_endpoint` is `None`, this still installs the local
/// stdout `tracing-subscriber` (so logs work) but skips all OTel pipeline
/// init.
pub fn init(cfg: &TelemetryConfig) -> Result<TelemetryGuard, String> {
	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
	let fmt_layer = tracing_subscriber::fmt::layer();

	let Some(endpoint) = &cfg.otlp_endpoint else {
		// Telemetry off — install only the stdout layer. Force TRACE_MODE
		// to None regardless of what the user asked for, so the pipeline
		// hot path's spike-span branch is dead code (no allocations,
		// no fmt-layer ghost spans going nowhere useful).
		let _ = TRACE_MODE.set(TraceMode::None);
		tracing_subscriber::registry().with(env_filter).with(fmt_layer).init();
		return Ok(TelemetryGuard {
			tracer_provider: None,
			meter_provider: None,
		});
	};

	// Endpoint is set — record the user's chosen mode for the pipeline.
	let _ = TRACE_MODE.set(cfg.trace_mode);

	let service_name = cfg.service_name.clone().unwrap_or_else(|| "moonshine".to_string());
	let resource = build_resource(&service_name);

	// === Tracer provider ===
	// Tail sampling: ParentBased(TraceIdRatioBased(rate)). Caller spans
	// can override per-trace via tracing attributes (`always_sample = true`)
	// when emitting a known-spike frame.
	let exporter = opentelemetry_otlp::SpanExporter::builder()
		.with_tonic()
		.with_endpoint(endpoint)
		.build()
		.map_err(|e| format!("OTel: build span exporter: {e}"))?;

	let sampler_ratio = cfg.trace_mode.sampler_ratio();
	let tracer_provider = TracerProvider::builder()
		.with_resource(resource.clone())
		.with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
			sampler_ratio,
		))))
		.with_batch_exporter(exporter, runtime::Tokio)
		.build();

	let tracer = tracer_provider.tracer(service_name.clone());
	global::set_tracer_provider(tracer_provider.clone());

	// === Meter provider ===
	let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
		.with_tonic()
		.with_endpoint(endpoint)
		.build()
		.map_err(|e| format!("OTel: build metric exporter: {e}"))?;

	let reader = PeriodicReader::builder(metric_exporter, runtime::Tokio)
		.with_interval(cfg.metric_export_interval)
		.build();

	let meter_provider = SdkMeterProvider::builder()
		.with_resource(resource)
		.with_reader(reader)
		.build();
	global::set_meter_provider(meter_provider.clone());

	// === tracing → OTel bridge ===
	// Existing `tracing::info_span!` calls in the pipeline now also emit
	// OTel spans without code changes.
	let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

	tracing_subscriber::registry()
		.with(env_filter)
		.with(fmt_layer)
		.with(otel_layer)
		.init();

	Ok(TelemetryGuard {
		tracer_provider: Some(tracer_provider),
		meter_provider: Some(meter_provider),
	})
}

/// Pre-built metric instruments used by the video pipeline. Cheap to
/// construct (interns into the global meter provider) and lock-free to
/// record into. Held by `VideoPipelineInner` so we don't re-resolve
/// instruments per frame.
///
/// Attributes (codec/hdr/stage) are pre-built once at construction so
/// per-frame recording doesn't allocate or re-stringify. The hot path
/// is just: increment counter / record histogram with a borrowed slice.
pub struct PipelineMetrics {
	pub frames_total: Counter<u64>,
	pub spikes_total: Counter<u64>,
	pub stage_latency_us: Histogram<u64>,
	pub total_latency_us: Histogram<u64>,
	pub encoded_bytes: Histogram<u64>,
	pub dmabuf_cache_size: Gauge<u64>,
}

/// Per-pipeline cached attribute sets. Built once at session start,
/// borrowed on every frame. Keeps the hot path allocation-free.
pub struct FrameAttrs {
	/// `[codec, hdr]` — used for `frames_total`, `spikes_total`,
	/// `total_latency_us`, `encoded_bytes`.
	pub frame: [KeyValue; 2],
	/// `[codec, hdr, stage=<name>]` — one per stage, indexed by `Stage::*`.
	/// Avoids rebuilding the stage attribute string per histogram record.
	pub stages: [(Stage, [KeyValue; 3]); 6],
}

#[derive(Copy, Clone, Debug)]
pub enum Stage {
	ChannelWait,
	Import,
	Convert,
	Encode,
	Packetize,
	Send,
}

impl Stage {
	const fn label(self) -> &'static str {
		match self {
			Stage::ChannelWait => "channel_wait",
			Stage::Import => "import",
			Stage::Convert => "convert",
			Stage::Encode => "encode",
			Stage::Packetize => "packetize",
			Stage::Send => "send",
		}
	}
}

impl FrameAttrs {
	pub fn new(codec: &str, hdr: bool) -> Self {
		let mk_stage = |s: Stage| {
			(
				s,
				[
					KeyValue::new("codec", codec.to_string()),
					KeyValue::new("hdr", hdr),
					KeyValue::new("stage", s.label()),
				],
			)
		};
		Self {
			frame: [KeyValue::new("codec", codec.to_string()), KeyValue::new("hdr", hdr)],
			stages: [
				mk_stage(Stage::ChannelWait),
				mk_stage(Stage::Import),
				mk_stage(Stage::Convert),
				mk_stage(Stage::Encode),
				mk_stage(Stage::Packetize),
				mk_stage(Stage::Send),
			],
		}
	}
}

impl PipelineMetrics {
	pub fn new(meter: &Meter) -> Self {
		Self {
			frames_total: meter.u64_counter("moonshine.frames").build(),
			spikes_total: meter.u64_counter("moonshine.spikes").build(),
			stage_latency_us: meter
				.u64_histogram("moonshine.stage_latency")
				.with_unit("us")
				.with_description("Per-stage frame latency (channel_wait/import/convert/encode/packetize/send)")
				.build(),
			total_latency_us: meter
				.u64_histogram("moonshine.total_latency")
				.with_unit("us")
				.with_description("End-to-end host-processing latency per frame")
				.build(),
			encoded_bytes: meter.u64_histogram("moonshine.encoded_bytes").with_unit("By").build(),
			dmabuf_cache_size: meter
				.u64_gauge("moonshine.dmabuf.cache_size")
				.with_description("Number of cached DMA-BUF imports currently resident")
				.build(),
		}
	}

	/// Record a fully-tagged latency sample. Uses pre-built `FrameAttrs`
	/// so this hot-path call is ~9 atomic ops + 8 histogram records, no
	/// allocations.
	#[inline]
	pub fn record_frame(&self, attrs: &FrameAttrs, sample: &PipelineLatency) {
		self.frames_total.add(1, &attrs.frame);
		self.total_latency_us.record(sample.total_us, &attrs.frame);
		self.encoded_bytes.record(sample.encoded_bytes as u64, &attrs.frame);
		let stage_us = [
			sample.channel_wait_us,
			sample.import_us,
			sample.convert_us,
			sample.encode_us,
			sample.packetize_us,
			sample.send_us,
		];
		for (i, (_, kvs)) in attrs.stages.iter().enumerate() {
			self.stage_latency_us.record(stage_us[i], kvs);
		}
		if sample.total_us > sample.frame_budget_us {
			self.spikes_total.add(1, &attrs.frame);
		}
	}
}

/// Mirror of the existing pipeline `LatencySample` shaped for metric emission.
pub struct PipelineLatency {
	pub channel_wait_us: u64,
	pub import_us: u64,
	pub convert_us: u64,
	pub encode_us: u64,
	pub packetize_us: u64,
	pub send_us: u64,
	pub total_us: u64,
	pub encoded_bytes: usize,
	pub frame_budget_us: u64,
}

// ---- minimal hostname shim so we don't add another dependency just for this
mod hostname {
	use std::ffi::CStr;
	pub fn get() -> std::io::Result<std::ffi::OsString> {
		let mut buf = vec![0u8; 256];
		// SAFETY: gethostname writes a NUL-terminated string into buf.
		let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
		if r != 0 {
			return Err(std::io::Error::last_os_error());
		}
		let cstr = unsafe { CStr::from_ptr(buf.as_ptr() as *const _) };
		Ok(std::ffi::OsString::from(cstr.to_string_lossy().into_owned()))
	}
}
