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

		let gamescope_process = start_gamescope(&context, &sink_name)?;

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
			.map_err(|e| tracing::error!("Failed to send Start command: {e}"))
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
					let video_stream =
						match VideoStream::new(self.config.clone(), video_stream_context, stop_session_manager.clone())
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
					let control_stream = match ControlStream::new(
						self.config.clone(),
						video_stream.clone(),
						audio_stream.clone(),
						session_context.clone(),
						stop_session_manager.clone(),
						self.enet.clone(),
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

		if let Some(mut gamescope_process) = self.gamescope_process {
			let _ = gamescope_process.kill();
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
