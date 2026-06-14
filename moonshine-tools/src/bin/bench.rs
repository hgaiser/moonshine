use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use clap::Parser;
use moonshine_core::config::ApplicationConfig;
use moonshine_core::session::compositor::CompositorConfig;
use moonshine_core::session::manager::SessionManager;
use moonshine_core::session::stream::audio::AudioChannels;
use moonshine_core::session::stream::audio::AudioConfig;
use moonshine_core::session::stream::audio::AudioStreamConfig;
use moonshine_core::session::stream::audio::AudioStreamContext;
use moonshine_core::session::stream::control::ControlStreamConfig;
use moonshine_core::session::stream::video::FrameStats;
use moonshine_core::session::stream::video::VideoChromaSampling;
use moonshine_core::session::stream::video::VideoDynamicRange;
use moonshine_core::session::stream::video::VideoFormat;
use moonshine_core::session::stream::video::VideoStreamConfig;
use moonshine_core::session::stream::video::VideoStreamContext;
use moonshine_core::session::SessionContext;
use moonshine_core::session::SessionKeyData;
use moonshine_core::session::SessionKeys;
use moonshine_core::ShutdownReason;
use tokio::signal;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "moonshine-bench", about = "Benchmark Moonshine's encoding pipeline")]
struct Args {
	/// Command to run (application to spawn).
	command: Vec<String>,

	/// Stream resolution (WxH).
	#[arg(long, default_value = "1920x1080")]
	resolution: String,

	/// Target FPS.
	#[arg(long, default_value_t = 60)]
	fps: u32,

	/// Target bitrate in bits per second.
	#[arg(long, default_value_t = 20_000_000)]
	bitrate: usize,

	/// Video codec.
	#[arg(long, default_value = "h264", value_parser = ["h264", "hevc", "av1"])]
	codec: String,

	/// Seconds to run before stopping (0 = run until Ctrl+C).
	#[arg(long, default_value_t = 0)]
	duration: u64,

	/// Seconds to discard before recording stats (warmup period).
	#[arg(long, default_value_t = 4)]
	warmup: u64,

	/// Enable HDR mode.
	#[arg(long)]
	hdr: bool,

	/// Print per-frame stats to stderr instead of periodic summary.
	#[arg(long)]
	verbose: bool,
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
	let parts: Vec<&str> = s.split('x').collect();
	if parts.len() != 2 {
		return Err("Invalid resolution format, expected WxH (e.g. 1920x1080)".to_string());
	}
	let w = parts[0].parse::<u32>().map_err(|e| format!("Invalid width: {}", e))?;
	let h = parts[1].parse::<u32>().map_err(|e| format!("Invalid height: {}", e))?;
	Ok((w, h))
}

fn parse_codec(s: &str) -> VideoFormat {
	match s {
		"h264" => VideoFormat::H264,
		"hevc" => VideoFormat::Hevc,
		"av1" => VideoFormat::Av1,
		_ => unreachable!(),
	}
}

struct StatsAccumulator {
	count: u64,
	total_us: u128,
	channel_wait_us: u128,
	import_us: u128,
	convert_us: u128,
	encode_us: u128,
	packetize_us: u128,
	send_us: u128,
	min_total_us: u64,
	max_total_us: u64,
	min_encode_us: u64,
	max_encode_us: u64,
	key_frames: u64,
	encoded_bytes: u64,
	start: Instant,
	last_print: Instant,
}

impl StatsAccumulator {
	fn new() -> Self {
		let now = Instant::now();
		Self {
			count: 0,
			total_us: 0,
			channel_wait_us: 0,
			import_us: 0,
			convert_us: 0,
			encode_us: 0,
			packetize_us: 0,
			send_us: 0,
			min_total_us: u64::MAX,
			max_total_us: 0,
			min_encode_us: u64::MAX,
			max_encode_us: 0,
			key_frames: 0,
			encoded_bytes: 0,
			start: now,
			last_print: now,
		}
	}

	fn add(&mut self, stats: &FrameStats) {
		self.count += 1;
		let total = stats.total.as_micros() as u64;
		let encode = stats.encode.as_micros() as u64;
		self.total_us += total as u128;
		self.channel_wait_us += stats.channel_wait.as_micros();
		self.import_us += stats.import.as_micros();
		self.convert_us += stats.convert.as_micros();
		self.encode_us += encode as u128;
		self.packetize_us += stats.packetize.as_micros();
		self.send_us += stats.send.as_micros();
		self.min_total_us = self.min_total_us.min(total);
		self.max_total_us = self.max_total_us.max(total);
		self.min_encode_us = self.min_encode_us.min(encode);
		self.max_encode_us = self.max_encode_us.max(encode);
		if stats.is_key_frame {
			self.key_frames += 1;
		}
		self.encoded_bytes += stats.encoded_bytes as u64;
	}

	fn print_summary(&self, label: &str) {
		if self.count == 0 {
			return;
		}
		let elapsed = self.start.elapsed().as_secs_f64();
		let fps = self.count as f64 / elapsed;
		let avg_total = self.total_us as f64 / self.count as f64;
		let avg_encode = self.encode_us as f64 / self.count as f64;
		let avg_channel_wait = self.channel_wait_us as f64 / self.count as f64;
		let avg_import = self.import_us as f64 / self.count as f64;
		let avg_convert = self.convert_us as f64 / self.count as f64;
		let avg_packetize = self.packetize_us as f64 / self.count as f64;
		let avg_send = self.send_us as f64 / self.count as f64;
		let mbps = self.encoded_bytes as f64 * 8.0 / elapsed / 1_000_000.0;

		tracing::info!("{} [{} frames, {:.1} fps, {:.2} Mbps]", label, self.count, fps, mbps);
		tracing::info!(
			"  total:    avg={avg_total:.0}us  min={}us  max={}us",
			self.min_total_us,
			self.max_total_us
		);
		tracing::info!(
			"  encode:   avg={avg_encode:.0}us  min={}us  max={}us",
			self.min_encode_us,
			self.max_encode_us
		);
		tracing::info!(
			"  breakdown: ch_wait={avg_channel_wait:.0}us  import={avg_import:.0}us  convert={avg_convert:.0}us  pkt={avg_packetize:.0}us  send={avg_send:.0}us",
		);
		tracing::info!("  key_frames: {}", self.key_frames);
	}

	fn print_and_reset_interval(&mut self) -> Self {
		self.print_summary("Interval");
		let now = Instant::now();
		Self {
			count: 0,
			total_us: 0,
			channel_wait_us: 0,
			import_us: 0,
			convert_us: 0,
			encode_us: 0,
			packetize_us: 0,
			send_us: 0,
			min_total_us: u64::MAX,
			max_total_us: 0,
			min_encode_us: u64::MAX,
			max_encode_us: 0,
			key_frames: 0,
			encoded_bytes: 0,
			start: now,
			last_print: now,
		}
	}
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer())
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
		.init();

	let args = Args::parse();

	if args.command.is_empty() {
		tracing::error!("No command provided. Usage: moonshine-bench [OPTIONS] <COMMAND>");
		std::process::exit(1);
	}

	let (width, height) = parse_resolution(&args.resolution).unwrap_or_else(|e| {
		tracing::error!("Error: {}", e);
		std::process::exit(1);
	});
	let video_format = parse_codec(&args.codec);

	tracing::info!("Starting Moonshine benchmark");
	tracing::info!("  command:    {}", args.command.join(" "));
	tracing::info!("  resolution: {}x{}", width, height);
	tracing::info!("  fps:        {}", args.fps);
	tracing::info!("  bitrate:    {} bps", args.bitrate);
	tracing::info!("  codec:      {}", args.codec);
	tracing::info!("  hdr:        {}", args.hdr);
	let duration_str = if args.duration == 0 {
		"infinite".to_string()
	} else {
		args.duration.to_string()
	};
	tracing::info!("  duration:   {}s", duration_str);
	tracing::info!("  warmup:     {}s", args.warmup);

	let shutdown = ShutdownManager::<ShutdownReason>::new();
	let session_manager = SessionManager::new(
		CompositorConfig::default(),
		VideoStreamConfig::default(),
		AudioStreamConfig { port: 0 },
		ControlStreamConfig {
			port: 0,
			..Default::default()
		},
		"127.0.0.1".to_string(),
		60,
		shutdown.clone(),
	)
	.map_err(|_| "Failed to create session manager")?;

	let mut stats_rx = session_manager.bench_stats_receiver();

	let app_config = ApplicationConfig {
		title: "bench".to_string(),
		command: args.command.clone(),
		..Default::default()
	};

	let session_ctx = SessionContext {
		application: app_config,
		application_id: 1,
		resolution: (width, height),
		refresh_rate: args.fps,
		keys: SessionKeys::Keys(SessionKeyData {
			remote_input_key: vec![0u8; 16],
			remote_input_key_id: 0,
		}),
		audio_channels: AudioChannels::Stereo,
		audio_channel_mask: 0x3,
		hdr: args.hdr,
	};

	tracing::info!("Initializing session...");
	session_manager
		.initialize_session(session_ctx)
		.await
		.map_err(|_| "Failed to initialize session")?;

	tracing::info!("Launching session (compositor + app)...");
	session_manager
		.launch_session()
		.await
		.map_err(|_| "Failed to launch session")?;

	let video_ctx = VideoStreamContext {
		width,
		height,
		fps: args.fps,
		packet_size: 1400,
		bitrate: args.bitrate,
		minimum_fec_packets: 2,
		qos: false,
		video_format,
		dynamic_range: if args.hdr {
			VideoDynamicRange::Hdr
		} else {
			VideoDynamicRange::Sdr
		},
		chroma_sampling_type: VideoChromaSampling::Yuv420,
		max_reference_frames: 1,
		encrypt_video: false,
	};

	let audio_ctx = AudioStreamContext {
		packet_duration_ms: 20,
		qos: false,
		audio_config: AudioConfig::default(),
		encrypt_audio: false,
	};

	tracing::info!("Setting stream contexts...");
	session_manager
		.set_stream_context(video_ctx, audio_ctx)
		.await
		.map_err(|_| "Failed to set stream context")?;

	tracing::info!("Starting session streams...");
	session_manager
		.start_session()
		.await
		.map_err(|_| "Failed to start session")?;

	tracing::info!("Triggering video and audio pipelines...");
	session_manager.trigger_streams_start().await;

	tracing::info!("Session active. Collecting stats...");

	let warmup_deadline = Instant::now() + Duration::from_secs(args.warmup);
	let mut accum = StatsAccumulator::new();
	let mut total = StatsAccumulator::new();
	let mut warned_no_frames = false;

	let duration_deadline = if args.duration > 0 {
		Some(Instant::now() + Duration::from_secs(args.duration))
	} else {
		None
	};

	loop {
		let duration_remaining = duration_deadline
			.map(|d| d.saturating_duration_since(Instant::now()))
			.unwrap_or(Duration::from_secs(86400 * 365 * 100)); // ~100 years = no limit
		tokio::select! {
			biased;
			result = stats_rx.recv() => {
				match result {
					Ok(stats) => {
						if Instant::now() >= warmup_deadline {
							if args.verbose {
								tracing::info!(
									"frame: total={}us encode={}us import={}us convert={}us pkt={}us send={}us ch_wait={}us {} {}b",
									stats.total.as_micros(),
									stats.encode.as_micros(),
									stats.import.as_micros(),
									stats.convert.as_micros(),
									stats.packetize.as_micros(),
									stats.send.as_micros(),
									stats.channel_wait.as_micros(),
									if stats.is_key_frame { "K" } else { "P" },
									stats.encoded_bytes,
								);
							}
							accum.add(&stats);
							total.add(&stats);

							if accum.last_print.elapsed() >= Duration::from_secs(5) {
								accum = accum.print_and_reset_interval();
							}

							if duration_deadline.is_some_and(|d| Instant::now() >= d) {
								tracing::info!("Duration reached, stopping...");
								break;
							}
						}
					},
					Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
						tracing::warn!("Stats channel lagged, dropped {} frames", n);
					},
					Err(tokio::sync::broadcast::error::RecvError::Closed) => {
						tracing::info!("Stats channel closed (pipeline stopped).");
						break;
					},
				}
			},
			_ = signal::ctrl_c() => {
				tracing::info!("Ctrl+C received, stopping...");
				break;
			},
			_ = tokio::time::sleep(duration_remaining) => {
				if duration_deadline.is_some_and(|d| Instant::now() >= d) {
					tracing::info!("Duration reached, stopping...");
					break;
				}
			},
			_ = tokio::time::sleep(Duration::from_secs(3)) => {
				if accum.count == 0 && Instant::now() >= warmup_deadline && !warned_no_frames {
					tracing::warn!("No frames received after 3s — check that the app renders to the compositor's Wayland/X11 display.");
					warned_no_frames = true;
				}
			},
		}
	}

	if total.count == 0 {
		tracing::warn!("No frames were encoded during the session.");
	} else {
		total.print_summary("Session");
	}

	tracing::info!("Stopping session...");
	let _ = session_manager.stop_session().await;

	tracing::info!("Done.");
	Ok(())
}
