use async_shutdown::TriggerShutdownToken;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;

use super::{Session, stream::{AudioStreamContext, VideoStreamContext}, SessionContext, SessionKeys};

pub enum SessionManagerCommand {
	SetStreamContext(VideoStreamContext, AudioStreamContext),
	GetSessionContext(oneshot::Sender<Option<SessionContext>>),
	InitializeSession(SessionContext),
	StartSession,
	StopSession,
	UpdateKeys(SessionKeys),
}

#[derive(Clone)]
pub struct SessionManager {
	command_tx: mpsc::Sender<SessionManagerCommand>,
}

#[derive(Default)]
struct SessionManagerInner { }

impl SessionManager {
	#[allow(clippy::result_unit_err)]
	pub fn new(config: Config, shutdown_token: TriggerShutdownToken<i32>) -> Result<Self, ()> {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner: SessionManagerInner = Default::default();
		tokio::spawn(async move { inner.run(config, command_rx).await; drop(shutdown_token); });
		Ok(Self { command_tx })
	}

	pub async fn set_stream_context(
		&self,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext
	) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::SetStreamContext(video_stream_context, audio_stream_context)).await
			.map_err(|e| tracing::error!("Failed to send SetStreamContext command: {e}"))
	}

	pub async fn get_session_context(&self) -> Result<Option<SessionContext>, ()> {
		let (session_context_tx, session_context_rx) = oneshot::channel();
		self.command_tx.send(SessionManagerCommand::GetSessionContext(session_context_tx))
			.await
			.map_err(|e| tracing::error!("Failed to get session context: {e}"))?;
		session_context_rx.await
			.map_err(|e| tracing::error!("Failed to wait for GetCurrentSession response: {e}"))
	}

	pub async fn initialize_session(&self, context: SessionContext) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::InitializeSession(context))
			.await
			.map_err(|e| tracing::error!("Failed to initialize session: {e}"))?;
		Ok(())
	}

	pub async fn start_session(&self) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::StartSession)
			.await
			.map_err(|e| tracing::error!("Failed to start session: {e}"))
	}

	pub async fn stop_session(&self) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::StopSession)
			.await
			.map_err(|e| tracing::error!("Failed to stop session: {e}"))
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(SessionManagerCommand::UpdateKeys(keys))
			.await
			.map_err(|e| tracing::error!("Failed to stop session: {e}"))
	}
}

impl SessionManagerInner {
	async fn run(
		self,
		config: Config,
		mut command_rx: mpsc::Receiver<SessionManagerCommand>,
	) {
		// The active session, or None if there is no active session.
		let mut active_session: Option<Session> = None;

		// The context within which the next video stream will be created.
		let mut video_stream_context = None;

		// The context within which the next audio stream will be created.
		let mut audio_stream_context = None;

		tracing::debug!("Session manager waiting for commands.");

		while let Some(command) = command_rx.recv().await {
			match command {
				SessionManagerCommand::SetStreamContext(video, audio) =>  {
					if active_session.is_none() {
						// Well we can, but it is not expected.
						tracing::warn!("Can't set stream context without an active session.");
						continue;
					}

					video_stream_context = Some(video);
					audio_stream_context = Some(audio);
				},

				SessionManagerCommand::GetSessionContext(session_context_tx) => {
					let context = active_session.as_ref().map(|s| Some(s.context().clone())).unwrap_or(None);
					if session_context_tx.send(context).is_err() {
						tracing::error!("Failed to send current session context.");
					}
				},

				SessionManagerCommand::InitializeSession(session_context) => {
					if active_session.is_some() {
						tracing::warn!("Can't initialize a session, there is already an active session.");
						continue;
					}

					active_session = match Session::new(config.clone(), session_context) {
						Ok(session) => Some(session),
						Err(()) => continue,
					};
				},

				SessionManagerCommand::StartSession => {
					tracing::info!("Starting session.");

					let Some(session) = &mut active_session else {
						tracing::warn!("Can't launch a session, there is no session created yet.");
						continue;
					};

					if session.is_running() {
						tracing::info!("Can't start session, it is already running.");
						continue;
					}

					let Some(video_stream_context) = video_stream_context.clone() else {
						tracing::warn!("Can't start a stream without a video stream context.");
						continue;
					};
					let Some(audio_stream_context) = audio_stream_context.clone() else {
						tracing::warn!("Can't start a stream without a audio stream context.");
						continue;
					};

					let _ = session.start(video_stream_context, audio_stream_context).await;
				},

				SessionManagerCommand::StopSession => {
					if let Some(session) = &mut active_session {
						let _ = session.stop().await;
						active_session = None;
					} else {
						tracing::debug!("Trying to stop session, but no session is currently active.");
					}
				},

				SessionManagerCommand::UpdateKeys(keys) => {
					let Some(session) = &mut active_session else {
						tracing::warn!("Can't update session keys, there is no session created yet.");
						continue;
					};

					let _ = session.update_keys(keys).await;
				},
			};
		}

		// Stop a session if there is one active.
		if let Some(session) = &mut active_session {
			let _ = session.stop().await;
		}

		tracing::debug!("Session manager stopped.");
	}
}
