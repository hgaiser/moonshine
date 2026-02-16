use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use enet::Enet;
use manager::SessionShutdownReason;
use tokio::sync::mpsc;

use crate::{
	config::{ApplicationConfig, Config},
	session::stream::{AudioStream, ControlStream, VideoStream},
};

use self::stream::{AudioStreamContext, VideoStreamContext};
pub use manager::SessionManager;

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
	sink_name: Option<String>,
}

fn create_audio_sink(name: &str) -> Result<String, ()> {
	let output = Command::new("pactl")
		.arg("load-module")
		.arg("module-null-sink")
		.arg(format!("sink_name={}", name))
		.arg(format!("sink_properties=device.description={}", name))
		.output()
		.map_err(|e| tracing::error!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::error!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
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
		.map_err(|e| tracing::error!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::error!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
		return Err(());
	}

	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(stdout.trim().to_string())
}

fn get_default_sink() -> Result<String, ()> {
	let output = Command::new("pactl")
		.arg("get-default-sink")
		.output()
		.map_err(|e| tracing::error!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::error!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
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
		.map_err(|e| tracing::error!("Failed to run pactl: {e}"))?;

	if !output.status.success() {
		tracing::error!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
		return Err(());
	}

	Ok(())
}

#[allow(clippy::result_unit_err)]
impl Session {
	pub async fn new(
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

		// Shut down existing Steam before launching gamescope if needed.
		if context.application.enable_steam_integration {
			shutdown_steam().await;
		}

		let gamescope_process = start_gamescope(&context, &sink_name)?;

		// Don't wait for gamescope's PipeWire node here - that would block the
		// session manager and prevent RTSP commands from being processed.
		// The video stream discovers the node when StartB triggers capture.

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = SessionInner {
			config,
			video_stream: None,
			audio_stream: None,
			control_stream: None,
			gamescope_process: Some(gamescope_process),
			audio_sink_module_id: Some(module_id),
			audio_loopback_module_id: loopback_module_id,
			enet,
		};
		tokio::spawn(inner.run(command_rx, context.clone(), stop_session_signal));
		Ok(Self {
			command_tx,
			context,
			sink_name: Some(sink_name),
		})
	}

	pub async fn start(
		&mut self,
		video_stream_context: VideoStreamContext,
		mut audio_stream_context: AudioStreamContext,
	) -> Result<(), ()> {
		tracing::info!("Starting session streams.");
		audio_stream_context.sink_name = self.sink_name.clone();
		self.command_tx
			.send(SessionCommand::Start(video_stream_context, audio_stream_context))
			.await
			.map_err(|e| tracing::error!("Failed to send Start command: {e}"))
	}

	pub fn context(&self) -> &SessionContext {
		&self.context
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx
			.send(SessionCommand::UpdateKeys(keys))
			.await
			.map_err(|e| tracing::error!("Failed to send UpdateKeys command: {e}"))
	}
}

struct SessionInner {
	config: Config,
	video_stream: Option<VideoStream>,
	audio_stream: Option<AudioStream>,
	control_stream: Option<ControlStream>,
	gamescope_process: Option<Child>,
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
					// Create a stream-level shutdown manager for this connection.
					// Client disconnect triggers this, stopping streams but keeping
					// gamescope alive for reconnection.
					let stop_stream_manager = ShutdownManager::new();

					let video_stream =
						match VideoStream::new(self.config.clone(), video_stream_context, stop_stream_manager.clone())
							.await
						{
							Ok(video_stream) => video_stream,
							Err(()) => continue,
						};
					let audio_stream =
						match AudioStream::new(self.config.clone(), audio_stream_context, stop_stream_manager.clone())
							.await
						{
							Ok(audio_stream) => audio_stream,
							Err(()) => continue,
						};
					let control_stream = match ControlStream::new(
						self.config.clone(),
						video_stream.clone(),
						audio_stream.clone(),
						session_context.clone(),
						stop_stream_manager,
						self.enet.clone(),
					)
					.await
					{
						Ok(control_stream) => control_stream,
						Err(()) => {
							tracing::error!("Failed to create control stream.");
							continue;
						},
					};

					self.video_stream = Some(video_stream);
					self.audio_stream = Some(audio_stream);
					self.control_stream = Some(control_stream);
				},

				SessionCommand::UpdateKeys(keys) => {
					// Always save keys for the next stream start (reconnection case).
					session_context.keys = keys.clone();

					// If streams are active, update them too.
					if let Some(audio_stream) = &self.audio_stream {
						let _ = audio_stream.update_keys(keys.clone()).await;
					}
					if let Some(control_stream) = &self.control_stream {
						let _ = control_stream.update_keys(keys).await;
					}
				},
			}
		}

		// Slow cleanup (process termination, Steam restart) runs in background.
		// The game may need time to save state before exiting.
		let gamescope_process = self.gamescope_process.take();
		let enable_steam = session_context.application.enable_steam_integration;
		tokio::spawn(async move {
			if let Some(child) = gamescope_process {
				graceful_terminate(child, 30).await;
			}
			if enable_steam {
				restart_host_steam();
			}
			tracing::debug!("Background session cleanup complete.");
		});

		// Fast audio cleanup stays in-line to prevent sink name conflicts
		// if a new session starts immediately.
		let loopback_id = self.audio_loopback_module_id.take();
		let sink_id = self.audio_sink_module_id.take();
		let _ = tokio::task::spawn_blocking(move || {
			if let Some(id) = loopback_id {
				unload_audio_sink(&id);
			}
			if let Some(id) = sink_id {
				unload_audio_sink(&id);
			}
		})
		.await;

		tracing::debug!("Session stopped.");
	}
}

/// Check if Steam is currently running.
fn is_steam_running() -> bool {
	Command::new("pgrep")
		.arg("-x")
		.arg("steam")
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

/// Shut down an existing Steam instance and poll for it to exit.
/// Tries graceful shutdown first, then force-kills if needed.
async fn shutdown_steam() {
	if !is_steam_running() {
		tracing::debug!("Steam is not running, skipping shutdown");
		return;
	}

	tracing::debug!("Stopping existing Steam instance");
	let _ = Command::new("steam").arg("-shutdown").output();

	// Poll for graceful exit (5 seconds).
	for _ in 0..25 {
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;
		if !is_steam_running() {
			tracing::debug!("Steam has exited gracefully");
			return;
		}
	}

	// Force kill if graceful shutdown didn't work.
	tracing::warn!("Steam didn't respond to graceful shutdown, force killing");
	let _ = Command::new("pkill").arg("-x").arg("steam").output();

	for _ in 0..10 {
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;
		if !is_steam_running() {
			tracing::debug!("Steam has exited after force kill");
			return;
		}
	}
	tracing::warn!("Steam still running after force kill, proceeding anyway");
}

/// Send SIGTERM to the gamescope process group, poll for exit, then SIGKILL as fallback.
async fn graceful_terminate(mut child: Child, timeout_secs: u64) {
	let pid = child.id() as i32;

	// SIGTERM to the process group (negative PID targets entire group).
	// safe because we set process_group(0) on spawn.
	unsafe {
		libc::kill(-pid, libc::SIGTERM);
	}

	let polls = timeout_secs * 2;
	for _ in 0..polls {
		tokio::time::sleep(Duration::from_millis(500)).await;
		match child.try_wait() {
			Ok(Some(status)) => {
				tracing::info!("Gamescope exited with status: {status}");
				return;
			},
			Ok(None) => continue,
			Err(e) => {
				tracing::warn!("Error checking gamescope status: {e}");
				break;
			},
		}
	}

	tracing::warn!("Gamescope did not exit after {timeout_secs}s SIGTERM, sending SIGKILL");
	let _ = child.kill();
	let _ = child.wait();
}

/// Restart Steam on the host so it's available after session ends.
fn restart_host_steam() {
	tracing::debug!("Restarting Steam on host");
	let _ = Command::new("setsid")
		.args(["steam", "-silent"])
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn();
}

fn start_gamescope(context: &SessionContext, sink_name: &str) -> Result<Child, ()> {
	let width = context.resolution.0.to_string();
	let height = context.resolution.1.to_string();
	let refresh_rate = context._refresh_rate.to_string();

	let mut command = vec![
		"--backend".to_string(),
		"headless".to_string(),
		"-w".to_string(),
		width.clone(),
		"-h".to_string(),
		height.clone(),
		"-W".to_string(),
		width,
		"-H".to_string(),
		height,
		"-r".to_string(),
		refresh_rate,
		"--immediate-flips".to_string(),
		"--force-grab-cursor".to_string(),
	];

	if context.application.enable_steam_integration {
		command.push("--steam".to_string());
	}

	command.push("--".to_string());
	command.extend(context.application.command.clone());

	tracing::debug!("Starting gamescope with command: {:?}", command);

	let log_dir = std::env::temp_dir().join("moonshine");
	std::fs::create_dir_all(&log_dir).map_err(|e| tracing::error!("Failed to create log directory: {e}"))?;
	let log_path = log_dir.join(format!("gamescope-{}.log", context.application_id));
	tracing::debug!("Gamescope log path: {}", log_path.display());
	let log_file = std::fs::File::create(&log_path).map_err(|e| tracing::error!("Failed to create log file: {e}"))?;

	Command::new("gamescope")
		.args(command)
		.env("PULSE_SINK", sink_name)
		.process_group(0) // Own process group so SIGTERM reaches entire tree.
		.stdout(
			log_file
				.try_clone()
				.map_err(|e| tracing::error!("Failed to clone log file handle: {e}"))?,
		)
		.stderr(log_file)
		.stdin(Stdio::null())
		.spawn()
		.map_err(|e| tracing::error!("Failed to start gamescope: {e}"))
}
