use async_shutdown::{ShutdownManager, TriggerShutdownToken};
use enet::Enet;
use tokio::sync::{mpsc, oneshot};

use crate::{config::Config, session::rtsp::RtspServer};

mod rtsp;

pub enum SessionManagerCommand {
	LaunchSession(SessionContext),
	GetCurrentSession(oneshot::Sender<Option<RtspServer>>),
	StopSession,
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

#[derive(Clone)]
pub struct SessionManager {
	command_tx: mpsc::Sender<SessionManagerCommand>,
}

struct SessionManagerInner {
	session: Option<RtspServer>,
}

impl SessionManager {
	pub fn new(config: Config, shutdown_token: TriggerShutdownToken<i32>) -> Result<Self, ()> {
		// Preferably this gets constructed in control.rs, however it needs to stay
		// alive throughout the entire application runtime.
		// Once dropped, it cannot be initialized again.
		let enet = Enet::new()
			.map_err(|e| log::error!("Failed to initialize Enet session: {e}"))?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = SessionManagerInner { session: None };
		tokio::spawn(async move { inner.run(config, command_rx, enet).await; drop(shutdown_token); });
		Ok(Self { command_tx })
	}

	pub async fn launch(&self, context: SessionContext) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::LaunchSession(context))
			.await
			.map_err(|e| log::error!("Failed to launch session: {e}"))?;
		Ok(())
	}

	pub async fn get_current_session(&self) -> Result<Option<RtspServer>, ()> {
		let (session_tx, session_rx) = oneshot::channel();
		self.command_tx.send(SessionManagerCommand::GetCurrentSession(session_tx))
			.await
			.map_err(|e| log::error!("Failed to launch session: {e}"))?;
		session_rx.await
			.map_err(|e| log::error!("Failed to wait for get current session response: {e}"))
	}

	pub async fn stop_session(&self) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::StopSession)
			.await
			.map_err(|e| log::error!("Failed to launch session: {e}"))
	}
}

impl SessionManagerInner {
	async fn run(
		mut self,
		config: Config,
		mut command_rx: mpsc::Receiver<SessionManagerCommand>,
		enet: Enet,
	) {
		log::debug!("Waiting for commands.");

		while let Some(command) = command_rx.recv().await {
			match command {
				SessionManagerCommand::LaunchSession(session_context) => {
					if self.session.is_some() {
						log::warn!("Can't launch a session, there is already an active session running.");
						continue;
					}

					log::info!("Launching session with arguments: {session_context:?}");
					if let Ok(session) = RtspServer::new(config.clone(), session_context, enet.clone()) {
						self.session = Some(session);
					}
				},
				SessionManagerCommand::GetCurrentSession(session_tx) => {
					if session_tx.send(self.session.clone()).is_err() {
						log::error!("Failed to send current session");
					}
				}
				SessionManagerCommand::StopSession => {
					if let Some(session) = self.session {
						session.stop_stream();
						self.session = None;
					} else {
						log::debug!("Trying to cancel session, but no session is currently active.");
					}
				},
			};
		}

		log::debug!("Channel closed, stopped listening for commands.");
	}
}