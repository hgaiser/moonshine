use std::sync::Arc;

use async_shutdown::ShutdownManager;
use tokio::sync::{watch, Mutex};

use crate::config::Config;
use crate::session::stream::AudioStreamContext;
use crate::session::stream::VideoStreamContext;
use crate::session::InitializedSession;
use crate::session::SessionContext;
use crate::session::SessionKeyData;
use crate::session::SessionKeys;
use crate::session::SessionKeysSender;
use crate::session::SessionState;
use crate::ShutdownReason;

const SESSION_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionShutdownReason {
	/// Session manager is shutting down.
	ManagerShutdown,
	/// The session was stopped by the user.
	UserStopped,
	/// Video packet handler stopped unexpectedly.
	VideoPacketHandlerStopped,
	/// Video encoder stopped unexpectedly.
	VideoEncoderStopped,
	/// Audio packet handler stopped unexpectedly.
	AudioPacketHandlerStopped,
	/// PulseAudio server stopped unexpectedly.
	PulseServerStopped,
	/// Audio encoder stopped unexpectedly.
	AudioEncoderStopped,
	/// Control stream stopped unexpectedly.
	ControlStreamStopped,
	/// Input handler stopped unexpectedly.
	InputHandlerStopped,
	/// Compositor stopped unexpectedly.
	CompositorStopped,
}

struct SessionManagerInner {
	/// Configuration for the session manager.
	config: Config,

	/// The currently active session, if any.
	session: Option<SessionState>,

	/// Shutdown manager for the active session, used to trigger session shutdown upon request.
	stop: ShutdownManager<SessionShutdownReason>,

	/// Sender for session keys, used to update keys from the webserver in different subsystems.
	///
	/// Subsystems that get updated are: video encoder, audio encoder and input handler.
	keys_tx: Option<SessionKeysSender>,

	/// Stream contexts received via RTSP ANNOUNCE, consumed by `start_session`.
	video_stream_context: Option<VideoStreamContext>,
	audio_stream_context: Option<AudioStreamContext>,

	/// Watchdog task for monitoring unexpected session shutdowns.
	stop_watcher: Option<tokio::task::JoinHandle<()>>,

	/// Shutdown manager for the entire application.
	shutdown: ShutdownManager<ShutdownReason>,

	/// Trigger token for the session manager's own shutdown trigger.
	///
	/// Used to trigger an application shutdown if the session manager stops unexpectedly.
	_trigger_token: async_shutdown::TriggerShutdownToken<ShutdownReason>,

	/// Delay token for the session manager's own shutdown trigger.
	///
	/// Used to delay shutdown until the session manager has cleaned up.
	_delay_token: async_shutdown::DelayShutdownToken<ShutdownReason>,
}

impl SessionManagerInner {
	fn reset_session(&mut self) {
		if let Some(handle) = self.stop_watcher.take() {
			handle.abort();
		}
		self.session = None;
		self.keys_tx = None;
		self.video_stream_context = None;
		self.audio_stream_context = None;
		self.stop = ShutdownManager::new();
	}
}

impl Drop for SessionManagerInner {
	fn drop(&mut self) {
		if let Some(handle) = self.stop_watcher.take() {
			handle.abort();
		}
		if self.session.is_some() {
			tracing::debug!("Stopping active session before shutdown.");
			let _ = self.stop.trigger_shutdown(SessionShutdownReason::ManagerShutdown);
			// Wait until shutdown completed.
			if let Ok(handle) = tokio::runtime::Handle::try_current() {
				handle.block_on(self.stop.wait_shutdown_complete());
			}
		}
	}
}

#[derive(Clone)]
pub struct SessionManager {
	inner: Arc<Mutex<SessionManagerInner>>,
}

impl SessionManager {
	pub fn new(config: Config, shutdown: ShutdownManager<ShutdownReason>) -> Result<Self, ()> {
		let trigger_token = shutdown.trigger_shutdown_token(ShutdownReason::SessionManagerShutdown);
		let delay_token = shutdown.delay_shutdown_token().map_err(|e| {
			tracing::error!("Failed to create delay shutdown token: {e:?}");
		})?;

		let inner = SessionManagerInner {
			config,
			session: None,
			stop: ShutdownManager::new(),
			keys_tx: None,
			video_stream_context: None,
			audio_stream_context: None,
			stop_watcher: None,
			shutdown: shutdown.clone(),
			_trigger_token: trigger_token,
			_delay_token: delay_token,
		};

		let inner = Arc::new(Mutex::new(inner));

		Ok(Self { inner })
	}

	/// Set the video and audio stream contexts after receiving RTSP ANNOUNCE.
	pub async fn set_stream_context(
		&self,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
	) -> Result<(), ()> {
		let mut guard = self.inner.lock().await;
		match guard.session.as_ref() {
			Some(SessionState::Launched(_)) => {
				tracing::debug!("Stream contexts received via RTSP ANNOUNCE.");
				guard.video_stream_context = Some(video_stream_context);
				guard.audio_stream_context = Some(audio_stream_context);
				Ok(())
			},
			Some(SessionState::Initialized(_)) => {
				tracing::warn!("SetStreamContext rejected: session not yet launched (Initialized state)");
				Err(())
			},
			Some(SessionState::Active(_)) => {
				tracing::warn!("SetStreamContext rejected: session already active (Active state)");
				Err(())
			},
			None => {
				tracing::warn!("SetStreamContext rejected: no active session");
				Err(())
			},
		}
	}

	/// Get the current session context if there is an active session; otherwise return `None`.
	pub async fn get_session_context(&self) -> Result<Option<SessionContext>, ()> {
		let guard = self.inner.lock().await;
		Ok(guard.session.as_ref().map(|s| s.context().clone()))
	}

	/// Initialize a new session with the provided context.
	///
	/// The session is not launched until `launch_session` is called.
	pub async fn initialize_session(&self, mut context: SessionContext) -> Result<(), ()> {
		let mut guard = self.inner.lock().await;

		if guard.session.is_some() || guard.keys_tx.is_some() {
			tracing::warn!("Session already initialized, rejecting InitializeSession command.");
			return Err(());
		}

		// Extract the raw keys from context and create the watch channel.
		let session_keys = match context.keys {
			SessionKeys::Keys(data) => data,
			SessionKeys::Rx(_) => {
				tracing::error!("Session keys already initialized as a watch receiver");
				return Err(());
			},
		};
		let (tx, rx) = watch::channel(session_keys);
		context.keys = SessionKeys::Rx(rx);

		let config = guard.config.clone();
		let stop = guard.stop.clone();
		let session = InitializedSession::new(config, context, stop).await?;
		guard.session = Some(SessionState::Initialized(session));

		spawn_session_watchdog(&self.inner, &mut guard);
		tracing::info!("Session initialized successfully, waiting to be launched.");

		// Set the keys sender here so that it is not set if session initialization failed.
		guard.keys_tx = Some(tx);
		Ok(())
	}

	/// Launch the session by starting the compositor and application, but don't start streams until RTSP ANNOUNCE is received.
	pub async fn launch_session(&self) -> Result<(), ()> {
		let session = {
			let mut guard = self.inner.lock().await;
			match guard.session.take() {
				Some(SessionState::Initialized(session)) => session,
				Some(SessionState::Launched(launched)) => {
					guard.session = Some(SessionState::Launched(launched));
					tracing::warn!("LaunchSession rejected: session already launched");
					return Err(());
				},
				Some(SessionState::Active(active)) => {
					guard.session = Some(SessionState::Active(active));
					tracing::warn!("LaunchSession rejected: session already active");
					return Err(());
				},
				None => {
					tracing::warn!("LaunchSession rejected: no active session");
					return Err(());
				},
			}
		};

		tracing::info!("Launching session (starting compositor and app).");
		match session.launch().await {
			Ok(launched) => {
				let mut guard = self.inner.lock().await;
				guard.session = Some(SessionState::Launched(launched));
				tracing::info!("Session launched successfully, waiting for RTSP ANNOUNCE.");
				Ok(())
			},
			Err(()) => {
				let mut guard = self.inner.lock().await;
				guard.reset_session();
				tracing::error!("Failed to launch session, waiting for new session.");
				Err(())
			},
		}
	}

	/// Start the video and audio streams.
	///
	/// Returns `Ok(())` only after all three streams (video, audio, control) are
	/// successfully constructed. Returns `Err(())` if any stream fails to initialize.
	pub async fn start_session(&self) -> Result<(), ()> {
		let (launched, video_stream_context, audio_stream_context, stop) = {
			let mut guard = self.inner.lock().await;
			let video_stream_context = guard.video_stream_context.take();
			let audio_stream_context = guard.audio_stream_context.take();
			match guard.session.take() {
				Some(SessionState::Launched(launched)) => {
					(launched, video_stream_context, audio_stream_context, guard.stop.clone())
				},
				Some(SessionState::Initialized(session)) => {
					guard.session = Some(SessionState::Initialized(session));
					tracing::warn!("StartSession rejected: session not yet launched");
					return Err(());
				},
				Some(SessionState::Active(active)) => {
					guard.session = Some(SessionState::Active(active));
					tracing::warn!("StartSession rejected: session already active");
					return Err(());
				},
				None => {
					tracing::warn!("StartSession rejected: no active session");
					return Err(());
				},
			}
		};

		let video_stream_context = video_stream_context.ok_or_else(|| {
			tracing::error!("VideoStreamContext not set");
		})?;
		let audio_stream_context = audio_stream_context.ok_or_else(|| {
			tracing::error!("AudioStreamContext not set");
		})?;

		tracing::info!("Starting session streams.");
		let mut guard = self.inner.lock().await;
		match launched.start(video_stream_context, audio_stream_context, stop) {
			Ok(active) => {
				guard.session = Some(SessionState::Active(active));
				tracing::info!("Session streams started successfully.");
				Ok(())
			},
			Err(()) => {
				guard.reset_session();
				tracing::error!("Failed to start session streams.");
				Err(())
			},
		}
	}

	/// Stop the session and return to Uninitialized state.
	pub async fn stop_session(&self) -> Result<(), ()> {
		let (stop, shutdown) = {
			let mut guard = self.inner.lock().await;
			match guard.session {
				Some(_) => {},
				None => return Ok(()),
			}
			let stop = guard.stop.clone();
			let shutdown = guard.shutdown.clone();

			// Drop session first, which drops the Application.
			guard.reset_session();
			(stop, shutdown)
		};

		// Then trigger shutdown of the compositor & streams.
		let _ = stop.trigger_shutdown(SessionShutdownReason::UserStopped);

		wait_for_session_shutdown(&stop, &shutdown, SESSION_SHUTDOWN_TIMEOUT_SECS).await?;
		tracing::info!("Session stopped by user, waiting for new session.");
		Ok(())
	}

	/// Update the session keys for the active session.
	pub async fn update_keys(&self, keys: SessionKeyData) -> Result<(), ()> {
		let guard = self.inner.lock().await;

		if guard.session.is_none() {
			tracing::warn!("No active session to update keys for.");
			return Err(());
		}

		if let Some(keys_tx) = &guard.keys_tx {
			keys_tx.send_replace(keys);
		} else {
			tracing::warn!("No active session to update keys for.");
		}

		Ok(())
	}
}

/// Spawn a watchdog task to monitor the session for unexpected shutdowns.
fn spawn_session_watchdog(inner: &Arc<Mutex<SessionManagerInner>>, guard: &mut SessionManagerInner) {
	if guard.stop_watcher.is_some() {
		tracing::error!("Session watchdog already running, not spawning another.");
		return;
	}

	let inner = inner.clone();
	let stop = guard.stop.clone();
	let shutdown = guard.shutdown.clone();
	let handle = tokio::spawn(async move {
		tokio::select! {
			reason = stop.wait_shutdown_triggered() => {
				if reason == SessionShutdownReason::UserStopped {
					tracing::info!("Session shutdown requested by user.");
				} else {
					tracing::warn!("Session stopped unexpectedly (reason: {reason:?}), waiting for new session.");
				}
			},
			_ = shutdown.wait_shutdown_triggered() => {
				tracing::debug!("Global shutdown triggered, stopping active session.");
				let _ = stop.trigger_shutdown(SessionShutdownReason::ManagerShutdown);
			},
		}

		// First drop the session so that the application exits as soon as possible.
		{
			inner.lock().await.reset_session();
		}

		// Then wait for the session to shut down.
		stop.wait_shutdown_complete().await;
	});
	guard.stop_watcher = Some(handle);
}

/// Wait for the session to shut down within the given timeout.
///
/// If the session does not shut down in time, triggers a global application
/// shutdown to prevent orphaned tasks and resource leaks.
async fn wait_for_session_shutdown(
	stop: &ShutdownManager<SessionShutdownReason>,
	shutdown: &ShutdownManager<ShutdownReason>,
	timeout_secs: u64,
) -> Result<(), ()> {
	match tokio::time::timeout(
		std::time::Duration::from_secs(timeout_secs),
		stop.wait_shutdown_complete(),
	)
	.await
	{
		Ok(_) => Ok(()),
		Err(_) => {
			tracing::error!("Session shutdown timed out after {timeout_secs}s — triggering application shutdown.");
			let _ = shutdown.trigger_shutdown(ShutdownReason::SessionManagerShutdown);
			Err(())
		},
	}
}
