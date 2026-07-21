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

	/// Run the built-in 4K/1440p/1080p x 60/120/360 FPS x HEVC/H.264/AV1 benchmark matrix.
	#[arg(long)]
	matrix: bool,

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

fn percentiles(samples: &[u64]) -> (u64, u64, u64) {
	if samples.is_empty() {
		return (0, 0, 0);
	}

	fn percentile(sorted: &[u64], p: f64) -> u64 {
		let idx = ((sorted.len() as f64 * p).ceil() as usize)
			.saturating_sub(1)
			.min(sorted.len() - 1);
		sorted[idx]
	}

	let mut sorted = samples.to_vec();
	sorted.sort_unstable();
	(
		percentile(&sorted, 0.50),
		percentile(&sorted, 0.95),
		percentile(&sorted, 0.99),
	)
}

#[derive(Clone, Debug)]
struct LatencyDistribution {
	avg_us: f64,
	min_us: u64,
	max_us: u64,
	p50_us: u64,
	p95_us: u64,
	p99_us: u64,
}

impl LatencyDistribution {
	fn new(total_us: u128, samples: &[u64]) -> Self {
		if samples.is_empty() {
			return Self {
				avg_us: 0.0,
				min_us: 0,
				max_us: 0,
				p50_us: 0,
				p95_us: 0,
				p99_us: 0,
			};
		}

		let (p50_us, p95_us, p99_us) = percentiles(samples);
		let count = samples.len() as f64;
		Self {
			avg_us: total_us as f64 / count,
			min_us: samples.iter().copied().min().unwrap_or(0),
			max_us: samples.iter().copied().max().unwrap_or(0),
			p50_us,
			p95_us,
			p99_us,
		}
	}

	fn matrix_value(&self) -> String {
		format!(
			"{:.0}/{}/{}/{}/{}",
			self.avg_us, self.p50_us, self.p95_us, self.p99_us, self.max_us
		)
	}
}

#[derive(Clone, Debug)]
struct StatsSummary {
	count: u64,
	fps: f64,
	mbps: f64,
	total: LatencyDistribution,
	submit: LatencyDistribution,
	encode_wait: LatencyDistribution,
	avg_channel_wait_us: f64,
	avg_import_us: f64,
	avg_convert_us: f64,
	avg_consumer_queue_us: f64,
	avg_packetize_us: f64,
	avg_send_us: f64,
	key_frames: u64,
}

impl StatsSummary {
	fn avg_accounted_us(&self) -> f64 {
		self.avg_channel_wait_us
			+ self.avg_import_us
			+ self.avg_convert_us
			+ self.submit.avg_us
			+ self.encode_wait.avg_us
			+ self.avg_packetize_us
			+ self.avg_send_us
	}

	fn avg_delta_us(&self) -> f64 {
		self.total.avg_us - self.avg_accounted_us()
	}
}

struct BenchmarkReport {
	resolution_label: &'static str,
	resolution: String,
	target_fps: u32,
	codec: String,
	summary: Option<StatsSummary>,
	interrupted: bool,
}

struct MatrixReport {
	resolution_label: &'static str,
	resolution: String,
	target_fps: u32,
	codec: String,
	status: MatrixStatus,
}

enum MatrixStatus {
	Ok(Option<StatsSummary>),
	Failed(String),
	Interrupted(Option<StatsSummary>),
}

struct StatsAccumulator {
	count: u64,
	total_us: u128,
	channel_wait_us: u128,
	import_us: u128,
	convert_us: u128,
	submit_us: u128,
	consumer_queue_us: u128,
	encode_wait_us: u128,
	packetize_us: u128,
	send_us: u128,
	total_samples_us: Vec<u64>,
	submit_samples_us: Vec<u64>,
	encode_wait_samples_us: Vec<u64>,
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
			submit_us: 0,
			consumer_queue_us: 0,
			encode_wait_us: 0,
			packetize_us: 0,
			send_us: 0,
			total_samples_us: Vec::new(),
			submit_samples_us: Vec::new(),
			encode_wait_samples_us: Vec::new(),
			key_frames: 0,
			encoded_bytes: 0,
			start: now,
			last_print: now,
		}
	}

	fn add(&mut self, stats: &FrameStats) {
		if self.count == 0 {
			let now = Instant::now();
			self.start = now;
			self.last_print = now;
		}
		self.count += 1;
		let total = stats.total.as_micros() as u64;
		let submit = stats.submit.as_micros() as u64;
		let encode_wait = stats.encode_wait.as_micros() as u64;
		self.total_us += total as u128;
		self.channel_wait_us += stats.channel_wait.as_micros();
		self.import_us += stats.import.as_micros();
		self.convert_us += stats.convert.as_micros();
		self.submit_us += submit as u128;
		self.consumer_queue_us += stats.consumer_queue.as_micros();
		self.encode_wait_us += encode_wait as u128;
		self.packetize_us += stats.packetize.as_micros();
		self.send_us += stats.send.as_micros();
		self.total_samples_us.push(total);
		self.submit_samples_us.push(submit);
		self.encode_wait_samples_us.push(encode_wait);
		if stats.is_key_frame {
			self.key_frames += 1;
		}
		self.encoded_bytes += stats.encoded_bytes as u64;
	}

	fn summary(&self) -> Option<StatsSummary> {
		if self.count == 0 {
			return None;
		}
		let elapsed = self.start.elapsed().as_secs_f64();
		let fps = if elapsed > 0.0 {
			self.count as f64 / elapsed
		} else {
			0.0
		};
		let mbps = if elapsed > 0.0 {
			self.encoded_bytes as f64 * 8.0 / elapsed / 1_000_000.0
		} else {
			0.0
		};

		Some(StatsSummary {
			count: self.count,
			fps,
			mbps,
			total: LatencyDistribution::new(self.total_us, &self.total_samples_us),
			submit: LatencyDistribution::new(self.submit_us, &self.submit_samples_us),
			encode_wait: LatencyDistribution::new(self.encode_wait_us, &self.encode_wait_samples_us),
			avg_channel_wait_us: self.channel_wait_us as f64 / self.count as f64,
			avg_import_us: self.import_us as f64 / self.count as f64,
			avg_convert_us: self.convert_us as f64 / self.count as f64,
			avg_consumer_queue_us: self.consumer_queue_us as f64 / self.count as f64,
			avg_packetize_us: self.packetize_us as f64 / self.count as f64,
			avg_send_us: self.send_us as f64 / self.count as f64,
			key_frames: self.key_frames,
		})
	}

	fn print_summary(&self, label: &str) -> Option<StatsSummary> {
		let summary = self.summary()?;

		tracing::info!(
			"{} [{} frames, {:.1} fps, {:.2} Mbps]",
			label,
			summary.count,
			summary.fps,
			summary.mbps
		);
		tracing::info!(
			"  total:    avg={avg_total:.0}us  min={}us  max={}us",
			summary.total.min_us,
			summary.total.max_us,
			avg_total = summary.total.avg_us
		);
		tracing::info!(
			"            p50={}us  p95={}us  p99={}us",
			summary.total.p50_us,
			summary.total.p95_us,
			summary.total.p99_us
		);
		tracing::info!(
			"  submit:   avg={avg_submit:.0}us  min={}us  max={}us",
			summary.submit.min_us,
			summary.submit.max_us,
			avg_submit = summary.submit.avg_us
		);
		tracing::info!(
			"            p50={}us  p95={}us  p99={}us",
			summary.submit.p50_us,
			summary.submit.p95_us,
			summary.submit.p99_us
		);
		tracing::info!(
			"  enc_wait: avg={avg_encode_wait:.0}us  min={}us  max={}us",
			summary.encode_wait.min_us,
			summary.encode_wait.max_us,
			avg_encode_wait = summary.encode_wait.avg_us
		);
		tracing::info!(
			"            p50={}us  p95={}us  p99={}us",
			summary.encode_wait.p50_us,
			summary.encode_wait.p95_us,
			summary.encode_wait.p99_us
		);
		tracing::info!(
			"  avg breakdown: ch_wait={avg_channel_wait:.0}us  import={avg_import:.0}us  convert={avg_convert:.0}us  submit={avg_submit:.0}us  enc_wait={avg_encode_wait:.0}us  pkt={avg_packetize:.0}us  send={avg_send:.0}us",
			avg_channel_wait = summary.avg_channel_wait_us,
			avg_import = summary.avg_import_us,
			avg_convert = summary.avg_convert_us,
			avg_submit = summary.submit.avg_us,
			avg_encode_wait = summary.encode_wait.avg_us,
			avg_packetize = summary.avg_packetize_us,
			avg_send = summary.avg_send_us,
		);
		tracing::info!(
			"  accounted: avg={avg_accounted:.0}us  delta={avg_delta:.0}us  queue_diag={avg_consumer_queue:.0}us",
			avg_accounted = summary.avg_accounted_us(),
			avg_delta = summary.avg_delta_us(),
			avg_consumer_queue = summary.avg_consumer_queue_us
		);
		tracing::info!("  key_frames: {}", summary.key_frames);

		Some(summary)
	}

	fn print_and_reset_interval(&mut self) -> Self {
		self.print_summary("Interval");
		Self::new()
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

	if args.matrix {
		run_matrix(&args).await
	} else {
		run_single(&args).await
	}
}

async fn run_single(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
	run_benchmark(args, "", &args.resolution, args.fps, &args.codec, args.duration).await?;
	Ok(())
}

async fn run_matrix(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
	const RESOLUTIONS: [(&str, &str); 3] = [("4k", "3840x2160"), ("1440p", "2560x1440"), ("1080p", "1920x1080")];
	const FPS_VALUES: [u32; 3] = [60, 120, 360];
	const CODECS: [&str; 3] = ["hevc", "h264", "av1"];

	let duration = if args.duration == 0 {
		tracing::info!("Matrix mode requires a finite duration; defaulting to 8s. Pass --duration to override.");
		8
	} else {
		args.duration
	};

	let mut reports = Vec::new();
	let mut interrupted = false;

	tracing::info!("Starting Moonshine benchmark matrix");
	tracing::info!("  command:    {}", args.command.join(" "));
	tracing::info!("  duration:   {}s", duration);
	tracing::info!("  warmup:     {}s", args.warmup);
	tracing::info!("  fps:        {:?}", FPS_VALUES);
	tracing::info!("  bitrate:    {} bps", args.bitrate);
	tracing::info!("  hdr:        {}", args.hdr);

	'outer: for (resolution_label, resolution) in RESOLUTIONS {
		for fps in FPS_VALUES {
			for codec in CODECS {
				tracing::info!(
					"Starting matrix run: {} {} {}fps {}",
					resolution_label,
					resolution,
					fps,
					codec
				);

				match run_benchmark(args, resolution_label, resolution, fps, codec, duration).await {
					Ok(report) => {
						interrupted = report.interrupted;
						let status = if report.interrupted {
							MatrixStatus::Interrupted(report.summary)
						} else {
							MatrixStatus::Ok(report.summary)
						};
						reports.push(MatrixReport {
							resolution_label: report.resolution_label,
							resolution: report.resolution,
							target_fps: report.target_fps,
							codec: report.codec,
							status,
						});

						if interrupted {
							break 'outer;
						}
					},
					Err(err) => {
						tracing::error!(
							"Matrix run failed: {} {} {}fps {}: {}",
							resolution_label,
							resolution,
							fps,
							codec,
							err
						);
						reports.push(MatrixReport {
							resolution_label,
							resolution: resolution.to_string(),
							target_fps: fps,
							codec: codec.to_string(),
							status: MatrixStatus::Failed(err.to_string()),
						});
					},
				}
			}
		}
	}

	print_matrix_summary(&reports);

	if interrupted {
		return Err(boxed_error("Matrix benchmark interrupted"));
	}

	let failures = reports
		.iter()
		.filter(|report| matches!(report.status, MatrixStatus::Failed(_)))
		.count();
	if failures > 0 {
		return Err(boxed_error(format!("{failures} matrix benchmark run(s) failed")));
	}

	Ok(())
}

async fn run_benchmark(
	args: &Args,
	resolution_label: &'static str,
	resolution: &str,
	target_fps: u32,
	codec: &str,
	duration: u64,
) -> Result<BenchmarkReport, Box<dyn std::error::Error>> {
	const STREAM_TIMEOUT_SECS: u64 = 60;

	let (width, height) = parse_resolution(resolution).map_err(boxed_error)?;
	let video_format = parse_codec(codec);

	tracing::info!("Starting Moonshine benchmark");
	tracing::info!("  command:    {}", args.command.join(" "));
	tracing::info!("  resolution: {}x{}", width, height);
	tracing::info!("  fps:        {}", target_fps);
	tracing::info!("  bitrate:    {} bps", args.bitrate);
	tracing::info!("  codec:      {}", codec);
	tracing::info!("  hdr:        {}", args.hdr);
	let duration_str = if duration == 0 {
		"infinite".to_string()
	} else {
		duration.to_string()
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
		STREAM_TIMEOUT_SECS,
		shutdown.clone(),
	)
	.map_err(|_| boxed_error("Failed to create session manager"))?;

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
		refresh_rate: target_fps,
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
		.map_err(|_| boxed_error("Failed to initialize session"))?;

	tracing::info!("Launching session (compositor + app)...");
	if let Err(err) = session_manager.launch_session().await {
		let _ = session_manager.stop_session().await;
		return Err(boxed_error(format!("Failed to launch session: {err:?}")));
	}

	let video_ctx = VideoStreamContext {
		width,
		height,
		fps: target_fps,
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
	if let Err(err) = session_manager.set_stream_context(video_ctx, audio_ctx).await {
		let _ = session_manager.stop_session().await;
		return Err(boxed_error(format!("Failed to set stream context: {err:?}")));
	}

	tracing::info!("Starting session streams...");
	if let Err(err) = session_manager.start_session().await {
		let _ = session_manager.stop_session().await;
		return Err(boxed_error(format!("Failed to start session: {err:?}")));
	}

	tracing::info!("Triggering video and audio pipelines...");
	session_manager.trigger_streams_start().await;

	tracing::info!("Session active. Collecting stats...");

	let warmup_deadline = Instant::now() + Duration::from_secs(args.warmup);
	let mut accum = StatsAccumulator::new();
	let mut total = StatsAccumulator::new();
	let mut warned_no_frames = false;

	let duration_deadline = if duration > 0 {
		Some(Instant::now() + Duration::from_secs(duration))
	} else {
		None
	};
	let mut interrupted = false;

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
									"frame: total={}us ch_wait={}us import={}us convert={}us submit={}us queue={}us enc_wait={}us pkt={}us send={}us {} {}b",
									stats.total.as_micros(),
									stats.channel_wait.as_micros(),
									stats.import.as_micros(),
									stats.convert.as_micros(),
									stats.submit.as_micros(),
									stats.consumer_queue.as_micros(),
									stats.encode_wait.as_micros(),
									stats.packetize.as_micros(),
									stats.send.as_micros(),
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
				interrupted = true;
				break;
			},
			_ = tokio::time::sleep(duration_remaining) => {
				if duration_deadline.is_some_and(|d| Instant::now() >= d) {
					tracing::info!("Duration reached, stopping...");
					break;
				}
			},
			_ = tokio::time::sleep(Duration::from_secs(3)) => {
				if total.count == 0 && Instant::now() >= warmup_deadline && !warned_no_frames {
					tracing::warn!("No frames received after 3s — check that the app renders to the compositor's Wayland/X11 display.");
					warned_no_frames = true;
				}
			},
		}
	}

	let summary = if total.count == 0 {
		tracing::warn!("No frames were encoded during the session.");
		None
	} else {
		total.print_summary("Session")
	};

	tracing::info!("Stopping session...");
	let _ = session_manager.stop_session().await;

	tracing::info!("Done.");
	Ok(BenchmarkReport {
		resolution_label,
		resolution: format!("{}x{}", width, height),
		target_fps,
		codec: codec.to_string(),
		summary,
		interrupted,
	})
}

fn print_matrix_summary(reports: &[MatrixReport]) {
	tracing::info!("Moonshine benchmark matrix summary");
	tracing::info!("Latency distributions (us): values are avg/p50/p95/p99/max");
	tracing::info!(
		"  {:<8} {:<10} {:>6} {:<5} {:<11} {:>7} {:>7} {:>8}  {:>24}  {:>24}  {:>24}",
		"label",
		"resolution",
		"target",
		"codec",
		"status",
		"frames",
		"actual",
		"mbps",
		"total",
		"submit",
		"enc_wait"
	);

	for report in reports {
		match &report.status {
			MatrixStatus::Ok(Some(summary)) | MatrixStatus::Interrupted(Some(summary)) => {
				let status = if matches!(&report.status, MatrixStatus::Ok(_)) {
					"ok"
				} else {
					"interrupted"
				};
				tracing::info!(
					"  {:<8} {:<10} {:>6} {:<5} {:<11} {:>7} {:>7.1} {:>8.2}  {:>24}  {:>24}  {:>24}",
					report.resolution_label,
					report.resolution,
					report.target_fps,
					report.codec,
					status,
					summary.count,
					summary.fps,
					summary.mbps,
					summary.total.matrix_value(),
					summary.submit.matrix_value(),
					summary.encode_wait.matrix_value(),
				);
			},
			MatrixStatus::Ok(None) | MatrixStatus::Interrupted(None) => {
				let status = if matches!(&report.status, MatrixStatus::Ok(_)) {
					"no-frames"
				} else {
					"interrupted"
				};
				tracing::info!(
					"  {:<8} {:<10} {:>6} {:<5} {:<11}",
					report.resolution_label,
					report.resolution,
					report.target_fps,
					report.codec,
					status
				);
			},
			MatrixStatus::Failed(err) => {
				tracing::info!(
					"  {:<8} {:<10} {:>6} {:<5} failed: {}",
					report.resolution_label,
					report.resolution,
					report.target_fps,
					report.codec,
					err
				);
			},
		}
	}

	tracing::info!("Average pipeline breakdown (us/frame): additive stages; delta = total - accounted; queue_diag is included in enc_wait");
	tracing::info!(
		"  {:<8} {:<10} {:>6} {:<5} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10} {:>8} {:>8} {:>6}",
		"label",
		"resolution",
		"target",
		"codec",
		"ch_wait",
		"import",
		"convert",
		"submit",
		"enc_wait",
		"pkt",
		"send",
		"accounted",
		"delta",
		"queue",
		"key"
	);
	for report in reports {
		if let MatrixStatus::Ok(Some(summary)) | MatrixStatus::Interrupted(Some(summary)) = &report.status {
			tracing::info!(
				"  {:<8} {:<10} {:>6} {:<5} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>10.0} {:>8.0} {:>8.0} {:>6}",
				report.resolution_label,
				report.resolution,
				report.target_fps,
				report.codec,
				summary.avg_channel_wait_us,
				summary.avg_import_us,
				summary.avg_convert_us,
				summary.submit.avg_us,
				summary.encode_wait.avg_us,
				summary.avg_packetize_us,
				summary.avg_send_us,
				summary.avg_accounted_us(),
				summary.avg_delta_us(),
				summary.avg_consumer_queue_us,
				summary.key_frames,
			);
		}
	}
}

fn boxed_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
	Box::new(std::io::Error::new(std::io::ErrorKind::Other, message.into()))
}
