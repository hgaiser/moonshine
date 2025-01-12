use std::process::Stdio;

use async_shutdown::ShutdownManager;
use enet::Enet;
use tokio::sync::mpsc;

use crate::{config::{Config, ApplicationConfig}, session::stream::{VideoStream, AudioStream, ControlStream}};

use self::stream::{VideoStreamContext, AudioStreamContext};
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
}

enum SessionCommand {
	StartStream(VideoStreamContext, AudioStreamContext),
	StopStream,
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
		enet: Enet,
		stop_signal: ShutdownManager<()>,
	) -> Result<Self, ()> {
		if let Some(run_before) = &context.application.run_before {
			for command in run_before {
				run_command(command, &context);
			}
		}

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = SessionInner { config, video_stream: None, audio_stream: None, control_stream: None };
		tokio::spawn(inner.run(command_rx, context.clone(), enet, stop_signal));
		Ok(Self { command_tx, context, running: false })
	}

	pub async fn start_stream(
		&mut self,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
	) -> Result <(), ()> {
		self.running = true;
		self.command_tx.send(SessionCommand::StartStream(video_stream_context, audio_stream_context))
			.await
			.map_err(|e| tracing::error!("Failed to send StartStream command: {e}"))
	}

	pub async fn stop_stream(&mut self) -> Result<(), ()> {
		self.running = false;
		self.command_tx.send(SessionCommand::StopStream)
			.await
			.map_err(|e| tracing::error!("Failed to send StopStream command: {e}"))
	}

	pub fn get_context(&self) -> &SessionContext {
		&self.context
	}

	pub fn is_running(&self) -> bool {
		self.running
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(SessionCommand::UpdateKeys(keys)).await
			.map_err(|e| tracing::error!("Failed to send UpdateKeys command: {e}"))
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		if let Some(run_after) = &self.context.application.run_after {
			for command in run_after {
				run_command(command, &self.context);
			}
		}
	}
}

struct SessionInner {
	config: Config,
	video_stream: Option<VideoStream>,
	audio_stream: Option<AudioStream>,
	control_stream: Option<ControlStream>,
}

impl SessionInner {
	async fn run(
		mut self,
		mut command_rx: mpsc::Receiver<SessionCommand>,
		mut session_context: SessionContext,
		enet: Enet,
		stop_signal: ShutdownManager<()>,
	) {
		while let Some(command) = command_rx.recv().await {
			match command {
				SessionCommand::StartStream(video_stream_context, audio_stream_context) => {
					let video_stream = VideoStream::new(self.config.clone(), video_stream_context, stop_signal.clone());
					let audio_stream = AudioStream::new(self.config.clone(), audio_stream_context, stop_signal.clone());
					let control_stream = match ControlStream::new(
						self.config.clone(),
						video_stream.clone(),
						audio_stream.clone(),
						session_context.clone(),
						enet.clone(),
						stop_signal.clone()
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

				SessionCommand::StopStream => {
					let _ = stop_signal.trigger_shutdown(());
				},

				SessionCommand::UpdateKeys(keys) => {
					let Some(audio_stream) = &self.audio_stream else {
						tracing::warn!("Can't update session keys without an audio stream.");
						continue;
					};
					let Some(control_stream) = &self.control_stream else {
						tracing::warn!("Can't update session keys without an control stream.");
						continue;
					};

					session_context.keys = keys.clone();
					let _ = audio_stream.update_keys(keys.clone()).await;
					let _ = control_stream.update_keys(keys).await;
				},
			}
		}

		let _ = stop_signal.trigger_shutdown(());
		tracing::debug!("Command channel closed.");
	}
}

fn run_command(command: &[String], context: &SessionContext) {
	if command.is_empty() {
		tracing::warn!("Can't run an empty command.");
		return;
	}

	let command: Vec<String> = command.to_vec()
		.iter_mut()
		.map(|c| {
			let c = c
				.replace("{width}", &context.resolution.0.to_string())
				.replace("{height}", &context.resolution.1.to_string());
			shellexpand::full(&c).map(|c| c.into()).unwrap_or(c)
		})
		.collect();

	tracing::info!("Running command: {command:?}");

	// Now run the command.
	let _ = std::process::Command::new(&command[0])
		.args(&command[1..])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.stdin(Stdio::null())
		.spawn()
		.map_err(|e| tracing::error!("Failed to run command: {e}"));
}
