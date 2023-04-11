use async_shutdown::Shutdown;
use tokio::sync::mpsc;

use crate::{session::rtsp, config::SessionConfig};

pub enum SessionManagerCommand {
	LaunchSession(SessionContext),
}

/// Launch a session for a client.
#[derive(Clone, Debug)]
pub struct SessionContext {
	/// Id of the application to launch.
	pub application_id: u32,

	/// Resolution of the video stream.
	pub resolution: (u32, u32),

	/// Refresh rate of the video stream.
	pub refresh_rate: u32,

	/// AES GCM key used for encoding control messages.
	pub remote_input_key: Vec<u8>,

	/// AES GCM initialization vector for control messages.
	pub remote_input_key_id: String,
}

pub struct SessionManager {
	command_rx: mpsc::Receiver<SessionManagerCommand>,
}

impl SessionManager {
	pub fn new(command_rx: mpsc::Receiver<SessionManagerCommand>) -> Self {
		Self { command_rx }
	}

	pub async fn run(mut self, rtsp_port: u16, config: SessionConfig, shutdown: Shutdown) -> Result<(), ()> {
		loop {
			let command = shutdown.wrap_cancel(self.command_rx.recv())
				.await
				.ok_or(())?;

			match command {
				Some(SessionManagerCommand::LaunchSession(session_context)) => {
					log::info!("Launching session with arguments: {session_context:?}");
					tokio::spawn(rtsp::run(
						"0.0.0.0".to_string(),
						rtsp_port,
						session_context,
						config.clone(),
					));
				},

				None => {
					break;
				},
			};
		}

		Ok(())
	}
}
