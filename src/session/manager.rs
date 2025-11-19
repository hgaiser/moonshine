use async_shutdown::{ShutdownManager, TriggerShutdownToken};
use tokio::sync::{mpsc, oneshot};

use crate::{config::Config, state::State};

use super::{Session, stream::{AudioStreamContext, VideoStreamContext}, SessionContext, SessionKeys};

#[derive(Clone, Debug)]
pub enum SessionShutdownReason {
	/// Session manager is shutting down.
	ManagerShutdown,
	/// The session was stopped by the user.
	UserStopped,
	/// Session stopped unexpectedly.
	SessionStopped,
	/// Video stream stopped unexpectedly.
	VideoStreamStopped,
	/// Video packet handler stopped unexpectedly.
	VideoPacketHandlerStopped,
	/// Video frame capture stopped unexpectedly.
	VideoFrameCaptureStopped,
	/// Video encoder stopped unexpectedly.
	VideoEncoderStopped,
	/// Audio stream stopped unexpectedly.
	AudioStreamStopped,
	/// Audio packet handler stopped unexpectedly.
	AudioPacketHandlerStopped,
	/// Audio capture stopped unexpectedly.
	AudioCaptureStopped,
	/// Audio encoder stopped unexpectedly.
	AudioEncoderStopped,
	/// Control stream stopped unexpectedly.
	ControlStreamStopped,
	/// Input handler stopped unexpectedly.
	InputHandlerStopped,
}

pub enum SessionManagerCommand {
	SetStreamContext(VideoStreamContext, AudioStreamContext),
	GetSessionContext(oneshot::Sender<Option<SessionContext>>),
	InitializeSession(SessionContext),
	StartSession,
	StopSession(oneshot::Sender<()>),
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
	pub fn new(config: Config, state: State, shutdown_token: TriggerShutdownToken<i32>) -> Result<Self, ()> {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner: SessionManagerInner = Default::default();
		tokio::spawn(async move { inner.run(config, state, command_rx).await; drop(shutdown_token); });
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
		tracing::info!("Requesting session to be stopped.");
		let (result_tx, result_rx) = oneshot::channel();
		self.command_tx.send(SessionManagerCommand::StopSession(result_tx))
			.await
			.map_err(|e| tracing::error!("Failed to stop session: {e}"))?;
		result_rx.await
			.map_err(|e| tracing::error!("Failed to wait for session to stop: {e}"))?;
		Ok(())
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
		state: State,
		mut command_rx: mpsc::Receiver<SessionManagerCommand>,
	) {
		// The active session, or None if there is no active session.
		let mut active_session: Option<Session> = None;

		// The context within which the next video stream will be created.
		let mut video_stream_context = None;

		// The context within which the next audio stream will be created.
		let mut audio_stream_context = None;

		tracing::debug!("Session manager waiting for commands.");

		let mut stop_session_manager = ShutdownManager::new();
		while let Some(command) = command_rx.recv().await {
			if active_session.is_some() && stop_session_manager.is_shutdown_triggered() {
				let reason = stop_session_manager.wait_shutdown_complete().await;
				tracing::warn!("Session stopped unexpectedly, waiting for new session (reason: {reason:?}).");
				active_session = None;
				stop_session_manager = ShutdownManager::new();
			}

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

					active_session = match Session::new(config.clone(), state.clone(), session_context, stop_session_manager.clone()) {
						Ok(session) => Some(session),
						Err(()) => continue,
					};
				},

				SessionManagerCommand::StartSession => {
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

				SessionManagerCommand::StopSession(result_tx) => {
					if active_session.is_some() {
						let _ = stop_session_manager.trigger_shutdown(SessionShutdownReason::UserStopped);

						// What to do in case of a timeout here?
						if tokio::time::timeout(
							std::time::Duration::from_secs(10),
							stop_session_manager.wait_shutdown_complete()
						).await.is_err() {
							let _ = result_tx.send(());
							tracing::error!("Timeout while waiting for session to stop.");
							break;
						}

						stop_session_manager = ShutdownManager::new();
						active_session = None;
					} else {
						tracing::warn!("Trying to stop session, but no session is currently active.");
					}
					let _ = result_tx.send(());
					tracing::info!("Session stopped, waiting for new session.");
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
		if active_session.is_some() {
			let _ = stop_session_manager.trigger_shutdown(SessionShutdownReason::ManagerShutdown);
			stop_session_manager.wait_shutdown_complete().await;
		}

		tracing::debug!("Session manager stopped.");
	}
}
