use std::collections::HashMap;
use std::net::IpAddr;

use async_shutdown::ShutdownManager;
use manager::SessionShutdownReason;
use tokio::sync::watch;

use crate::session::compositor::CompositorConfig;
use crate::session::stream::audio::AudioChannels;
use crate::session::stream::audio::AudioStream;
use crate::session::stream::audio::AudioStreamContext;
use crate::session::stream::control::ControlStream;
use crate::session::stream::control::ControlStreamContext;
use crate::session::stream::video::FrameStats;
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
use self::lifecycle::StartLatch;
use self::manager::{ResumePlan, SessionBackend, StartRequest};
use self::stream::audio::AudioStreamConfig;
use self::stream::control::ControlStreamConfig;
use self::stream::video::VideoStreamConfig;

pub mod application;
pub mod authorization;
pub mod compositor;
pub mod inhibit;
pub mod keys;
pub(crate) mod lifecycle;
pub mod manager;
pub(crate) mod negotiation;
pub mod stream;

/// Timeout in seconds for the HTTP launch endpoint to wait for the session to launch.
///
/// The launch itself is owned by the session manager, not the HTTP request:
/// when this expires the handler stops the session, which cancels the launch
/// and stops any transient unit systemd already created for it.
pub(crate) const APP_LAUNCH_HTTP_TIMEOUT_SECS: u64 = 60;

/// Fixed name of the application's transient user-systemd unit. Only one
/// session exists at a time, and a replacement cannot launch until the previous
/// session's teardown has stopped this unit.
pub(crate) const APPLICATION_UNIT_NAME: &str = "moonshine-session.service";

pub use self::keys::{RemoteInputKey, RemoteInputKeyId, SessionKeyData};

pub(crate) type SessionKeysReceiver = watch::Receiver<keys::ActiveKeys>;
pub(crate) type SessionKeysSender = watch::Sender<keys::ActiveKeys>;
pub(crate) type AuthorizationReceiver = watch::Receiver<authorization::StreamAuthorization>;

/// Session keys — validated keys from launch, then the manager-published watch.
#[derive(Clone, Debug)]
pub enum SessionKeys {
	Keys(SessionKeyData),
	Rx(SessionKeysReceiver),
}

impl SessionKeys {
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

	/// Address of the paired client that authenticated the launch. RTSP, control
	/// and media endpoint discovery are bound to it (see `authorization`).
	pub client_ip: IpAddr,
}

/// Session-level values carried by the authenticated HTTP `/resume` request.
/// RTSP ANNOUNCE remains authoritative for encoded stream properties; these
/// values are retained to drive and diagnose compositor/capture changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResumeRequest {
	pub resolution: Option<(u32, u32)>,
	pub refresh_rate: Option<u32>,
	pub hdr: Option<bool>,
	pub audio_channels: Option<AudioChannels>,
	pub audio_channel_mask: Option<u32>,
}

/// Configuration and shared channels used to build production sessions.
///
/// This is the manager's production [`SessionBackend`]: it owns the slow,
/// resource-creating work for each transition while the manager owns state,
/// generations, cancellation and teardown ordering.
pub(crate) struct SystemSession {
	pub(crate) compositor_config: CompositorConfig,
	pub(crate) video_config: VideoStreamConfig,
	pub(crate) audio_config: AudioStreamConfig,
	pub(crate) control_config: ControlStreamConfig,
	pub(crate) address: String,
	pub(crate) stream_timeout: u64,
	pub(crate) inhibit_sleep: bool,
	pub(crate) stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
}

impl SessionBackend for SystemSession {
	type Initialized = InitializedSession;
	type Launched = LaunchedSession;
	type Active = ActiveSession;

	async fn initialize(
		&self,
		context: SessionContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<InitializedSession, ()> {
		InitializedSession::new(
			self.compositor_config.clone(),
			self.video_config.clone(),
			self.audio_config.clone(),
			self.control_config.clone(),
			self.address.clone(),
			context,
			stop,
			self.stats_tx.clone(),
		)
		.await
	}

	async fn launch(&self, session: InitializedSession) -> Result<LaunchedSession, ()> {
		session.launch().await
	}

	async fn start(
		&self,
		session: LaunchedSession,
		request: StartRequest,
	) -> Result<(ActiveSession, Vec<StartLatch>), ()> {
		// Acquire before consuming the session: nothing is owned yet if this
		// await is cancelled.
		let sleep_inhibitor = if self.inhibit_sleep {
			SleepInhibitor::acquire().await
		} else {
			None
		};
		session.start(self.video_config.clone(), self.stream_timeout, request, sleep_inhibitor)
	}

	fn pause(
		&self,
		session: &ActiveSession,
		video: bool,
		audio: bool,
	) -> impl Future<Output = Result<(), ()>> + Send + 'static {
		let video = video.then(|| session.video_handle.clone());
		let audio = audio.then(|| session.audio_handle.clone());
		async move {
			if let Some(handle) = video {
				handle.pause_for_reconfigure().await.map_err(|()| {
					tracing::warn!("Failed to pause the active video epoch for reconnect reconfiguration")
				})?;
			}
			if let Some(handle) = audio {
				handle.pause_for_reconfigure().await.map_err(|()| {
					tracing::warn!("Failed to pause the active audio epoch for reconnect reconfiguration")
				})?;
			}
			Ok(())
		}
	}

	async fn resume(&self, session: &mut ActiveSession, plan: ResumePlan) -> Result<(), ()> {
		match plan.video {
			Some(context) => session.reconfigure_video(context).await?,
			None => session.reset_video_stream().await?,
		}
		if let Some(context) = plan.audio {
			session.reconfigure_audio(context).await?;
		}
		Ok(())
	}

	async fn stop_application(&self, unit_name: &str) -> Result<(), ()> {
		application::stop_application_unit(unit_name).await
	}
}

/// Initialized session state — components created, compositor and app not yet started.
pub(crate) struct InitializedSession {
	context: SessionContext,
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
		stop: ShutdownManager<SessionShutdownReason>,
		stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
	) -> Result<Self, ()> {
		// Create HDR metadata watch channel.
		let (hdr_metadata_tx, hdr_metadata_rx) = watch::channel(HdrModeState::new(context.hdr));

		// Create compositor, audio stream, video stream, and control stream.
		let (compositor, handles) = Compositor::new(
			compositor_config,
			compositor::CompositorContext::from_session(&context, video_config.log_stats),
			stop.clone(),
		);
		let audio = AudioStream::new(audio_config, address.clone(), stop.clone()).await?;
		let video_stream = VideoStream::new(
			video_config.clone(),
			address.clone(),
			handles.frame_rx,
			hdr_metadata_tx,
			stop.clone(),
			stats_tx,
		)
		.await?;
		let control_stream = ControlStream::new(control_config, address, handles.input_tx, stop.clone())?;

		Ok(Self {
			context,
			compositor,
			audio_stream: audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
			stop,
		})
	}

	/// Launch the session — starts the compositor and application, but does not start streams.
	///
	/// The manager records the application unit before calling this, so a
	/// cancellation after systemd accepted the unit is still cleaned up.
	pub(crate) async fn launch(self) -> Result<LaunchedSession, ()> {
		let Self {
			context,
			compositor,
			audio_stream: audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
			stop,
		} = self;

		// Waiting for the compositor's readiness blocks; keep it off the runtime
		// workers. A cancelled launch drops the result, and the compositor thread
		// exits on the session stop that cancelled it.
		let launched_compositor = tokio::task::spawn_blocking(move || compositor.launch())
			.await
			.map_err(|e| tracing::error!("Compositor launch task failed: {e}"))??;
		let ready = launched_compositor.ready();
		let pulse_socket_path = audio.pulse_socket_path.clone();

		let application = Application::spawn(
			context.application.clone(),
			ApplicationContext {
				unit_name: APPLICATION_UNIT_NAME.to_string(),
				pulse_socket_path,
				xdisplay: ready.xdisplay,
				wayland_display: ready.wayland_display.clone(),
				// Keep HDR-capable application paths enabled so a later reconnect can
				// switch SDR/HDR without relaunching the application.
				hdr: ready.hdr_capable,
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
	/// Spawn every stream worker. Each worker is registered with the session
	/// before it is spawned and waits on its stream's start latch.
	///
	/// Synchronous by design: once this consumes the session, no await point
	/// can drop half-started streams.
	pub(crate) fn start(
		self,
		video_config: VideoStreamConfig,
		stream_timeout: u64,
		request: StartRequest,
		sleep_inhibitor: Option<SleepInhibitor>,
	) -> Result<(ActiveSession, Vec<StartLatch>), ()> {
		let Self {
			context,
			launched_compositor,
			application,
			audio,
			video_stream,
			control_stream,
			hdr_metadata_rx,
		} = self;
		let StartRequest {
			video: video_ctx,
			audio: audio_ctx,
			authorization_rx,
			stop,
		} = request;

		// Extract the watch receiver for streams.
		let keys_rx = context.keys.clone_rx().ok_or_else(|| {
			tracing::error!("Session keys not initialized");
		})?;

		// Start video stream — gated, returns VideoStreamHandle.
		let video_handle = video_stream
			.start(
				video_config,
				video_ctx.clone(),
				keys_rx.clone(),
				authorization_rx.clone(),
				stop.clone(),
			)
			.map_err(|()| tracing::error!("Failed to start video stream"))?;

		// Start audio stream — gated, returns AudioStartHandle.
		let audio_trigger = audio
			.start(audio_ctx.clone(), keys_rx, authorization_rx.clone())
			.map_err(|()| tracing::error!("Failed to start audio stream"))?;

		// Start latches for external triggering (e.g. bench binary).
		let latches = vec![video_handle.start_latch(), audio_trigger.start_latch()];
		let audio_handle_for_resume = audio_trigger.clone();

		// Keep a handle to the video stream so a resuming client can reset its
		// frame counters (see `ActiveSession::reset_video_stream`).
		let video_handle_for_resume = video_handle.clone();

		// Start control stream — receives both handles.
		let control_ctx = ControlStreamContext::new(&context, authorization_rx);
		control_stream.start(
			stream_timeout,
			control_ctx,
			video_handle,
			audio_trigger,
			hdr_metadata_rx,
		);

		Ok((
			ActiveSession {
				_application: application,
				compositor: launched_compositor,
				video_handle: video_handle_for_resume,
				audio_handle: audio_handle_for_resume,
				video_context: video_ctx,
				audio_context: audio_ctx,
				sleep_inhibitor,
			},
			latches,
		))
	}
}

/// Active session state — streams are active.
///
/// The authoritative session context (as reported to HTTP/RTSP) lives in the
/// manager's session record; this keeps only what reconfiguration consumes.
pub(crate) struct ActiveSession {
	_application: Application,
	compositor: LaunchedCompositor,
	video_handle: VideoStreamHandle,
	audio_handle: stream::audio::AudioStartHandle,
	video_context: VideoStreamContext,
	audio_context: AudioStreamContext,
	/// Held while the session is active to keep the host awake; dropped on teardown.
	#[allow(dead_code)]
	sleep_inhibitor: Option<SleepInhibitor>,
}

impl ActiveSession {
	/// Reset the video stream's frame counters and force an IDR for a resuming client.
	pub(crate) async fn reset_video_stream(&self) -> Result<(), ()> {
		self.video_handle.request_reset().await
	}

	pub(crate) async fn reconfigure_video(&mut self, context: VideoStreamContext) -> Result<(), ()> {
		let effective_hdr = self
			.compositor
			.reconfigure(context.width, context.height, context.fps, context.format.hdr)
			.await?;
		if effective_hdr != context.format.hdr {
			tracing::warn!(
				requested_hdr = context.format.hdr,
				effective_hdr,
				"Compositor could not apply the negotiated HDR mode"
			);
			return Err(());
		}
		self.video_handle.reconfigure(context.clone()).await?;
		self.video_context = context;
		Ok(())
	}

	pub(crate) async fn reconfigure_audio(&mut self, context: AudioStreamContext) -> Result<(), ()> {
		let reconfigure_capture = self.audio_context.packet_duration_ms != context.packet_duration_ms
			|| self.audio_context.audio_config.channels != context.audio_config.channels;
		self.audio_handle
			.reconfigure(context.clone(), reconfigure_capture)
			.await?;
		self.audio_context = context;
		Ok(())
	}
}
