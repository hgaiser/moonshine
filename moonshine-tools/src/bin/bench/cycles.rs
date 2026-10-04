//! Repeated-session acceptance cycles against the real session backend.
//!
//! `--cycles N` runs N launch → stream → stop → relaunch sessions through one
//! `SessionManager`, as the server does across clients. `--reconnect-cycles N`
//! keeps one application and runs N authenticated resume → ANNOUNCE → PLAY
//! epochs. Both rotate one negotiated property per cycle (or none, for the
//! unchanged-resume step), require encoded frames and received UDP bytes in
//! every cycle, and record process resources. After every completed full stop
//! the video port must be bindable and no application unit or child process
//! may remain.
//!
//! The benchmark has no Moonlight client: there is no ENet control stream,
//! client decode or audio endpoint. These cycles establish backend ownership
//! and streaming continuity, not client compatibility.

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use moonshine_core::ShutdownReason;
use moonshine_core::config::ApplicationConfig;
use moonshine_core::session::compositor::{CaptureMode, CompositorConfig};
use moonshine_core::session::manager::SessionManager;
use moonshine_core::session::stream::audio::{AudioChannels, AudioConfig, AudioStreamConfig, AudioStreamContext};
use moonshine_core::session::stream::control::ControlStreamConfig;
use moonshine_core::session::stream::video::pyrowave_protocol::PyroWaveDialect;
use moonshine_core::session::stream::video::{
	BitDepth, ChromaFormat, ColorRange, FrameStats, NegotiatedVideoFormat, VideoCodec, VideoStreamConfig,
	VideoStreamContext,
};
use moonshine_core::session::{RemoteInputKey, RemoteInputKeyId, SessionContext, SessionKeyData, SessionKeys};
use tokio::sync::broadcast;

use super::{Args, STREAM_TIMEOUT_SECS, boxed_error, parse_codec};

type BoxError = Box<dyn std::error::Error>;

const STEPS: [&str; 11] = [
	"unchanged",
	"resolution",
	"fps",
	"bitrate",
	"codec",
	"encryption",
	"dynamic-range",
	"chroma",
	"audio-channels",
	"audio-quality",
	"packet-duration",
];

/// Application unit created by the session backend for every launch.
const APPLICATION_UNIT: &str = "moonshine-session.service";

/// Requested settings; [`Self::video`] normalizes combinations the backend
/// cannot encode (recorded per cycle as the effective format).
#[derive(Clone, Copy, Debug, Default)]
struct Settings {
	small: bool,
	high_fps: bool,
	high_bitrate: bool,
	codec: usize,
	encrypt: bool,
	hdr: bool,
	yuv444: bool,
	channels: usize,
	low_quality: bool,
	ten_ms: bool,
}

impl Settings {
	fn step(mut self, step: usize, codecs: usize) -> Self {
		match step {
			0 => {},
			1 => self.small ^= true,
			2 => self.high_fps ^= true,
			3 => self.high_bitrate ^= true,
			4 => self.codec = (self.codec + 1) % codecs,
			5 => self.encrypt ^= true,
			6 => self.hdr ^= true,
			7 => self.yuv444 ^= true,
			8 => self.channels = (self.channels + 1) % 3,
			9 => self.low_quality ^= true,
			10 => self.ten_ms ^= true,
			_ => unreachable!(),
		}
		self
	}

	fn resolution(self) -> (u32, u32) {
		if self.small { (1280, 720) } else { (1920, 1080) }
	}

	fn fps(self) -> u32 {
		if self.high_fps { 120 } else { 60 }
	}

	fn audio_channels(self) -> (AudioChannels, u32) {
		[
			(AudioChannels::Stereo, 0x3),
			(AudioChannels::Surround51, 0x3f),
			(AudioChannels::Surround71, 0x63f),
		][self.channels]
	}

	fn video(self, codecs: &[VideoCodec], packet_size: usize) -> VideoStreamContext {
		let codec = codecs[self.codec];
		// Conventional 4:4:4 and 10-bit H.264 are not encoded by the Vulkan
		// Video backend; PyroWave carries full-range 4:4:4 and HDR.
		let pyrowave = codec == VideoCodec::PyroWave;
		let chroma = if self.yuv444 && pyrowave {
			ChromaFormat::Yuv444
		} else {
			ChromaFormat::Yuv420
		};
		let range = if pyrowave {
			ColorRange::Full
		} else {
			ColorRange::Limited
		};
		let format = if self.hdr && codec != VideoCodec::H264 {
			NegotiatedVideoFormat::hdr10(codec, chroma, range)
		} else {
			NegotiatedVideoFormat::sdr(codec, chroma, BitDepth::Eight, range)
		};
		let bitrate = if self.high_bitrate { 50_000_000 } else { 20_000_000 };
		let (width, height) = self.resolution();
		VideoStreamContext {
			pyrowave_dialect: pyrowave.then_some(PyroWaveDialect::NativeWireV1),
			width,
			height,
			fps: self.fps(),
			packet_size,
			// PyroWave is intra-only; give it a proportionate budget.
			bitrate: if pyrowave { bitrate * 10 } else { bitrate },
			minimum_fec_packets: 2,
			qos: false,
			format,
			max_reference_frames: 1,
			encrypt_video: self.encrypt,
		}
	}

	fn audio(self) -> AudioStreamContext {
		let (channels, mask) = self.audio_channels();
		AudioStreamContext {
			packet_duration_ms: if self.ten_ms { 10 } else { 5 },
			qos: false,
			audio_config: AudioConfig::from_channels(channels, mask, !self.low_quality),
			encrypt_audio: self.encrypt,
		}
	}
}

/// Process-wide resource snapshot from procfs.
#[derive(Clone, Copy, Debug, Default)]
struct Resources {
	fds: usize,
	threads: usize,
	rss_kib: u64,
	children: usize,
}

impl Resources {
	fn sample() -> Self {
		let count = |path: &str| std::fs::read_dir(path).map(|dir| dir.count()).unwrap_or(0);
		let rss_kib = std::fs::read_to_string("/proc/self/status")
			.ok()
			.and_then(|status| {
				status
					.lines()
					.find_map(|line| line.strip_prefix("VmRSS:"))
					.and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
			})
			.unwrap_or(0);
		Self {
			fds: count("/proc/self/fd"),
			threads: count("/proc/self/task"),
			rss_kib,
			children: child_processes(),
		}
	}
}

/// Direct child processes of this process (Xwayland, helpers).
fn child_processes() -> usize {
	let me = std::process::id().to_string();
	let Ok(dir) = std::fs::read_dir("/proc") else {
		return 0;
	};
	dir.filter_map(Result::ok)
		.filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
		.filter(|stat| {
			// The command name may contain spaces/parentheses; fields follow the last ')'.
			stat.rsplit_once(')')
				.and_then(|(_, rest)| rest.split_whitespace().nth(1))
				.is_some_and(|ppid| ppid == me)
		})
		.count()
}

fn application_unit_active() -> bool {
	std::process::Command::new("systemctl")
		.args(["--user", "is-active", "--quiet", APPLICATION_UNIT])
		.status()
		.is_ok_and(|status| status.success())
}

/// Loopback stand-in for the client's video socket: discovers the endpoint
/// with a legacy PING after every (re)start and counts received bytes.
struct Receiver {
	socket: Arc<tokio::net::UdpSocket>,
	bytes: Arc<AtomicU64>,
	task: tokio::task::JoinHandle<()>,
}

impl Receiver {
	async fn bind() -> Result<Self, BoxError> {
		let socket = Arc::new(tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
		let bytes = Arc::new(AtomicU64::new(0));
		let task = tokio::spawn({
			let socket = socket.clone();
			let bytes = bytes.clone();
			async move {
				let mut buffer = vec![0u8; 65536];
				// ICMP errors from a stopped server are expected between sessions.
				loop {
					if let Ok((len, _)) = socket.recv_from(&mut buffer).await {
						bytes.fetch_add(len as u64, Ordering::Relaxed);
					}
				}
			}
		});
		Ok(Self { socket, bytes, task })
	}

	async fn ping(&self, video_port: u16) -> Result<(), BoxError> {
		self.socket
			.send_to(b"PING", SocketAddr::from((Ipv4Addr::LOCALHOST, video_port)))
			.await?;
		Ok(())
	}
}

impl Drop for Receiver {
	fn drop(&mut self) {
		self.task.abort();
	}
}

/// Frames and bytes delivered during one cycle's streaming window.
struct Window {
	frames: u64,
	key_frames: u64,
	encoded_bytes: u64,
	received_bytes: u64,
}

async fn stream_window(
	stats: &mut broadcast::Receiver<FrameStats>,
	receiver: &Receiver,
	video_port: u16,
	seconds: u64,
) -> Result<Window, BoxError> {
	// Discard frames from before this epoch.
	while matches!(stats.try_recv(), Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_))) {}
	let received_before = receiver.bytes.load(Ordering::Relaxed);
	let deadline = Instant::now() + Duration::from_secs(seconds);
	let mut next_ping = Instant::now();
	let mut window = Window {
		frames: 0,
		key_frames: 0,
		encoded_bytes: 0,
		received_bytes: 0,
	};
	while Instant::now() < deadline {
		// Discovery is per generation; repeat it until the epoch delivers.
		if receiver.bytes.load(Ordering::Relaxed) == received_before && Instant::now() >= next_ping {
			receiver.ping(video_port).await?;
			next_ping = Instant::now() + Duration::from_millis(200);
		}
		match tokio::time::timeout(Duration::from_millis(100), stats.recv()).await {
			Ok(Ok(frame)) => {
				window.frames += 1;
				window.key_frames += u64::from(frame.is_key_frame);
				window.encoded_bytes += frame.encoded_bytes as u64;
			},
			Ok(Err(broadcast::error::RecvError::Lagged(missed))) => window.frames += missed,
			Ok(Err(broadcast::error::RecvError::Closed)) => break,
			Err(_) => {},
		}
	}
	window.received_bytes = receiver.bytes.load(Ordering::Relaxed) - received_before;
	Ok(window)
}

fn port_is_free(port: u16) -> bool {
	std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn describe(video: &VideoStreamContext, audio: &AudioStreamContext) -> String {
	format!(
		"{}x{}@{} {} {} {}-bit {} {} Mbps enc={} audio={:?}/{}ms/{}",
		video.width,
		video.height,
		video.fps,
		video.format.codec,
		video.format.chroma,
		video.format.bit_depth.bits(),
		if video.format.hdr { "HDR10" } else { "SDR" },
		video.bitrate / 1_000_000,
		video.encrypt_video,
		audio.audio_config.channels,
		audio.packet_duration_ms,
		if audio.audio_config.high_quality {
			"high"
		} else {
			"normal"
		},
	)
}

struct CycleLog {
	file: Option<std::fs::File>,
	failures: usize,
}

impl CycleLog {
	fn new(args: &Args) -> Result<Self, BoxError> {
		Ok(Self {
			file: args.cycle_log.as_ref().map(std::fs::File::create).transpose()?,
			failures: 0,
		})
	}

	#[allow(clippy::too_many_arguments)]
	fn record(
		&mut self,
		kind: &str,
		cycle: u32,
		step: &str,
		settings: &str,
		window: Option<&Window>,
		resources: Resources,
		checks: &[(&str, bool)],
		elapsed: Duration,
	) -> Result<(), BoxError> {
		let passed = checks.iter().all(|(_, ok)| *ok);
		self.failures += usize::from(!passed);
		let failed: Vec<&str> = checks.iter().filter(|(_, ok)| !ok).map(|(name, _)| *name).collect();
		let (frames, key_frames, encoded, received) = window.map_or((0, 0, 0, 0), |w| {
			(w.frames, w.key_frames, w.encoded_bytes, w.received_bytes)
		});
		tracing::info!(
			kind,
			cycle,
			step,
			settings,
			frames,
			received_bytes = received,
			fds = resources.fds,
			threads = resources.threads,
			rss_kib = resources.rss_kib,
			children = resources.children,
			elapsed_ms = elapsed.as_millis() as u64,
			result = if passed { "pass" } else { "FAIL" },
			?failed,
			"Cycle"
		);
		if let Some(file) = &mut self.file {
			let mut line = String::new();
			write!(
				line,
				"{{\"kind\":\"{kind}\",\"cycle\":{cycle},\"step\":\"{step}\",\"settings\":\"{settings}\",\
				 \"frames\":{frames},\"key_frames\":{key_frames},\"encoded_bytes\":{encoded},\
				 \"received_bytes\":{received},\"fds\":{},\"threads\":{},\"rss_kib\":{},\"children\":{},\
				 \"elapsed_ms\":{},\"pass\":{passed},\"failed\":{:?}}}",
				resources.fds,
				resources.threads,
				resources.rss_kib,
				resources.children,
				elapsed.as_millis(),
				failed,
			)?;
			writeln!(file, "{line}")?;
		}
		Ok(())
	}
}

fn session_manager(args: &Args, shutdown: &ShutdownManager<ShutdownReason>) -> Result<SessionManager, BoxError> {
	SessionManager::new(
		CompositorConfig {
			capture_mode: if args.composited {
				CaptureMode::Composited
			} else {
				CaptureMode::Auto
			},
			bench_pointer: super::bench_pointer(args),
			..Default::default()
		},
		VideoStreamConfig::default(),
		AudioStreamConfig { port: 0 },
		ControlStreamConfig {
			port: 0,
			..Default::default()
		},
		Ipv4Addr::LOCALHOST.to_string(),
		STREAM_TIMEOUT_SECS,
		false,
		shutdown.clone(),
	)
	.map_err(|()| boxed_error("Failed to create session manager"))
	.inspect(|_| tracing::info!(command = ?args.command, "Session manager ready for cycles"))
}

fn keys(cycle: u32) -> SessionKeyData {
	// A fresh key on even cycles and a reused one on odd cycles.
	SessionKeyData::new(
		RemoteInputKey::from_bytes([(cycle / 2 % 251) as u8 + 1; 16]),
		RemoteInputKeyId::new(cycle),
	)
}

fn context(args: &Args, settings: Settings, cycle: u32) -> SessionContext {
	let (audio_channels, audio_channel_mask) = settings.audio_channels();
	SessionContext {
		application: ApplicationConfig {
			title: "bench-cycles".to_string(),
			command: args.command.clone(),
			..Default::default()
		},
		application_id: 1,
		resolution: settings.resolution(),
		refresh_rate: settings.fps(),
		keys: SessionKeys::Keys(keys(cycle)),
		audio_channels,
		audio_channel_mask,
		hdr: settings.hdr,
		client_ip: Ipv4Addr::LOCALHOST.into(),
	}
}

fn codecs(args: &Args) -> Vec<VideoCodec> {
	args.cycle_codecs
		.split(',')
		.map(str::trim)
		.filter(|codec| !codec.is_empty())
		.map(parse_codec)
		.collect()
}

/// Streaming in a cycle: frames at a meaningful fraction of the target rate,
/// and bytes actually received by the loopback client.
fn streaming_checks(window: &Window, video: &VideoStreamContext, seconds: u64) -> [(&'static str, bool); 2] {
	let expected = u64::from(video.fps) * seconds;
	[
		("frames", window.frames * 4 >= expected),
		("received", window.received_bytes > 0),
	]
}

/// `--cycles`: full sessions through one manager.
pub(super) async fn run_full_cycles(args: &Args) -> Result<(), BoxError> {
	let codecs = codecs(args);
	let video_port = VideoStreamConfig::default().port;
	let shutdown = ShutdownManager::<ShutdownReason>::new();
	let manager = session_manager(args, &shutdown)?;
	let mut stats = manager.bench_stats_receiver();
	let receiver = Receiver::bind().await?;
	let mut log = CycleLog::new(args)?;
	let baseline = Resources::sample();
	tracing::info!(?baseline, "Baseline before the first session");
	let mut settings = Settings::default();
	for cycle in 0..args.cycles {
		let step = cycle as usize % STEPS.len();
		settings = settings.step(step, codecs.len());
		let video = settings.video(&codecs, args.packet_size);
		let audio = settings.audio();
		let description = describe(&video, &audio);
		let started = Instant::now();
		let mut checks = Vec::new();
		let mut window = None;
		let result: Result<(), String> = async {
			manager
				.initialize_session(context(args, settings, cycle))
				.await
				.map_err(|()| "initialize")?;
			manager.launch_session().await.map_err(|()| "launch")?;
			let grant = manager
				.authorize_stream(Ipv4Addr::LOCALHOST.into())
				.await
				.ok_or("authorize")?;
			manager
				.set_stream_context(&grant, video.clone(), audio.clone(), false)
				.await
				.map_err(|()| "announce")?;
			manager.start_session(&grant).await.map_err(|()| "play")?;
			manager.trigger_streams_start().await;
			Ok(())
		}
		.await
		.map_err(str::to_string);
		checks.push(("start", result.is_ok()));
		if result.is_ok() {
			let measured = stream_window(&mut stats, &receiver, video_port, args.cycle_seconds).await?;
			checks.extend(streaming_checks(&measured, &video, args.cycle_seconds));
			window = Some(measured);
		} else {
			tracing::error!(cycle, failed_stage = ?result, settings = description, "Cycle start failed");
		}
		checks.push(("stop", manager.stop_session().await.is_ok()));
		checks.push(("port-released", port_is_free(video_port)));
		checks.push(("unit-stopped", !application_unit_active()));
		let resources = Resources::sample();
		checks.push(("no-children", resources.children == 0));
		log.record(
			"full",
			cycle,
			STEPS[step],
			&description,
			window.as_ref(),
			resources,
			&checks,
			started.elapsed(),
		)?;
		if shutdown.is_shutdown_triggered() {
			return Err(boxed_error("service shutdown requested during cycles"));
		}
	}
	finish(log, args.cycles, baseline)
}

/// `--reconnect-cycles`: authenticated reconnect epochs on one application.
pub(super) async fn run_reconnect_cycles(args: &Args) -> Result<(), BoxError> {
	let codecs = codecs(args);
	let video_port = VideoStreamConfig::default().port;
	let shutdown = ShutdownManager::<ShutdownReason>::new();
	let manager = session_manager(args, &shutdown)?;
	let mut stats = manager.bench_stats_receiver();
	let receiver = Receiver::bind().await?;
	let mut log = CycleLog::new(args)?;
	let baseline = Resources::sample();
	let mut settings = Settings::default();
	let video = settings.video(&codecs, args.packet_size);
	manager
		.initialize_session(context(args, settings, 0))
		.await
		.map_err(|()| boxed_error("initialize"))?;
	if manager.launch_session().await.is_err() {
		let _ = manager.stop_session().await;
		return Err(boxed_error("launch"));
	}
	let grant = manager
		.authorize_stream(Ipv4Addr::LOCALHOST.into())
		.await
		.ok_or_else(|| boxed_error("authorize"))?;
	let started = async {
		manager
			.set_stream_context(&grant, video.clone(), settings.audio(), false)
			.await?;
		manager.start_session(&grant).await
	}
	.await;
	if started.is_err() {
		let _ = manager.stop_session().await;
		return Err(boxed_error("initial stream start"));
	}
	manager.trigger_streams_start().await;
	let initial = stream_window(&mut stats, &receiver, video_port, args.cycle_seconds).await?;
	if streaming_checks(&initial, &video, args.cycle_seconds)
		.iter()
		.any(|(_, ok)| !ok)
	{
		let _ = manager.stop_session().await;
		return Err(boxed_error("initial stream did not deliver frames"));
	}
	for cycle in 1..=args.reconnect_cycles {
		let step = cycle as usize % STEPS.len();
		let previous = settings;
		settings = settings.step(step, codecs.len());
		let video = settings.video(&codecs, args.packet_size);
		let audio = settings.audio();
		let description = describe(&video, &audio);
		let started = Instant::now();
		let mut checks = Vec::new();
		let mut window = None;
		let result: Result<(), &str> = async {
			manager
				.bench_resume(keys(cycle), Ipv4Addr::LOCALHOST.into())
				.await
				.map_err(|()| "resume")?;
			let grant = manager
				.authorize_stream(Ipv4Addr::LOCALHOST.into())
				.await
				.ok_or("authorize")?;
			manager
				.set_stream_context(&grant, video.clone(), audio.clone(), false)
				.await
				.map_err(|()| "announce")?;
			manager.start_session(&grant).await.map_err(|()| "play")?;
			manager.trigger_streams_start().await;
			Ok(())
		}
		.await;
		checks.push(("reconnect", result.is_ok()));
		if let Err(stage) = result {
			tracing::error!(
				cycle,
				stage,
				from = describe(&previous.video(&codecs, args.packet_size), &previous.audio()),
				to = description,
				"Reconnect failed"
			);
		} else {
			let measured = stream_window(&mut stats, &receiver, video_port, args.cycle_seconds).await?;
			checks.extend(streaming_checks(&measured, &video, args.cycle_seconds));
			window = Some(measured);
		}
		let context = manager.get_session_context().await.ok().flatten();
		checks.push((
			"context",
			context.is_some_and(|context| {
				context.resolution == (video.width, video.height)
					&& context.refresh_rate == video.fps
					&& context.hdr == video.format.hdr
			}),
		));
		log.record(
			"reconnect",
			cycle,
			STEPS[step],
			&description,
			window.as_ref(),
			Resources::sample(),
			&checks,
			started.elapsed(),
		)?;
		if result.is_err() {
			// A failed reconnect tears the session down by design.
			break;
		}
	}
	let stopped = manager.stop_session().await.is_ok();
	let released = port_is_free(video_port) && !application_unit_active();
	let resources = Resources::sample();
	tracing::info!(stopped, released, ?resources, "Final teardown");
	if !stopped || !released || resources.children != 0 {
		log.failures += 1;
	}
	finish(log, args.reconnect_cycles, baseline)
}

fn finish(log: CycleLog, cycles: u32, baseline: Resources) -> Result<(), BoxError> {
	let after = Resources::sample();
	tracing::info!(cycles, failures = log.failures, ?baseline, ?after, "Cycles complete");
	if log.failures > 0 {
		return Err(boxed_error(format!("{} of {cycles} cycle(s) failed", log.failures)));
	}
	Ok(())
}
