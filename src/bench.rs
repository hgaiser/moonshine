//! Self-contained pipeline benchmark for moonshine.
//!
//! Runs the compositor + capture + convert + encode pipeline without
//! connecting a Moonlight client. Encoded packets are dropped on the floor;
//! per-frame latency samples are aggregated and reported when the run ends.
//!
//! This bypasses RTSP, the webserver, the control/audio streams, and the
//! UDP video socket entirely. Intended for iterating on pipeline / driver /
//! power-management tuning without a phone in hand.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use clap::Args;
use tokio::sync::{broadcast, mpsc, watch};

use crate::config::{ApplicationConfig, Config};
use crate::session::compositor::{self, frame::HdrModeState, CompositorConfig};
use crate::session::launch_application;
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::video::pipeline::{LatencySample, VideoPipeline};
use crate::session::stream::{VideoChromaSampling, VideoDynamicRange, VideoFormat};
use crate::session::{SessionContext, SessionKeys};

const BENCH_SCOPE: &str = "moonshine-bench";

#[derive(Args, Debug, Clone)]
pub struct BenchArgs {
	/// Duration of the bench run in seconds.
	#[arg(long, default_value_t = 30)]
	pub duration: u64,

	/// Discard frames captured during the first N seconds. Lets first-frame
	/// allocation, XWayland startup, and shader compile spikes settle before
	/// we start recording stats.
	#[arg(long, default_value_t = 2)]
	pub warmup: u64,

	/// Resolution as WIDTHxHEIGHT (e.g. 1920x1080).
	#[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
	pub resolution: (u32, u32),

	/// Frame rate in Hz.
	#[arg(long, default_value_t = 60)]
	pub fps: u32,

	/// Target bitrate in bits per second.
	#[arg(long, default_value_t = 50_000_000)]
	pub bitrate: usize,

	/// Codec: h264, hevc, or av1.
	#[arg(long, default_value = "hevc", value_parser = parse_codec)]
	pub codec: VideoFormat,

	/// Enable HDR (BT.2020 + PQ, 10-bit).
	#[arg(long)]
	pub hdr: bool,

	/// Application title (must match an entry in the config's `[[application]]`
	/// table). The application is launched inside the bench compositor.
	/// Mutually exclusive with a trailing command.
	#[arg(long, conflicts_with = "cmd")]
	pub app: Option<String>,

	/// Inline command to launch in the bench compositor, given after `--`.
	/// Example: `moonshine cfg bench -- /path/to/run.sh -arg1 -arg2`.
	/// Mutually exclusive with --app.
	#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
	pub cmd: Vec<String>,
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
	let (w, h) = s
		.split_once('x')
		.ok_or_else(|| format!("expected WIDTHxHEIGHT, got '{s}'"))?;
	let w: u32 = w.parse().map_err(|e| format!("invalid width: {e}"))?;
	let h: u32 = h.parse().map_err(|e| format!("invalid height: {e}"))?;
	Ok((w, h))
}

/// Resolve which application the bench should launch.
///
/// Either `--app <name>` picks one from the config, or a trailing command
/// (after `--`) provides one inline.
fn resolve_app(config: &Config, args: &BenchArgs) -> Result<ApplicationConfig, ()> {
	if !args.cmd.is_empty() {
		// Use the basename of the executable as the display title rather than
		// the full path (e.g. "glxgears" instead of "/usr/bin/glxgears").
		let title = args
			.cmd
			.first()
			.and_then(|p| std::path::Path::new(p).file_name())
			.and_then(|n| n.to_str())
			.unwrap_or("bench")
			.to_string();
		return Ok(ApplicationConfig {
			title,
			boxart: None,
			command: args.cmd.clone(),
			pre_command: Vec::new(),
			post_command: Vec::new(),
		});
	}
	let Some(name) = args.app.as_deref() else {
		tracing::error!("bench requires either --app <name> or a trailing command after --");
		return Err(());
	};
	config
		.applications
		.iter()
		.find(|a| a.title == name)
		.cloned()
		.ok_or_else(|| {
			tracing::error!(
				"Application '{}' not found in config. Available: {:?}",
				name,
				config.applications.iter().map(|a| &a.title).collect::<Vec<_>>(),
			);
		})
}

fn parse_codec(s: &str) -> Result<VideoFormat, String> {
	match s.to_ascii_lowercase().as_str() {
		"h264" | "avc" => Ok(VideoFormat::H264),
		"h265" | "hevc" => Ok(VideoFormat::Hevc),
		"av1" => Ok(VideoFormat::Av1),
		other => Err(format!("unknown codec '{other}' (expected h264, hevc, or av1)")),
	}
}

/// A LatencySample with the bench-relative timestamp of when it arrived,
/// so we can filter by warmup window.
#[derive(Clone)]
struct TimedSample {
	elapsed: Duration,
	sample: LatencySample,
}

pub async fn run(config: Config, args: BenchArgs, global_shutdown: ShutdownManager<i32>) -> Result<(), ()> {
	let app = resolve_app(&config, &args)?;

	// Session-level OTel span wrapping the whole bench run. Per-frame
	// `frame.encode` spans nest under this, and post-run summary metrics
	// recorded as fields. When `--otlp-endpoint` is set, this lets a
	// dashboard show "this bench run" as a single trace entry to drill
	// into — distinct from real streaming sessions.
	let bench_span = tracing::info_span!(
		"bench.session",
		resolution = format!("{}x{}", args.resolution.0, args.resolution.1),
		fps = args.fps,
		bitrate = args.bitrate,
		codec = ?args.codec,
		hdr = args.hdr,
		duration_s = args.duration,
		warmup_s = args.warmup,
		// filled by report() at end of run
		frames = tracing::field::Empty,
		spikes = tracing::field::Empty,
		total_p50_us = tracing::field::Empty,
		total_p99_us = tracing::field::Empty,
		total_max_us = tracing::field::Empty,
	);
	let _bench_span_guard = bench_span.clone().entered();

	tracing::info!(
		"Starting bench: {}x{} @ {}Hz, {} bps, {:?}, hdr={}, app={:?}, duration={}s, warmup={}s",
		args.resolution.0,
		args.resolution.1,
		args.fps,
		args.bitrate,
		args.codec,
		args.hdr,
		app.command,
		args.duration,
		args.warmup,
	);

	let runtime_dir =
		std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
	let pulse_dir = Path::new(&runtime_dir).join("moonshine-bench/pulse");
	std::fs::create_dir_all(&pulse_dir).map_err(|e| tracing::error!("Failed to create pulse dir: {e}"))?;

	let session_shutdown: ShutdownManager<SessionShutdownReason> = ShutdownManager::new();

	let compositor_config = CompositorConfig {
		width: args.resolution.0,
		height: args.resolution.1,
		refresh_rate: args.fps,
		gpu: config.gpu.clone(),
		hdr: args.hdr,
	};
	let (frame_rx, _input_tx, ready_rx) = compositor::start_compositor(Default::default(), compositor_config, session_shutdown.clone())
		.map_err(|e| tracing::error!("Failed to start compositor: {e}"))?;

	let app_pulse_dir = pulse_dir.clone();
	let app_resolution = args.resolution;
	let app_fps = args.fps;
	let app_shutdown = session_shutdown.clone();
	std::thread::Builder::new()
		.name("bench-app-launcher".to_string())
		.spawn(move || {
			let ready = match ready_rx.recv_timeout(Duration::from_secs(5)) {
				Ok(r) => r,
				Err(e) => {
					tracing::error!("Timed out waiting for compositor ready: {e}");
					let _ = app_shutdown.trigger_shutdown(SessionShutdownReason::CompositorStopped);
					return;
				},
			};

			let session_ctx = SessionContext {
				application: app,
				application_id: 0,
				resolution: app_resolution,
				_refresh_rate: app_fps,
				keys: SessionKeys {
					remote_input_key: vec![0u8; 16],
					remote_input_key_id: 0,
				},
				audio_channels: 2,
				audio_channel_mask: 0,
			};

			match launch_application(&session_ctx, &app_pulse_dir, &ready, BENCH_SCOPE) {
				Ok(mut child) => {
					if let Err(e) = child.wait() {
						tracing::warn!("Failed to wait for application: {e}");
					}
					tracing::info!("Application exited.");
					let _ = app_shutdown.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
				},
				Err(()) => {
					let _ = app_shutdown.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
				},
			}
		})
		.map_err(|e| tracing::error!("Failed to spawn app launcher thread: {e}"))?;

	// Drain the packet channel: the pipeline blocks on packet_tx.blocking_send.
	let (packet_tx, mut packet_rx) = mpsc::channel(128);
	tokio::spawn(async move {
		while let Some(_batch) = packet_rx.recv().await {
			// Discard.
		}
	});

	let (stats_tx, stats_rx) = std::sync::mpsc::channel::<LatencySample>();

	let (_idr_tx, idr_rx) = broadcast::channel::<()>(1);
	let (hdr_metadata_tx, _hdr_metadata_rx) = watch::channel(HdrModeState {
		enabled: args.hdr,
		metadata: None,
	});

	let dynamic_range = if args.hdr {
		VideoDynamicRange::Hdr
	} else {
		VideoDynamicRange::Sdr
	};

	let bench_started = Instant::now();

	// Frame stats collector: timestamps each LatencySample as it arrives so
	// we can filter the warmup window and correlate with GPU samples.
	let stats_collector = std::thread::Builder::new()
		.name("bench-stats-collector".to_string())
		.spawn(move || {
			let mut samples = Vec::with_capacity(4096);
			while let Ok(sample) = stats_rx.recv() {
				samples.push(TimedSample {
					elapsed: bench_started.elapsed(),
					sample,
				});
			}
			samples
		})
		.map_err(|e| tracing::error!("Failed to spawn stats collector: {e}"))?;

	// `pipeline` holds the JoinHandle for the pipeline thread. Drop at end
	// of scope joins the thread; session_shutdown is triggered before that
	// point so the thread exits promptly.
	let pipeline = VideoPipeline::new(
		frame_rx,
		args.resolution.0,
		args.resolution.1,
		args.fps,
		args.bitrate,
		1024, // packet_size
		0,    // minimum_fec_packets
		0,    // fec_percentage
		args.codec,
		dynamic_range,
		VideoChromaSampling::Yuv420,
		1, // max_reference_frames
		None,
		packet_tx,
		idr_rx,
		session_shutdown.clone(),
		hdr_metadata_tx,
		false, // log_frame_spikes (bench has its own end-of-run report)
		None,  // log_stage_summary_interval (bench has its own end-of-run report)
		Some(stats_tx),
	)
	.map_err(|()| tracing::error!("Failed to start video pipeline"))?;

	let session_done = session_shutdown.wait_shutdown_complete();
	tokio::select! {
		_ = tokio::time::sleep(Duration::from_secs(args.duration)) => {
			tracing::info!("Bench duration elapsed, shutting down.");
		},
		_ = global_shutdown.wait_shutdown_triggered() => {
			tracing::info!("Global shutdown triggered, stopping bench.");
		},
		_ = session_done => {
			tracing::info!("Session shutdown completed before duration elapsed.");
		},
	}

	let elapsed = bench_started.elapsed();

	let _ = session_shutdown.trigger_shutdown(SessionShutdownReason::UserStopped);
	let _ = tokio::time::timeout(Duration::from_secs(5), session_shutdown.wait_shutdown_complete()).await;

	let _ = Command::new("systemctl")
		.args(["--user", "stop", &format!("{BENCH_SCOPE}.scope")])
		.status();

	// Join the pipeline thread before waiting on the stats collector.
	// The pipeline thread holds stats_tx; dropping it closes the channel
	// and lets the collector's recv() loop terminate naturally.
	// spawn_blocking so the async runtime isn't stalled by the join.
	let _ = tokio::task::spawn_blocking(move || drop(pipeline)).await;

	// stats_tx is held by the pipeline thread; once that thread exits (due to
	// session shutdown) the channel closes and the collector's recv() returns
	// Err, ending the loop. We wrap the join in spawn_blocking so the async
	// timeout can fire if the pipeline thread hangs (e.g. stuck in a Vulkan
	// call) and we still get to print whatever samples were collected so far.
	let timed_samples = match tokio::time::timeout(
		Duration::from_secs(5),
		tokio::task::spawn_blocking(move || stats_collector.join().unwrap_or_else(|_| Vec::new())),
	)
	.await
	{
		Ok(Ok(samples)) => samples,
		Ok(Err(_)) => {
			tracing::warn!("Stats collector thread panicked; report will be empty.");
			Vec::new()
		},
		Err(_) => {
			tracing::warn!("Timed out waiting for stats collector; report may be incomplete.");
			Vec::new()
		},
	};

	report(&timed_samples, elapsed, &args, &bench_span);

	Ok(())
}

/// Return the q-th percentile of a **sorted** slice of microsecond values.
/// `q` must be in (0.0, 1.0]. Uses the "ceiling" formula so that p99 on a
/// 100-element slice picks index 99 (the last element) rather than 98.
fn percentile_sorted(sorted: &[u64], q: f64) -> u64 {
	debug_assert!(!sorted.is_empty());
	let idx = ((sorted.len() as f64 * q).ceil() as usize)
		.saturating_sub(1)
		.min(sorted.len() - 1);
	sorted[idx]
}

fn report(timed: &[TimedSample], elapsed: Duration, args: &BenchArgs, bench_span: &tracing::Span) {
	let warmup = Duration::from_secs(args.warmup);
	let warm_timed: Vec<&TimedSample> = timed.iter().filter(|s| s.elapsed >= warmup).collect();
	let frame_samples: Vec<&LatencySample> = warm_timed.iter().map(|s| &s.sample).collect();

	println!();
	println!("======================================================================");
	println!(" moonshine bench report");
	println!("======================================================================");
	println!(
		" config:    {}x{} @ {}Hz, {} bps, {:?}, hdr={}",
		args.resolution.0, args.resolution.1, args.fps, args.bitrate, args.codec, args.hdr,
	);
	println!(
		" duration:  {:.2}s elapsed (target {}s, warmup {}s, {} frames discarded)",
		elapsed.as_secs_f64(),
		args.duration,
		args.warmup,
		timed.len() - warm_timed.len(),
	);

	if frame_samples.is_empty() {
		println!(" frames:    0 — no samples after warmup");
		println!("======================================================================");
		return;
	}

	let n = frame_samples.len();
	let total_bytes: usize = frame_samples.iter().map(|s| s.encoded_bytes).sum();
	let key_frames = frame_samples.iter().filter(|s| s.is_key_frame).count();
	let frame_interval_us = 1_000_000_u128 / args.fps as u128;
	let measured_window = elapsed.saturating_sub(warmup).as_secs_f64().max(0.001);
	let observed_fps = n as f64 / measured_window;
	let observed_bitrate = (total_bytes as f64 * 8.0 / measured_window) as u64;
	let spike_indices: Vec<usize> = frame_samples
		.iter()
		.enumerate()
		.filter(|(_, s)| s.total.as_micros() > frame_interval_us)
		.map(|(i, _)| i)
		.collect();

	println!(" frames:    {n} ({key_frames} key)  observed_fps={:.2}", observed_fps,);
	println!(
		" bitrate:   {} bps observed (target {} bps)",
		observed_bitrate, args.bitrate,
	);
	println!(
		" spikes:    {} frames > {}us frame interval ({:.1}%)",
		spike_indices.len(),
		frame_interval_us,
		100.0 * spike_indices.len() as f64 / n as f64,
	);

	// Fill the bench session span with summary attributes so dashboards
	// can show "this bench run had p99=X / spikes=Y" without re-aggregating.
	let mut totals_us: Vec<u64> = frame_samples.iter().map(|s| s.total.as_micros() as u64).collect();
	totals_us.sort_unstable();
	bench_span.record("frames", n as u64);
	bench_span.record("spikes", spike_indices.len() as u64);
	bench_span.record("total_p50_us", percentile_sorted(&totals_us, 0.50));
	bench_span.record("total_p99_us", percentile_sorted(&totals_us, 0.99));
	bench_span.record("total_max_us", *totals_us.last().unwrap());

	println!();
	println!(" stage         min     p50     p95     p99     max     (microseconds)");
	println!(" -----         ---     ---     ---     ---     ---");
	print_stage("channel_wait", frame_samples.iter().map(|s| s.channel_wait));
	print_stage("import      ", frame_samples.iter().map(|s| s.import));
	print_stage("convert     ", frame_samples.iter().map(|s| s.convert));
	print_stage("encode      ", frame_samples.iter().map(|s| s.encode));
	print_stage("packetize   ", frame_samples.iter().map(|s| s.packetize));
	print_stage("send        ", frame_samples.iter().map(|s| s.send));
	print_stage("total       ", frame_samples.iter().map(|s| s.total));

	report_worst_spikes(&warm_timed, frame_interval_us);

	println!("======================================================================");
}

fn print_stage(name: &str, values: impl Iterator<Item = Duration>) {
	let mut us: Vec<u64> = values.map(|d| d.as_micros() as u64).collect();
	if us.is_empty() {
		println!(" {name}  (no data)");
		return;
	}
	us.sort_unstable();
	println!(
		" {name}  {:>6}  {:>6}  {:>6}  {:>6}  {:>6}",
		us[0],
		percentile_sorted(&us, 0.50),
		percentile_sorted(&us, 0.95),
		percentile_sorted(&us, 0.99),
		us[us.len() - 1],
	);
}

/// Print up to 10 worst spikes (frames over the frame budget) for the run.
/// `timed` must already be filtered to exclude the warmup window.
fn report_worst_spikes(timed: &[&TimedSample], frame_interval_us: u128) {
	let mut spikes: Vec<&&TimedSample> = timed
		.iter()
		.filter(|s| s.sample.total.as_micros() > frame_interval_us)
		.collect();
	if spikes.is_empty() {
		return;
	}
	spikes.sort_unstable_by_key(|s| std::cmp::Reverse(s.sample.total));
	spikes.truncate(10);

	println!();
	println!(" worst spikes (frame >{}us):", frame_interval_us);
	println!("    t (s)   total (us)   convert (us)   encode (us)");
	for s in spikes {
		println!(
			"   {:>6.2}     {:>8}      {:>9}     {:>9}",
			s.elapsed.as_secs_f64(),
			s.sample.total.as_micros(),
			s.sample.convert.as_micros(),
			s.sample.encode.as_micros(),
		);
	}
}
