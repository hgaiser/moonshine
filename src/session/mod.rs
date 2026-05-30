use async_shutdown::ShutdownManager;
use manager::SessionShutdownReason;
use tokio::sync::watch;

use crate::config::ApplicationConfig;
use crate::config::Config;
use crate::session::application::Application;
use crate::session::application::ApplicationContext;
use crate::session::compositor::frame::HdrModeState;
use crate::session::compositor::Compositor;
use crate::session::compositor::LaunchedCompositor;
use crate::session::stream::AudioChannels;
use crate::session::stream::AudioStream;
use crate::session::stream::AudioStreamContext;
use crate::session::stream::ControlStream;
use crate::session::stream::ControlStreamContext;
use crate::session::stream::VideoStream;
use crate::session::stream::VideoStreamContext;

pub mod application;
pub mod compositor;
pub mod manager;
pub mod stream;

/// Timeout in seconds for the HTTP launch endpoint to wait for the session to launch.
pub const APP_LAUNCH_HTTP_TIMEOUT_SECS: u64 = 60;

/// Raw session encryption key data.
#[derive(Clone, Debug)]
pub struct SessionKeyData {
	/// AES GCM key used for encoding video / audio / control messages.
	pub remote_input_key: Vec<u8>,

	/// AES GCM initialization vector for video / audio / control messages.
	pub remote_input_key_id: i64,
}

pub type SessionKeysReceiver = watch::Receiver<SessionKeyData>;
pub type SessionKeysSender = watch::Sender<SessionKeyData>;

/// Session keys — either raw keys or a watch receiver.
#[derive(Clone, Debug)]
pub enum SessionKeys {
	Keys(SessionKeyData),
	Rx(SessionKeysReceiver),
}

impl SessionKeys {
	pub fn new(remote_input_key: Vec<u8>, remote_input_key_id: i64) -> Self {
		Self::Keys(SessionKeyData {
			remote_input_key,
			remote_input_key_id,
		})
	}

	pub fn clone_rx(&self) -> Option<SessionKeysReceiver> {
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
pub struct InitializedSession {
	context: SessionContext,
	compositor: Compositor,
	audio_stream: AudioStream,
	video_stream: VideoStream,
	control_stream: ControlStream,
	hdr_metadata_rx: watch::Receiver<HdrModeState>,
}

impl InitializedSession {
	pub async fn new(
		config: Config,
		context: SessionContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		// Create HDR metadata watch channel.
		let (hdr_metadata_tx, hdr_metadata_rx) = watch::channel(HdrModeState::new(context.hdr));

		// Create compositor, audio stream, video stream, and control stream.
		let (compositor, handles) = Compositor::new(config.compositor.clone(), (&context).into(), stop.clone());
		let audio = AudioStream::new(config.clone(), stop.clone()).await?;
		let video_stream = VideoStream::new(config.clone(), handles.frame_rx, hdr_metadata_tx, stop.clone()).await?;
		let control_stream = ControlStream::new(config.clone(), handles.input_tx, stop.clone())?;

		Ok(Self {
			context,
			compositor,
			audio_stream: audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
		})
	}

	pub fn context(&self) -> &SessionContext {
		&self.context
	}

	/// Launch the session — starts the compositor and application, but does not start streams.
	pub async fn launch(self) -> Result<LaunchedSession, ()> {
		let Self {
			context,
			compositor,
			audio_stream: audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
		} = self;

		let launched_compositor = compositor.launch()?;
		let ready = launched_compositor.ready();
		let pulse_socket_path = audio.pulse_socket_path.clone();

		let application = Application::spawn(
			context.application.clone(),
			ApplicationContext {
				unit_name: "moonshine-session.service".to_string(),
				pulse_socket_path,
				xdisplay: ready.xdisplay,
				wayland_display: ready.wayland_display.clone(),
				hdr: ready.hdr,
			},
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
pub struct LaunchedSession {
	context: SessionContext,
	application: Application,
	video_stream: VideoStream,
	launched_compositor: LaunchedCompositor,
	audio: AudioStream,
	control_stream: ControlStream,
	hdr_metadata_rx: watch::Receiver<HdrModeState>,
}

impl LaunchedSession {
	pub fn context(&self) -> &SessionContext {
		&self.context
	}

	pub fn start(
		self,
		video_ctx: VideoStreamContext,
		audio_ctx: AudioStreamContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<ActiveSession, ()> {
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

		// Start video stream — gated, returns VideoStreamHandle.
		let video_handle = video_stream
			.start(video_ctx, keys_rx.clone(), stop.clone())
			.map_err(|()| tracing::error!("Failed to start video stream"))?;

		// Start audio stream — gated, returns AudioStartHandle.
		let audio_trigger = audio
			.start(audio_ctx, keys_rx)
			.map_err(|()| tracing::error!("Failed to start audio stream"))?;

		// Start control stream — receives both handles.
		let control_ctx = ControlStreamContext::new(&context, hdr_effective);
		control_stream.start(control_ctx, video_handle, audio_trigger, hdr_metadata_rx);

		Ok(ActiveSession {
			context,
			_application: application,
		})
	}
}

/// Active session state — streams are active.
pub struct ActiveSession {
	context: SessionContext,
	_application: Application,
}

impl ActiveSession {
	pub fn context(&self) -> &SessionContext {
		&self.context
	}
}
