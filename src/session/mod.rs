use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use async_shutdown::ShutdownManager;
use manager::SessionShutdownReason;
use tokio::sync::watch;

use crate::gpu::GpuContext;
use crate::session::compositor::CompositorConfig;
use crate::session::stream::StartSignal;
use crate::session::stream::audio::AudioChannels;
use crate::session::stream::audio::AudioHandle;
use crate::session::stream::audio::AudioStream;
use crate::session::stream::audio::AudioStreamContext;
use crate::session::stream::control::ControlStream;
use crate::session::stream::control::ControlStreamContext;
use crate::session::stream::control::{InputHandler, KeepAlive};
use crate::session::stream::video::StatsSender;
use crate::session::stream::video::VideoFormat;
use crate::session::stream::video::VideoStream;
use crate::session::stream::video::VideoStreamContext;
use crate::session::stream::video::VideoStreamHandle;

use self::application::Application;
use self::application::ApplicationConfig;
use self::application::ApplicationContext;
use self::compositor::Compositor;
use self::compositor::LaunchedCompositor;
use self::compositor::frame::HdrModeState;
use self::inhibit::SleepInhibitor;
use self::stream::audio::AudioStreamConfig;
use self::stream::control::ControlStreamConfig;
use self::stream::video::VideoStreamConfig;

pub mod application;
pub mod browser;
pub mod compositor;
pub mod inhibit;
pub mod manager;
pub mod stream;

/// Timeout in seconds for the HTTP launch endpoint to wait for the session to launch.
pub(crate) const APP_LAUNCH_HTTP_TIMEOUT_SECS: u64 = 60;

/// Raw session encryption key data.
#[derive(Clone, Debug)]
pub struct SessionKeyData {
	/// AES GCM key used for encoding video / audio / control messages.
	pub remote_input_key: Vec<u8>,

	/// AES GCM initialization vector for video / audio / control messages.
	pub remote_input_key_id: i64,
}

pub(crate) type SessionKeysReceiver = watch::Receiver<SessionKeyData>;
pub(crate) type SessionKeysSender = watch::Sender<SessionKeyData>;

/// Session keys — either raw keys or a watch receiver.
#[derive(Clone, Debug)]
pub enum SessionKeys {
	Keys(SessionKeyData),
	Rx(SessionKeysReceiver),
}

impl SessionKeys {
	pub(crate) fn new(remote_input_key: Vec<u8>, remote_input_key_id: i64) -> Self {
		Self::Keys(SessionKeyData {
			remote_input_key,
			remote_input_key_id,
		})
	}

	pub(crate) fn clone_rx(&self) -> Option<SessionKeysReceiver> {
		match self {
			Self::Rx(rx) => Some(rx.clone()),
			_ => None,
		}
	}
}

/// Context for a session.
///
/// This is created at launch time and contains all the information about the session
/// that is needed to start the compositor, application, and streams.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SessionContext {
	/// Application to launch.
	pub application: ApplicationConfig,

	/// ID of the application as reported to the client.
	pub application_id: i32,

	/// Resolution of the video stream (width, height).
	pub resolution: (u32, u32),

	/// Refresh rate of the video stream (in Hz).
	pub refresh_rate: u32,

	/// Encryption keys for encoding traffic.
	pub keys: SessionKeys,

	/// Audio channel count (2, 6, or 8).
	pub audio_channels: AudioChannels,

	/// Audio channel mask.
	pub audio_channel_mask: u32,

	/// If true, the compositor will be launched with HDR support.
	pub hdr: bool,
}

/// The state of the session. This enum enforces the session lifecycle:
///
/// 1. `Initialized` — Session created; compositor and app not yet started.
/// 2. `Launched` — Compositor and app are running; waiting for RTSP negotiation.
/// 3. `Active` — Streams are active.
enum SessionState {
	/// Session initialized; compositor and app not yet started.
	Initialized(InitializedSession),
	/// Compositor and app launched; waiting for RTSP PLAY.
	Launched(LaunchedSession),
	/// Streams active.
	Active(ActiveSession),
}

impl SessionState {
	fn context(&self) -> &SessionContext {
		match self {
			Self::Initialized(session) => session.context(),
			Self::Launched(launched) => launched.context(),
			Self::Active(active) => active.context(),
		}
	}
}

/// Initialized session state — components created, compositor and app not yet started.
pub(crate) struct InitializedSession {
	context: SessionContext,
	/// Names the application's systemd unit and the PulseAudio socket directory
	/// (see `SessionManager::new`).
	session_name: String,
	compositor: Compositor,
	audio_stream: AudioStream,
	video_stream: VideoStream,
	control_stream: ControlStream,
	hdr_metadata_rx: watch::Receiver<HdrModeState>,
	stop: ShutdownManager<SessionShutdownReason>,
}

impl InitializedSession {
	#[allow(clippy::too_many_arguments)]
	pub(crate) async fn new(
		compositor_config: CompositorConfig,
		video_config: VideoStreamConfig,
		audio_config: AudioStreamConfig,
		control_config: ControlStreamConfig,
		address: String,
		context: SessionContext,
		session_name: String,
		gpu: GpuContext,
		stop: ShutdownManager<SessionShutdownReason>,
		stats_tx: StatsSender,
	) -> Result<Self, ()> {
		// Create HDR metadata watch channel.
		let (hdr_metadata_tx, hdr_metadata_rx) = watch::channel(HdrModeState::new(context.hdr));

		// Create compositor, audio stream, video stream, and control stream.
		// Read the host's keyboard layout for each stream, so a change on the
		// host applies without a restart.
		let compositor_config = CompositorConfig {
			keyboard: compositor_config.keyboard.resolve().await,
			..compositor_config
		};
		let (compositor, handles) = Compositor::new(compositor_config, (&context).into(), gpu.clone(), stop.clone());
		let audio = AudioStream::new(audio_config, address.clone(), &session_name, stop.clone()).await?;
		let video_stream = VideoStream::new(
			video_config.clone(),
			address.clone(),
			handles.frames,
			gpu,
			hdr_metadata_tx,
			stop.clone(),
			stats_tx,
		)
		.await?;
		let control_stream = ControlStream::new(control_config, address, handles.input, stop.clone())?;

		Ok(Self {
			context,
			session_name,
			compositor,
			audio_stream: audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
			stop,
		})
	}

	pub(crate) fn context(&self) -> &SessionContext {
		&self.context
	}

	/// Launch the session — starts the compositor and application, but does not start streams.
	pub(crate) async fn launch(self) -> Result<LaunchedSession, ()> {
		let Self {
			context,
			session_name,
			compositor,
			audio_stream: audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
			stop,
		} = self;

		let launched_compositor = compositor.launch()?;
		let ready = launched_compositor.ready();
		let pulse_socket_path = audio.pulse_socket_path.clone();

		let application = Application::spawn(
			context.application.clone(),
			ApplicationContext {
				unit_name: format!("{session_name}-session.service"),
				pulse_socket_path,
				xdisplay: ready.xdisplay,
				wayland_display: ready.wayland_display.clone(),
				compositor_env: ready.env.clone(),
				// Populate extra_env with width, height and refreshrate values of the client for e.g. scripting
				extra_env: HashMap::from([
					("MOONSHINE_CLIENT_WIDTH".to_string(), context.resolution.0.to_string()),
					("MOONSHINE_CLIENT_HEIGHT".to_string(), context.resolution.1.to_string()),
					(
						"MOONSHINE_CLIENT_FRAMERATE".to_string(),
						context.refresh_rate.to_string(),
					),
				]),
			},
			stop,
		)
		.await?;

		Ok(LaunchedSession {
			context,
			application,
			video_stream,
			launched_compositor,
			audio,
			control_stream,
			hdr_metadata_rx,
		})
	}
}

/// Launched session state — compositor and app running, waiting for RTSP negotiation.
pub(crate) struct LaunchedSession {
	context: SessionContext,
	application: Application,
	video_stream: VideoStream,
	launched_compositor: LaunchedCompositor,
	audio: AudioStream,
	control_stream: ControlStream,
	hdr_metadata_rx: watch::Receiver<HdrModeState>,
}

impl LaunchedSession {
	pub(crate) fn context(&self) -> &SessionContext {
		&self.context
	}

	pub(crate) async fn start(
		self,
		video_config: VideoStreamConfig,
		stream_timeout: u64,
		video_ctx: VideoStreamContext,
		audio_ctx: AudioStreamContext,
		stop: ShutdownManager<SessionShutdownReason>,
		inhibit_sleep: bool,
	) -> Result<(ActiveSession, StartSignal, StartSignal), ()> {
		let Self {
			context,
			launched_compositor,
			application,
			audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
		} = self;

		// The compositor reports the *effective* HDR: false when HDR was requested
		// but the GPU fell back to an SDR format.
		let hdr_effective = launched_compositor.hdr();

		// Extract the watch receiver for streams.
		let keys_rx = context.keys.clone_rx().ok_or_else(|| {
			tracing::error!("Session keys not initialized");
		})?;

		let video_format = video_ctx.video_format;
		let audio_layout = audio_ctx.audio_config.stream_config.clone();
		let ten_bit = video_ctx.dynamic_range == crate::session::stream::video::VideoDynamicRange::Hdr;
		let started_with = (video_ctx.clone(), audio_ctx.clone());

		// Start video stream — gated, returns VideoStreamHandle.
		let video_handle = video_stream
			.start(video_config, video_ctx, keys_rx.clone(), stop.clone())
			.map_err(|()| tracing::error!("Failed to start video stream"))?;

		// Start audio stream — gated, returns its StartSignal.
		let (audio_trigger, audio_handle) = audio
			.start(audio_ctx, keys_rx)
			.map_err(|()| tracing::error!("Failed to start audio stream"))?;

		// Clone the start flags for external triggering (e.g. bench binary).
		let video_start = video_handle.start_signal();
		let audio_start = audio_trigger.clone();

		// Keep a handle to the video stream so a resuming client can reset its
		// frame counters (see `ActiveSession::reset_video_stream`).
		let video_handle_for_resume = video_handle.clone();

		let controls = SessionControls {
			video: video_handle.clone(),
			audio: audio_handle,
			audio_start: audio_trigger.clone(),
			input: control_stream.input_handler(),
			keepalive: KeepAlive::new(),
			kick_gamestream: Arc::new(AtomicBool::new(false)),
		};

		// Start control stream — receives both handles.
		let control_ctx = ControlStreamContext::new(&context, hdr_effective);
		control_stream.start(
			stream_timeout,
			control_ctx,
			video_handle,
			audio_trigger,
			hdr_metadata_rx,
			controls.keepalive.clone(),
			controls.kick_gamestream.clone(),
		);

		let sleep_inhibitor = if inhibit_sleep {
			SleepInhibitor::acquire().await
		} else {
			None
		};

		Ok((
			ActiveSession {
				context,
				_application: application,
				video_handle: video_handle_for_resume,
				sleep_inhibitor,
				video_format,
				audio_layout,
				ten_bit,
				started_with,
				controls,
				browser: None,
				attachment: Arc::new(AtomicU64::new(0)),
			},
			video_start,
			audio_start,
		))
	}
}

/// The parts of a running session that a client (Moonlight or a browser) drives.
#[derive(Clone)]
pub(crate) struct SessionControls {
	pub video: VideoStreamHandle,
	pub audio: AudioHandle,
	/// Starts the audio encoder (`StartB`).
	pub audio_start: StartSignal,
	pub input: Arc<InputHandler>,
	pub keepalive: KeepAlive,
	/// Set to end the Moonlight client's stream.
	pub kick_gamestream: Arc<AtomicBool>,
}

/// Active session state — streams are active.
pub(crate) struct ActiveSession {
	context: SessionContext,
	_application: Application,
	video_handle: VideoStreamHandle,
	/// Held while the session is active to keep the host awake; dropped on teardown.
	#[allow(dead_code)]
	sleep_inhibitor: Option<SleepInhibitor>,
	/// The codec of the session's encoder. A client must decode it to join.
	video_format: VideoFormat,
	/// The Opus layout of the session's audio. A client must decode it to join.
	audio_layout: crate::session::stream::audio::OpusStreamConfig,
	/// Whether the encoder makes 10-bit HDR video. A client must decode it to join.
	ten_bit: bool,
	/// The stream settings that the running encoders and packetizers use.
	started_with: (VideoStreamContext, AudioStreamContext),
	controls: SessionControls,
	/// The attached browser, if any.
	browser: Option<browser::Attachment>,
	/// Counts attachments; the client of the current one owns the sinks.
	attachment: Arc<AtomicU64>,
}

impl ActiveSession {
	pub(crate) fn context(&self) -> &SessionContext {
		&self.context
	}

	/// Why a resuming Moonlight client cannot use the running streams, if it cannot. The encoders
	/// and packetizers keep the settings they started with, and another client (or a browser)
	/// may have started them.
	pub(crate) fn resume_mismatch(&self, video: &VideoStreamContext, audio: &AudioStreamContext) -> Option<String> {
		let (running, running_audio) = &self.started_with;
		let differences = [
			(running.video_format != video.video_format)
				.then(|| format!("codec {:?} (asked: {:?})", running.video_format, video.video_format)),
			(running.dynamic_range != video.dynamic_range).then(|| {
				format!(
					"dynamic range {:?} (asked: {:?})",
					running.dynamic_range, video.dynamic_range
				)
			}),
			(running.chroma_sampling_type != video.chroma_sampling_type).then(|| {
				format!(
					"chroma {:?} (asked: {:?})",
					running.chroma_sampling_type, video.chroma_sampling_type
				)
			}),
			(running.packet_size != video.packet_size)
				.then(|| format!("packet size {} (asked: {})", running.packet_size, video.packet_size)),
			(running.encrypt_video != video.encrypt_video).then(|| {
				format!(
					"video encryption {} (asked: {})",
					running.encrypt_video, video.encrypt_video
				)
			}),
			(running_audio.encrypt_audio != audio.encrypt_audio).then(|| {
				format!(
					"audio encryption {} (asked: {})",
					running_audio.encrypt_audio, audio.encrypt_audio
				)
			}),
			(running_audio.audio_config.channels != audio.audio_config.channels
				|| running_audio.audio_config.high_quality != audio.audio_config.high_quality)
				.then(|| {
					format!(
						"audio {:?} (high quality {}) (asked: {:?}, high quality {})",
						running_audio.audio_config.channels,
						running_audio.audio_config.high_quality,
						audio.audio_config.channels,
						audio.audio_config.high_quality
					)
				}),
		];
		let differences: Vec<String> = differences.into_iter().flatten().collect();
		(!differences.is_empty()).then(|| differences.join(", "))
	}

	/// Arm a video stream reset (frame counters + forced IDR) for a resuming client.
	/// It fires once the client's first video PING arrives.
	pub(crate) fn reset_video_stream(&self) {
		self.video_handle.request_reset();
	}
}
