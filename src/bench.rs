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

use crate::config::Config;
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
	#[arg(long)]
	pub app: String,
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
	let (w, h) = s
		.split_once('x')
		.ok_or_else(|| format!("expected WIDTHxHEIGHT, got '{s}'"))?;
	let w: u32 = w.parse().map_err(|e| format!("invalid width: {e}"))?;
	let h: u32 = h.parse().map_err(|e| format!("invalid height: {e}"))?;
	Ok((w, h))
}

fn parse_codec(s: &str) -> Result<VideoFormat, String> {
	match s.to_ascii_lowercase().as_str() {
		"h264" | "avc" => Ok(VideoFormat::H264),
		"h265" | "hevc" => Ok(VideoFormat::Hevc),
		"av1" => Ok(VideoFormat::Av1),
		other => Err(format!("unknown codec '{other}' (expected h264, hevc, or av1)")),
	}
}

pub async fn run(config: Config, args: BenchArgs, global_shutdown: ShutdownManager<i32>) -> Result<(), ()> {
	tracing::info!(
		"Starting bench: {}x{} @ {}Hz, {} bps, {:?}, hdr={}, app={}, duration={}s",
		args.resolution.0,
		args.resolution.1,
		args.fps,
		args.bitrate,
		args.codec,
		args.hdr,
		args.app,
		args.duration,
	);

	let app = config
		.applications
		.iter()
		.find(|a| a.title == args.app)
		.cloned()
		.ok_or_else(|| {
			tracing::error!(
				"Application '{}' not found in config. Available: {:?}",
				args.app,
				config.applications.iter().map(|a| &a.title).collect::<Vec<_>>(),
			);
		})?;

	// Pulse socket dir — we don't run a server, so apps will see connection
	// refused on PULSE_SERVER and fall back to silence. This avoids polluting
	// the host's PulseAudio.
	let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
		.unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
	let pulse_dir = Path::new(&runtime_dir).join("moonshine-bench/pulse");
	std::fs::create_dir_all(&pulse_dir).map_err(|e| tracing::error!("Failed to create pulse dir: {e}"))?;

	// Session-scoped shutdown so we can tear down the compositor + pipeline
	// independently of the global CTRL+C handler.
	let session_shutdown: ShutdownManager<SessionShutdownReason> = ShutdownManager::new();

	// Compositor.
	let compositor_config = CompositorConfig {
		width: args.resolution.0,
		height: args.resolution.1,
		refresh_rate: args.fps,
		gpu: config.gpu.clone(),
		hdr: args.hdr,
	};
	let (frame_rx, _input_tx, ready_rx) = compositor::start_compositor(compositor_config, session_shutdown.clone())
		.map_err(|e| tracing::error!("Failed to start compositor: {e}"))?;

	// Launch the application on a worker thread once the compositor is ready.
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
					// If the app exits before the bench duration is up, end the
					// run early — no point capturing an empty compositor.
					let _ = app_shutdown.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
				},
				Err(()) => {
					let _ = app_shutdown.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
				},
			}
		})
		.map_err(|e| tracing::error!("Failed to spawn app launcher thread: {e}"))?;

	// Drain the packet channel: the pipeline blocks on packet_tx.blocking_send,
	// so we need a real receiver, but we don't care about the bytes.
	let (packet_tx, mut packet_rx) = mpsc::channel(128);
	tokio::spawn(async move {
		while let Some(_batch) = packet_rx.recv().await {
			// Discard.
		}
	});

	// Stats sink: bench gathers per-frame samples here and reports at the end.
	let (stats_tx, stats_rx) = std::sync::mpsc::channel::<LatencySample>();

	// IDR + HDR plumbing: bench drives neither, just creates the channels the
	// pipeline expects.
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

	// Run for the configured duration, or until something else triggers a
	// shutdown (CTRL+C, app exits, compositor crashes).
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

	// Tear down the session.
	let _ = session_shutdown.trigger_shutdown(SessionShutdownReason::UserStopped);
	let _ = tokio::time::timeout(Duration::from_secs(5), session_shutdown.wait_shutdown_complete()).await;

	// Stop the systemd scope so the launched application is killed.
	let _ = Command::new("systemctl")
		.args(["--user", "stop", &format!("{BENCH_SCOPE}.scope")])
		.status();

	// Drain remaining stats samples and report.
	let mut samples: Vec<LatencySample> = Vec::new();
	while let Ok(sample) = stats_rx.try_recv() {
		samples.push(sample);
	}

	report(&samples, elapsed, &args);

	Ok(())
}

fn report(samples: &[LatencySample], elapsed: Duration, args: &BenchArgs) {
	println!();
	println!("======================================================================");
	println!(" moonshine bench report");
	println!("======================================================================");
	println!(" config:    {}x{} @ {}Hz, {} bps, {:?}, hdr={}",
		args.resolution.0, args.resolution.1, args.fps, args.bitrate, args.codec, args.hdr);
	println!(" duration:  {:.2}s (target {}s)", elapsed.as_secs_f64(), args.duration);

	if samples.is_empty() {
		println!(" frames:    0 — no samples collected");
		println!("======================================================================");
		return;
	}

	let n = samples.len();
	let total_bytes: usize = samples.iter().map(|s| s.encoded_bytes).sum();
	let key_frames = samples.iter().filter(|s| s.is_key_frame).count();
	let frame_interval_us = 1_000_000_u128 / args.fps as u128;
	let spikes = samples
		.iter()
		.filter(|s| s.total.as_micros() > frame_interval_us)
		.count();
	let observed_fps = n as f64 / elapsed.as_secs_f64();
	let observed_bitrate = (total_bytes as f64 * 8.0 / elapsed.as_secs_f64()) as u64;

	println!(" frames:    {n} ({key_frames} key)  observed_fps={:.2}", observed_fps);
	println!(" bitrate:   {} bps observed (target {} bps)", observed_bitrate, args.bitrate);
	println!(" spikes:    {} frames > {}us frame interval ({:.1}%)",
		spikes, frame_interval_us, 100.0 * spikes as f64 / n as f64);
	println!();
	println!(" stage         min     p50     p95     p99     max     (microseconds)");
	println!(" -----         ---     ---     ---     ---     ---");

	print_stage("channel_wait", samples.iter().map(|s| s.channel_wait));
	print_stage("import      ", samples.iter().map(|s| s.import));
	print_stage("convert     ", samples.iter().map(|s| s.convert));
	print_stage("encode      ", samples.iter().map(|s| s.encode));
	print_stage("packetize   ", samples.iter().map(|s| s.packetize));
	print_stage("send        ", samples.iter().map(|s| s.send));
	print_stage("total       ", samples.iter().map(|s| s.total));

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

