//! Self-contained pipeline benchmark for moonshine.
//!
//! Runs the compositor + capture + convert + encode pipeline without
//! connecting a Moonlight client. Encoded packets are dropped on the floor;
//! per-frame latency samples are aggregated and reported when the run ends.
//!
//! This bypasses RTSP, the webserver, the control/audio streams, and the
//! UDP video socket entirely. Intended for iterating on pipeline / driver /
//! power-management tuning without a phone in hand.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
const AMD_VENDOR_ID: &str = "0x1002";

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

	/// GPU stat sampling interval in milliseconds. 0 disables sampling.
	#[arg(long, default_value_t = 100)]
	pub gpu_stats_interval_ms: u64,

	/// DRM card name to sample (e.g. `card1`). Defaults to auto-detecting the
	/// first AMD card under /sys/class/drm.
	#[arg(long)]
	pub gpu_stats_card: Option<String>,

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
		let title = args.cmd.first().cloned().unwrap_or_else(|| "bench".to_string());
		return Ok(ApplicationConfig {
			title,
			boxart: None,
			command: args.cmd.clone(),
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
/// so we can filter by warmup window and correlate with GPU samples.
#[derive(Clone)]
struct TimedSample {
	elapsed: Duration,
	sample: LatencySample,
}

/// A GPU power state observation at a point in time.
#[derive(Clone, Copy)]
struct GpuSample {
	elapsed: Duration,
	sclk_mhz: u32,
	busy_pct: u8,
}

/// Find the first AMD card under `/sys/class/drm`, ignoring connector entries
/// like `card1-DP-1`.
fn auto_detect_amd_card() -> Option<PathBuf> {
	let entries = std::fs::read_dir("/sys/class/drm").ok()?;
	for entry in entries.flatten() {
		let file_name = entry.file_name();
		let name = file_name.to_string_lossy();
		// Match `card<N>` exactly — skip connector entries like `card1-DP-1`.
		if !name.starts_with("card") || !name[4..].chars().all(|c| c.is_ascii_digit()) {
			continue;
		}
		let vendor_path = entry.path().join("device/vendor");
		if let Ok(vendor) = std::fs::read_to_string(&vendor_path) {
			if vendor.trim() == AMD_VENDOR_ID {
				return Some(entry.path());
			}
		}
	}
	None
}

/// Read `pp_dpm_sclk` and return the currently active clock in MHz (the line
/// containing `*`). Returns None on any parse error.
fn read_active_sclk_mhz(path: &Path) -> Option<u32> {
	let content = std::fs::read_to_string(path).ok()?;
	for line in content.lines() {
		if !line.contains('*') {
			continue;
		}
		// Lines look like: "1: 1330Mhz *"
		let after_colon = line.split_once(':')?.1.trim().trim_end_matches('*').trim();
		// Strip trailing "Mhz" / "MHz".
		let mhz = after_colon.trim_end_matches(|c: char| c.is_alphabetic()).trim();
		return mhz.parse().ok();
	}
	None
}

/// Read `gpu_busy_percent` and return the value (0-100).
fn read_busy_percent(path: &Path) -> Option<u8> {
	std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub async fn run(config: Config, args: BenchArgs, global_shutdown: ShutdownManager<i32>) -> Result<(), ()> {
	let app = resolve_app(&config, &args)?;

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

	// Resolve the GPU sysfs card for stats sampling. None means "don't sample".
	let gpu_card_path: Option<PathBuf> = if args.gpu_stats_interval_ms == 0 {
		None
	} else if let Some(name) = args.gpu_stats_card.as_deref() {
		Some(PathBuf::from("/sys/class/drm").join(name))
	} else {
		match auto_detect_amd_card() {
			Some(p) => {
				tracing::info!("GPU stats: auto-detected {}", p.display());
				Some(p)
			},
			None => {
				tracing::warn!("GPU stats: no AMD card found, sampling disabled");
				None
			},
		}
	};

	let session_shutdown: ShutdownManager<SessionShutdownReason> = ShutdownManager::new();

	let compositor_config = CompositorConfig {
		width: args.resolution.0,
		height: args.resolution.1,
		refresh_rate: args.fps,
		gpu: config.gpu.clone(),
		hdr: args.hdr,
	};
	let (frame_rx, _input_tx, ready_rx) = compositor::start_compositor(compositor_config, session_shutdown.clone())
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

	// GPU sampler: reads sysfs at a fixed interval. Stops when `gpu_stop` is
	// flipped (we use a flag instead of session_shutdown so the bench can
	// stop sampling slightly before tearing the session down, leaving us
	// known-good samples for the report).
	let gpu_stop = Arc::new(AtomicBool::new(false));
	let gpu_sampler = gpu_card_path.as_ref().map(|card_path| {
		let card_path = card_path.clone();
		let interval = Duration::from_millis(args.gpu_stats_interval_ms);
		let stop = gpu_stop.clone();
		std::thread::Builder::new()
			.name("bench-gpu-sampler".to_string())
			.spawn(move || {
				let sclk_path = card_path.join("device/pp_dpm_sclk");
				let busy_path = card_path.join("device/gpu_busy_percent");
				let mut samples = Vec::with_capacity(1024);
				while !stop.load(Ordering::Relaxed) {
					let elapsed = bench_started.elapsed();
					let sclk_mhz = read_active_sclk_mhz(&sclk_path).unwrap_or(0);
					let busy_pct = read_busy_percent(&busy_path).unwrap_or(0);
					samples.push(GpuSample {
						elapsed,
						sclk_mhz,
						busy_pct,
					});
					std::thread::sleep(interval);
				}
				samples
			})
			.expect("Failed to spawn GPU sampler")
	});

	let _pipeline = VideoPipeline::new(
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
		false, // log_frame_spikes (we have our own report)
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

	gpu_stop.store(true, Ordering::Relaxed);

	let _ = session_shutdown.trigger_shutdown(SessionShutdownReason::UserStopped);
	let _ = tokio::time::timeout(Duration::from_secs(5), session_shutdown.wait_shutdown_complete()).await;

	let _ = Command::new("systemctl")
		.args(["--user", "stop", &format!("{BENCH_SCOPE}.scope")])
		.status();

	// stats_tx is held by the pipeline thread; once that thread exits (due to
	// session shutdown) the channel closes and the collector's recv() returns
	// Err, ending the loop.
	let timed_samples = stats_collector.join().unwrap_or_else(|_| Vec::new());
	let gpu_samples = gpu_sampler.and_then(|h| h.join().ok()).unwrap_or_default();

	report(&timed_samples, &gpu_samples, elapsed, &args);

	Ok(())
}

fn report(timed: &[TimedSample], gpu: &[GpuSample], elapsed: Duration, args: &BenchArgs) {
	let warmup = Duration::from_secs(args.warmup);
	let frame_samples: Vec<&LatencySample> = timed
		.iter()
		.filter(|s| s.elapsed >= warmup)
		.map(|s| &s.sample)
		.collect();
	let gpu_in_window: Vec<&GpuSample> = gpu.iter().filter(|s| s.elapsed >= warmup).collect();

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
		timed.len() - frame_samples.len(),
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

	if !gpu_in_window.is_empty() {
		report_gpu(&gpu_in_window);
		report_spike_correlation(timed, gpu, args.warmup, frame_interval_us);
	}

	println!("======================================================================");
}

fn print_stage(name: &str, values: impl Iterator<Item = Duration>) {
	let mut us: Vec<u64> = values.map(|d| d.as_micros() as u64).collect();
	if us.is_empty() {
		println!(" {name}  (no data)");
		return;
	}
	us.sort_unstable();
	let pick = |q: f64| us[((us.len() as f64 * q) as usize).min(us.len() - 1)];
	println!(
		" {name}  {:>6}  {:>6}  {:>6}  {:>6}  {:>6}",
		us[0],
		pick(0.50),
		pick(0.95),
		pick(0.99),
		us[us.len() - 1],
	);
}

fn report_gpu(samples: &[&GpuSample]) {
	let mut sclk: Vec<u32> = samples.iter().map(|s| s.sclk_mhz).collect();
	let mut busy: Vec<u8> = samples.iter().map(|s| s.busy_pct).collect();
	sclk.sort_unstable();
	busy.sort_unstable();
	let pick_u32 = |v: &[u32], q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
	let pick_u8 = |v: &[u8], q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];

	println!();
	println!(
		" gpu          min     p50     p95     p99     max     ({} samples)",
		samples.len()
	);
	println!(" ---          ---     ---     ---     ---     ---");
	println!(
		" sclk MHz   {:>6}  {:>6}  {:>6}  {:>6}  {:>6}",
		sclk[0],
		pick_u32(&sclk, 0.50),
		pick_u32(&sclk, 0.95),
		pick_u32(&sclk, 0.99),
		sclk[sclk.len() - 1],
	);
	println!(
		" busy %     {:>6}  {:>6}  {:>6}  {:>6}  {:>6}",
		busy[0],
		pick_u8(&busy, 0.50),
		pick_u8(&busy, 0.95),
		pick_u8(&busy, 0.99),
		busy[busy.len() - 1],
	);
}

/// Print up to 10 worst spikes with the GPU power state at that moment, so we
/// can eyeball whether spikes correlate with low-clock or low-busy states.
fn report_spike_correlation(timed: &[TimedSample], gpu: &[GpuSample], warmup_secs: u64, frame_interval_us: u128) {
	if gpu.is_empty() {
		return;
	}
	let warmup = Duration::from_secs(warmup_secs);
	let mut spikes: Vec<&TimedSample> = timed
		.iter()
		.filter(|s| s.elapsed >= warmup && s.sample.total.as_micros() > frame_interval_us)
		.collect();
	if spikes.is_empty() {
		return;
	}
	spikes.sort_unstable_by_key(|s| std::cmp::Reverse(s.sample.total));
	spikes.truncate(10);

	println!();
	println!(
		" worst spikes (frame >{}us with nearest GPU sample):",
		frame_interval_us
	);
	println!("    t (s)   total (us)   convert (us)   encode (us)   sclk MHz   busy %");
	for s in spikes {
		let nearest = gpu
			.iter()
			.min_by_key(|g| g.elapsed.abs_diff(s.elapsed))
			.copied()
			.unwrap();
		println!(
			"   {:>6.2}     {:>8}      {:>9}     {:>9}     {:>6}    {:>5}",
			s.elapsed.as_secs_f64(),
			s.sample.total.as_micros(),
			s.sample.convert.as_micros(),
			s.sample.encode.as_micros(),
			nearest.sclk_mhz,
			nearest.busy_pct,
		);
	}
}
