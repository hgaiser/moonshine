use std::os::unix::process::CommandExt as _;
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

	/// Whether to play audio on the host.
	pub host_audio: bool,
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
	sink_name: Option<String>,
}

fn create_audio_sink(name: &str) -> Result<String, ()> {
	let output = Command::new("pactl")
		.arg("load-module")
		.arg("module-null-sink")
		.arg(format!("sink_name={}", name))
		.arg(format!("sink_properties=device.description={}", name))
		.output()
		.map_err(|e| tracing::warn!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::warn!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
		return Err(());
	}

	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(stdout.trim().to_string())
}

fn unload_audio_sink(module_id: &str) {
	let _ = Command::new("pactl").arg("unload-module").arg(module_id).output();
}

fn create_audio_loopback(source: &str, sink: &str) -> Result<String, ()> {
	let output = Command::new("pactl")
		.arg("load-module")
		.arg("module-loopback")
		.arg(format!("source={}.monitor", source))
		.arg(format!("sink={}", sink))
		.output()
		.map_err(|e| tracing::warn!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::warn!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
		return Err(());
	}

	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(stdout.trim().to_string())
}

fn get_default_sink() -> Result<String, ()> {
	let output = Command::new("pactl")
		.arg("get-default-sink")
		.output()
		.map_err(|e| tracing::warn!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::warn!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
		return Err(());
	}

	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(stdout.trim().to_string())
}

fn set_default_sink(name: &str) -> Result<(), ()> {
	let output = Command::new("pactl")
		.arg("set-default-sink")
		.arg(name)
		.output()
		.map_err(|e| tracing::warn!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::warn!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
		return Err(());
	}

	Ok(())
}

#[allow(clippy::result_unit_err)]
impl Session {
	pub fn new(
		config: Config,
		context: SessionContext,
		stop_session_signal: ShutdownManager<SessionShutdownReason>,
		enet: Arc<Enet>,
	) -> Result<Self, ()> {
		let default_sink = get_default_sink().ok();
		let sink_name = "moonshine-sink".to_string();
		let module_id = create_audio_sink(&sink_name)?;

		if let Some(sink) = &default_sink {
			let _ = set_default_sink(sink);
		}

		let loopback_module_id = if context.host_audio {
			if let Some(default_sink) = default_sink {
				create_audio_loopback(&sink_name, &default_sink).ok()
			} else {
				tracing::warn!("Could not determine default sink for loopback.");
				None
			}
		} else {
			None
		};

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
		let app_sink = sink_name.clone();
		let app_handle = std::thread::Builder::new()
			.name("app-launcher".to_string())
			.spawn(move || -> Result<Child, ()> {
				let xdisplay = xdisplay_rx
					.recv_timeout(std::time::Duration::from_secs(5))
					.map_err(|e| tracing::warn!("Timed out waiting for XWayland display: {e}"))?;
				launch_application(&app_context, &app_sink, xdisplay)
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
			app_launcher: Some(app_handle),
			audio_sink_module_id: Some(module_id),
			audio_loopback_module_id: loopback_module_id,
			enet,
		};
		tokio::spawn(inner.run(command_rx, context.clone(), stop_session_signal));
		Ok(Self {
			command_tx,
			context,
			running: false,
			sink_name: Some(sink_name),
		})
	}

	pub async fn start(
		&mut self,
		video_stream_context: VideoStreamContext,
		mut audio_stream_context: AudioStreamContext,
	) -> Result<(), ()> {
		tracing::info!("Starting session.");
		self.running = true;
		audio_stream_context.sink_name = self.sink_name.clone();
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
	app_launcher: Option<std::thread::JoinHandle<Result<Child, ()>>>,
	audio_sink_module_id: Option<String>,
	audio_loopback_module_id: Option<String>,
	enet: Arc<Enet>,
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
					let audio_stream =
						match AudioStream::new(self.config.clone(), audio_stream_context, stop_session_manager.clone())
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

		// Collect the app process from the launcher thread and terminate
		// the entire process group. The child was launched with
		// `process_group(0)` so it is the leader of its own group.
		// Sending the signal to the group ensures that sub-processes
		// spawned by the app (e.g. Steam's children) are also terminated.
		if let Some(handle) = self.app_launcher {
			if let Ok(Ok(ref child)) = handle.join() {
				let pid = child.id() as libc::pid_t;
				// Send SIGTERM to the entire process group (negative pid).
				unsafe {
					libc::kill(-pid, libc::SIGTERM);
				}
			}
		}

		if let Some(module_id) = self.audio_loopback_module_id {
			unload_audio_sink(&module_id);
		}

		if let Some(module_id) = self.audio_sink_module_id {
			unload_audio_sink(&module_id);
		}

		tracing::debug!("Session stopped.");
	}
}

/// Launch the application as a child process.
///
/// The compositor has already set `WAYLAND_DISPLAY` in the process
/// environment, so the child inherits it and connects to our
/// headless compositor automatically.
fn launch_application(context: &SessionContext, sink_name: &str, xdisplay: u32) -> Result<Child, ()> {
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

	Command::new(program)
		.args(args)
		.env("PULSE_SINK", sink_name)
		// Set DISPLAY so X11 apps connect to our XWayland instance.
		// WAYLAND_DISPLAY is already set by the compositor for native
		// Wayland apps.
		.env("DISPLAY", format!(":{xdisplay}"))
		// Launch in a new process group so we can kill the entire tree
		// on session stop (important for apps like Steam that spawn
		// many sub-processes).
		.process_group(0)
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
