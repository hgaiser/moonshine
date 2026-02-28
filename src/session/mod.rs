use std::process::Command;
use std::process::{Child, Stdio};
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use enet::Enet;
use manager::SessionShutdownReason;
use tokio::sync::mpsc;

use crate::{
	config::{ApplicationConfig, Config},
	session::stream::{AudioStream, ControlStream, VideoStream},
};

use self::compositor::frame::ExportedFrame;
use self::compositor::input::CompositorInputEvent;
use self::stream::{AudioStreamContext, VideoStreamContext};
pub use manager::SessionManager;

pub mod compositor;
pub mod manager;
pub mod stream;

#[derive(Clone, Debug)]
pub struct SessionKeys {
	/// AES GCM key used for encoding control messages.
	pub remote_input_key: Vec<u8>,

	/// AES GCM initialization vector for control messages.
	pub remote_input_key_id: i64,
}

/// Launch a session for a client.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SessionContext {
	/// Application to launch.
	pub application: ApplicationConfig,

	/// Id of the application as reported to the client.
	pub application_id: i32,

	/// Resolution of the video stream.
	pub resolution: (u32, u32),

	/// Refresh rate of the video stream.
	pub _refresh_rate: u32,

	/// Encryption keys for encoding traffic.
	pub keys: SessionKeys,

	/// Audio channel count (2, 6, or 8).
	pub audio_channels: u8,

	/// Audio channel mask (Windows SPEAKER_ bitmask).
	pub audio_channel_mask: u32,
}

enum SessionCommand {
	Start(VideoStreamContext, AudioStreamContext),
	UpdateKeys(SessionKeys),
}

#[derive(Clone)]
pub struct Session {
	command_tx: mpsc::Sender<SessionCommand>,
	context: SessionContext,
	running: bool,
}

#[allow(clippy::result_unit_err)]
impl Session {
	pub fn new(
		config: Config,
		context: SessionContext,
		stop_session_signal: ShutdownManager<SessionShutdownReason>,
		enet: Arc<Enet>,
	) -> Result<Self, ()> {
		// Create the socket directory for the PulseAudio server.
		let runtime_dir =
			std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
		let pulse_dir = std::path::Path::new(&runtime_dir).join("moonshine/pulse");
		std::fs::create_dir_all(&pulse_dir)
			.map_err(|e| tracing::error!("Failed to create pulse socket directory: {e}"))?;
		let socket_path = pulse_dir.join("native");

		// Remove any stale socket file from a previous session.
		let _ = std::fs::remove_file(&socket_path);

		// Bind the PulseAudio socket before launching the application so that
		// the app can connect as soon as it starts.
		let listener = std::os::unix::net::UnixListener::bind(&socket_path)
			.map_err(|e| tracing::error!("Failed to bind PulseAudio socket: {e}"))?;

		// Start the headless compositor.
		let compositor_config = compositor::CompositorConfig {
			width: context.resolution.0,
			height: context.resolution.1,
			refresh_rate: context._refresh_rate,
			gpu: config.gpu.clone(),
		};
		let (frame_rx, input_tx, xdisplay_rx) =
			compositor::start_compositor(compositor_config, stop_session_signal.clone())
				.map_err(|e| tracing::warn!("Failed to start compositor: {e}"))?;

		// Launch the application in a background thread that waits for
		// XWayland to become ready. We must not block Session::new()
		// because the session manager processes commands sequentially
		// and stalling it would prevent the control stream from being
		// established on time.
		let app_context = context.clone();
		let app_socket_path = socket_path.clone();
		std::thread::Builder::new()
			.name("app-launcher".to_string())
			.spawn(move || -> Result<Child, ()> {
				let xdisplay = xdisplay_rx
					.recv_timeout(std::time::Duration::from_secs(5))
					.map_err(|e| tracing::warn!("Timed out waiting for XWayland display: {e}"))?;
				launch_application(&app_context, &app_socket_path, xdisplay)
			})
			.map_err(|e| tracing::warn!("Failed to spawn app launcher thread: {e}"))?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = SessionInner {
			config,
			video_stream: None,
			audio_stream: None,
			control_stream: None,
			frame_rx: Some(frame_rx),
			input_tx: Some(input_tx),
			enet,
			listener: Some(listener),
		};
		tokio::spawn(inner.run(command_rx, context.clone(), stop_session_signal));
		Ok(Self {
			command_tx,
			context,
			running: false,
		})
	}

	pub async fn start(
		&mut self,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
	) -> Result<(), ()> {
		tracing::info!("Starting session.");
		self.running = true;
		self.command_tx
			.send(SessionCommand::Start(video_stream_context, audio_stream_context))
			.await
			.map_err(|e| tracing::warn!("Failed to send Start command: {e}"))
	}

	pub fn context(&self) -> &SessionContext {
		&self.context
	}

	pub fn is_running(&self) -> bool {
		self.running
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx
			.send(SessionCommand::UpdateKeys(keys))
			.await
			.map_err(|e| tracing::warn!("Failed to send UpdateKeys command: {e}"))
	}
}

struct SessionInner {
	config: Config,
	video_stream: Option<VideoStream>,
	audio_stream: Option<AudioStream>,
	control_stream: Option<ControlStream>,
	frame_rx: Option<std::sync::mpsc::Receiver<ExportedFrame>>,
	input_tx: Option<calloop::channel::Sender<CompositorInputEvent>>,
	enet: Arc<Enet>,
	listener: Option<std::os::unix::net::UnixListener>,
}

impl SessionInner {
	async fn run(
		mut self,
		mut command_rx: mpsc::Receiver<SessionCommand>,
		mut session_context: SessionContext,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		// Create a token that will trigger the shutdown of the session when the token is dropped.
		let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::SessionStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		while let Ok(Some(command)) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
			match command {
				SessionCommand::Start(video_stream_context, audio_stream_context) => {
					let frame_rx = self.frame_rx.take();
					let video_stream = match VideoStream::new(
						self.config.clone(),
						video_stream_context,
						frame_rx,
						stop_session_manager.clone(),
					)
					.await
					{
						Ok(video_stream) => video_stream,
						Err(()) => continue,
					};
					let Some(listener) = self.listener.take() else {
						tracing::error!("No listener available for audio stream.");
						continue;
					};
					let audio_stream = match AudioStream::new(
						self.config.clone(),
						audio_stream_context,
						listener,
						stop_session_manager.clone(),
					)
					.await
					{
						Ok(audio_stream) => audio_stream,
						Err(()) => continue,
					};
					let input_tx = self.input_tx.take().expect("Input sender already consumed");
					let control_stream = match ControlStream::new(
						self.config.clone(),
						video_stream.clone(),
						audio_stream.clone(),
						session_context.clone(),
						stop_session_manager.clone(),
						self.enet.clone(),
						input_tx,
					) {
						Ok(control_stream) => control_stream,
						Err(()) => {
							tracing::error!("Failed to create control stream, killing session.");
							continue;
						},
					};

					self.video_stream = Some(video_stream);
					self.audio_stream = Some(audio_stream);
					self.control_stream = Some(control_stream);
				},

				SessionCommand::UpdateKeys(keys) => {
					let Some(audio_stream) = &self.audio_stream else {
						tracing::warn!("Can't update session keys without an audio stream.");
						continue;
					};
					let Some(control_stream) = &self.control_stream else {
						tracing::warn!("Can't update session keys without a control stream.");
						continue;
					};

					session_context.keys = keys.clone();
					let _ = audio_stream.update_keys(keys.clone()).await;
					let _ = control_stream.update_keys(keys).await;
				},
			}
		}

		// Stop the systemd scope to kill the application and all of its
		// descendants. The scope was created with TimeoutStopSec=5, so
		// this blocks at most 5 seconds before systemd sends SIGKILL.
		let _ = Command::new("systemctl")
			.args(["--user", "stop", "moonshine-session.scope"])
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status();

		tracing::debug!("Session stopped.");
	}
}

/// Launch the application as a child process.
///
/// The compositor has already set `WAYLAND_DISPLAY` in the process
/// environment, so the child inherits it and connects to our
/// headless compositor automatically.
fn launch_application(context: &SessionContext, socket_path: &std::path::Path, xdisplay: u32) -> Result<Child, ()> {
	let Some(program) = context.application.command.first() else {
		tracing::warn!("Application command is empty.");
		return Err(());
	};
	let args = &context.application.command[1..];

	tracing::info!(program, ?args, "Launching application");

	let log_dir = std::env::temp_dir().join("moonshine");
	std::fs::create_dir_all(&log_dir).map_err(|e| tracing::warn!("Failed to create log directory: {e}"))?;
	let log_path = log_dir.join(format!("app-{}.log", context.application_id));
	tracing::debug!("Application log path: {}", log_path.display());

	let log_file = std::fs::File::create(&log_path).map_err(|e| tracing::warn!("Failed to create log file: {e}"))?;

	// Stop any leftover scope from a previous session before starting a new one.
	let _ = Command::new("systemctl")
		.args(["--user", "stop", "moonshine-session.scope"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status();

	Command::new("systemd-run")
		.args([
			"--user",
			"--scope",
			"--collect",
			"--unit",
			"moonshine-session",
			"--property=TimeoutStopSec=5",
			"--",
		])
		.arg(program)
		.args(args)
		.env("PULSE_SERVER", format!("unix:{}", socket_path.display()))
		.env("DISPLAY", format!(":{xdisplay}"))
		.stdout(
			log_file
				.try_clone()
				.map_err(|e| tracing::warn!("Failed to clone log file handle: {e}"))?,
		)
		.stderr(log_file)
		.stdin(Stdio::null())
		.spawn()
		.map_err(|e| tracing::warn!("Failed to launch application: {e}"))
}
