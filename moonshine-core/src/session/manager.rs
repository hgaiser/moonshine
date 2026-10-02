use std::net::IpAddr;
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use tokio::sync::{Mutex, broadcast, watch};

use crate::ShutdownReason;
use crate::session::FrameStats;
use crate::session::InitializedSession;
use crate::session::ResumeRequest;
use crate::session::SessionContext;
use crate::session::SessionKeyData;
use crate::session::SessionKeys;
use crate::session::SessionKeysSender;
use crate::session::SessionState;
use crate::session::authorization::StreamAuthorization;
use crate::session::compositor::CompositorConfig;
use crate::session::keys::KeyLedger;
use crate::session::stream::audio::AudioStreamConfig;
use crate::session::stream::audio::AudioStreamContext;
use crate::session::stream::control::ControlStreamConfig;
use crate::session::stream::video::VideoStreamConfig;
use crate::session::stream::video::VideoStreamContext;

const SESSION_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, PartialEq, Eq)]
enum ReconnectDecision {
	FastResume,
	Reconfigure {
		video_changed_fields: Vec<&'static str>,
		audio_changed: bool,
	},
	RejectShuttingDown,
}

fn reconnect_decision(
	active_video: &VideoStreamContext,
	requested_video: &VideoStreamContext,
	active_audio: &AudioStreamContext,
	requested_audio: &AudioStreamContext,
	shutting_down: bool,
) -> ReconnectDecision {
	if shutting_down {
		return ReconnectDecision::RejectShuttingDown;
	}
	let video_changed_fields = active_video.changed_fields(requested_video);
	let audio_changed = active_audio != requested_audio;
	if video_changed_fields.is_empty() && !audio_changed {
		ReconnectDecision::FastResume
	} else {
		ReconnectDecision::Reconfigure {
			video_changed_fields,
			audio_changed,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionShutdownReason {
	/// Session manager is shutting down.
	ManagerShutdown,
	/// The session was stopped by the user.
	UserStopped,
	/// The launched application exited.
	ApplicationStopped,
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
	/// Configuration for the compositor.
	compositor_config: CompositorConfig,

	/// Configuration for the video stream.
	video_config: VideoStreamConfig,

	/// Configuration for the audio stream.
	audio_config: AudioStreamConfig,

	/// Configuration for the control stream.
	control_config: ControlStreamConfig,

	/// Address to bind streams to.
	address: String,

	/// Time in seconds since last ping after which the stream closes.
	stream_timeout: u64,

	/// Whether to inhibit system sleep while a session is active.
	inhibit_sleep: bool,

	/// The currently active session, if any.
	session: Option<SessionState>,

	/// Shutdown manager for the active session, used to trigger session shutdown upon request.
	stop: ShutdownManager<SessionShutdownReason>,

	/// Sender for session keys, used to update keys from the webserver in different subsystems.
	///
	/// Subsystems that get updated are: video encoder, audio encoder and input handler.
	keys_tx: Option<SessionKeysSender>,

	/// Key-scoped nonce owner. Never reset: a key that is reused after a
	/// resume, reconfigure or new launch continues its nonce sequences.
	key_ledger: KeyLedger,

	/// Pending stream contexts received via RTSP ANNOUNCE. For an active
	/// session these belong to the reconnecting client and are not the contexts
	/// currently used by the live encoders until PLAY commits the new epoch.
	pending_video_stream_context: Option<VideoStreamContext>,
	pending_audio_stream_context: Option<AudioStreamContext>,
	/// Generation that produced the pending stream contexts. PLAY commits them
	/// only if no later launch/resume replaced that generation.
	pending_generation: Option<u64>,
	/// Authenticated session-level settings from the most recent `/resume`.
	resume_request: Option<ResumeRequest>,

	/// Authorization of the current launch/resume generation. RTSP, control and
	/// media endpoint discovery accept only this client and generation.
	authorization_tx: Option<watch::Sender<StreamAuthorization>>,

	/// Next authorization generation. Never reset, so a stale grant from an
	/// earlier session cannot match a later one.
	next_generation: u64,

	/// Broadcast sender for per-frame encoding statistics.
	stats_tx: tokio::sync::broadcast::Sender<FrameStats>,

	/// Watchdog task for monitoring unexpected session shutdowns.
	stop_watcher: Option<tokio::task::JoinHandle<()>>,

	/// Notify to trigger the video pipeline start (used by bench / external callers).
	video_start_notify: Option<Arc<tokio::sync::Notify>>,

	/// Notify to trigger the audio pipeline start (used by bench / external callers).
	audio_start_notify: Option<Arc<tokio::sync::Notify>>,

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
	/// Start a new authorization generation for `client_ip`, replacing every
	/// identifier the previous generation handed out.
	fn rotate_authorization(&mut self, client_ip: IpAddr) -> Result<(), ()> {
		let authorization = StreamAuthorization::new(self.next_generation, client_ip)?;
		self.next_generation += 1;
		match &self.authorization_tx {
			Some(tx) => {
				tx.send_replace(authorization);
			},
			None => self.authorization_tx = Some(watch::channel(authorization).0),
		}
		Ok(())
	}

	/// Whether `grant` belongs to the current generation.
	fn is_current(&self, grant: &StreamAuthorization) -> bool {
		self.authorization_tx
			.as_ref()
			.is_some_and(|tx| tx.borrow().generation() == grant.generation())
	}

	fn reset_session(&mut self) {
		if let Some(handle) = self.stop_watcher.take() {
			handle.abort();
		}
		self.session = None;
		self.keys_tx = None;
		self.pending_video_stream_context = None;
		self.pending_audio_stream_context = None;
		self.pending_generation = None;
		self.resume_request = None;
		self.authorization_tx = None;
		self.video_start_notify = None;
		self.audio_start_notify = None;
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
	stats_tx: broadcast::Sender<FrameStats>,
}

impl SessionManager {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		compositor_config: CompositorConfig,
		video_config: VideoStreamConfig,
		audio_config: AudioStreamConfig,
		control_config: ControlStreamConfig,
		address: String,
		stream_timeout: u64,
		inhibit_sleep: bool,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		let trigger_token = shutdown.trigger_shutdown_token(ShutdownReason::SessionManagerShutdown);
		let delay_token = shutdown.delay_shutdown_token().map_err(|e| {
			tracing::error!("Failed to create delay shutdown token: {e:?}");
		})?;

		let inner = SessionManagerInner {
			compositor_config,
			video_config,
			audio_config,
			control_config,
			address,
			stream_timeout,
			inhibit_sleep,
			session: None,
			stop: ShutdownManager::new(),
			keys_tx: None,
			key_ledger: KeyLedger::default(),
			pending_video_stream_context: None,
			pending_audio_stream_context: None,
			pending_generation: None,
			resume_request: None,
			authorization_tx: None,
			next_generation: 1,
			stats_tx: tokio::sync::broadcast::channel(256).0,
			stop_watcher: None,
			video_start_notify: None,
			audio_start_notify: None,
			shutdown: shutdown.clone(),
			_trigger_token: trigger_token,
			_delay_token: delay_token,
		};

		let stats_tx = inner.stats_tx.clone();
		let inner = Arc::new(Mutex::new(inner));

		Ok(Self { inner, stats_tx })
	}

	/// Returns a receiver for per-frame encoding statistics.
	///
	/// Call **before** `initialize_session()` to receive stats from the start.
	/// Multiple receivers can be created — each receives a copy of every message.
	pub fn bench_stats_receiver(&self) -> tokio::sync::broadcast::Receiver<FrameStats> {
		self.stats_tx.subscribe()
	}

	/// Trigger the video and audio pipelines to start encoding.
	///
	/// In the normal flow, this is triggered by the control stream when the
	/// client sends `StartB`. Call this from external callers (e.g. bench binary)
	/// that have no Moonlight client. Must be called after `start_session()`.
	pub async fn trigger_streams_start(&self) {
		let inner = self.inner.lock().await;
		if let Some(notify) = inner.video_start_notify.as_ref() {
			// Call notify_one() twice instead of notify_waiters() because
			// Notify only wakes tasks already .awaiting; notify_waiters()
			// is a no-op if no task is waiting yet.  notify_one() stores
			// a permit so the next notified().await completes immediately.
			notify.notify_one();
			notify.notify_one();
		}
		if let Some(notify) = inner.audio_start_notify.as_ref() {
			notify.notify_one();
			notify.notify_one();
		}
	}

	/// Authorize an RTSP peer for the current launch/resume generation.
	///
	/// The returned grant must accompany the ANNOUNCE and PLAY it authorizes;
	/// they are rejected if a later launch/resume replaced its generation.
	pub async fn authorize_stream(&self, peer: IpAddr) -> Option<StreamAuthorization> {
		let guard = self.inner.lock().await;
		let authorization = guard.authorization_tx.as_ref()?.borrow().clone();
		authorization.admits_peer(peer).then_some(authorization)
	}

	/// Set the video and audio stream contexts after receiving RTSP ANNOUNCE.
	///
	/// `session_id_v1` records that the client announced Moonlight's
	/// `ML_FF_SESSION_ID_V1`, after which media and control discovery require
	/// the generation's session identifiers.
	pub async fn set_stream_context(
		&self,
		grant: &StreamAuthorization,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
		session_id_v1: bool,
	) -> Result<(), ()> {
		// RTSP validates first to return a precise error; this guard keeps any
		// other caller from pausing or replacing a working epoch with values
		// the encoders, timers or transport cannot use.
		if let Err(reason) = video_stream_context.validate().and_then(|()| {
			crate::session::negotiation::validate_audio_packet_duration(audio_stream_context.packet_duration_ms)
		}) {
			tracing::warn!(
				reason,
				"Rejecting invalid stream negotiation before changing the session"
			);
			return Err(());
		}
		let mut guard = self.inner.lock().await;
		if !guard.is_current(grant) {
			tracing::warn!(
				generation = grant.generation(),
				"Rejecting RTSP ANNOUNCE from a replaced launch/resume generation"
			);
			return Err(());
		}
		let resume_request = guard.resume_request.clone();
		let (pause_video, pause_audio) = match guard.session.as_ref() {
			Some(SessionState::Launched(_)) => {
				tracing::debug!("Stream contexts received via RTSP ANNOUNCE.");
				guard.pending_video_stream_context = Some(video_stream_context);
				guard.pending_audio_stream_context = Some(audio_stream_context);
				guard.pending_generation = Some(grant.generation());
				(None, None)
			},
			Some(SessionState::Initialized(_)) => {
				tracing::warn!("SetStreamContext rejected: session not yet launched (Initialized state)");
				return Err(());
			},
			Some(SessionState::Active(active)) => {
				let changed = active.video_context().changed_fields(&video_stream_context);
				let audio_changed = active.audio_context() != &audio_stream_context;
				tracing::info!(
					active_width = active.video_context().width,
					active_height = active.video_context().height,
					active_fps = active.video_context().fps,
					active_codec = %active.video_context().format.codec,
					active_chroma = %active.video_context().format.chroma,
					active_bit_depth = active.video_context().format.bit_depth.bits(),
					active_hdr = active.video_context().format.hdr,
					active_bitrate = active.video_context().bitrate,
					requested_width = video_stream_context.width,
					requested_height = video_stream_context.height,
					requested_fps = video_stream_context.fps,
					requested_codec = %video_stream_context.format.codec,
					requested_chroma = %video_stream_context.format.chroma,
					requested_bit_depth = video_stream_context.format.bit_depth.bits(),
					requested_hdr = video_stream_context.format.hdr,
					requested_bitrate = video_stream_context.bitrate,
					changed_fields = ?changed,
					audio_changed,
					"Reconnect negotiation received"
				);
				if let Some(request) = resume_request
					&& (request
						.resolution
						.is_some_and(|value| value != (video_stream_context.width, video_stream_context.height))
						|| request
							.refresh_rate
							.is_some_and(|value| value != video_stream_context.fps)
						|| request
							.hdr
							.is_some_and(|value| value != video_stream_context.format.hdr))
				{
					tracing::warn!(
						?request,
						"HTTP resume parameters differ from authoritative RTSP negotiation"
					);
				}
				// Every reconnect needs a barrier, including identical-mode resume.
				let pause_video = Some(active.video_handle());
				let pause_audio = audio_changed.then(|| active.audio_handle());
				guard.pending_video_stream_context = Some(video_stream_context);
				guard.pending_audio_stream_context = Some(audio_stream_context);
				guard.pending_generation = Some(grant.generation());
				(pause_video, pause_audio)
			},
			None => {
				tracing::warn!("SetStreamContext rejected: no active session");
				return Err(());
			},
		};
		if session_id_v1 && let Some(tx) = &guard.authorization_tx {
			tx.send_modify(StreamAuthorization::require_session_id);
		}
		drop(guard);
		if let Some(handle) = pause_video {
			handle.pause_for_reconfigure().await.map_err(|()| {
				tracing::warn!("Failed to pause the active video epoch for reconnect reconfiguration");
			})?;
		}
		if let Some(handle) = pause_audio {
			handle.pause_for_reconfigure().await.map_err(|()| {
				tracing::warn!("Failed to pause the active audio epoch for reconnect reconfiguration");
			})?;
		}
		Ok(())
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
		let (tx, rx) = watch::channel(guard.key_ledger.publish(session_keys));
		context.keys = SessionKeys::Rx(rx);
		let client_ip = context.client_ip;

		let compositor_config = guard.compositor_config.clone();
		let video_config = guard.video_config.clone();
		let audio_config = guard.audio_config.clone();
		let control_config = guard.control_config.clone();
		let address = guard.address.clone();
		let stop = guard.stop.clone();
		let stats_tx = guard.stats_tx.clone();
		let session = InitializedSession::new(
			compositor_config,
			video_config,
			audio_config,
			control_config,
			address,
			context,
			stop,
			stats_tx,
		)
		.await?;
		guard.rotate_authorization(client_ip)?;
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
	///
	/// `grant` must be the authorization under which the pending contexts were
	/// announced; PLAY from a replaced generation cannot commit them.
	pub async fn start_session(&self, grant: &StreamAuthorization) -> Result<(), ()> {
		// Active sessions take an explicit resume path. Temporarily taking the
		// state prevents a concurrent PLAY from racing the epoch transition.
		let resume = {
			let mut guard = self.inner.lock().await;
			if !guard.is_current(grant) || guard.pending_generation != Some(grant.generation()) {
				tracing::warn!(
					generation = grant.generation(),
					pending_generation = ?guard.pending_generation,
					"Rejecting RTSP PLAY without a pending ANNOUNCE from the current generation"
				);
				return Err(());
			}
			guard.pending_generation = None;
			if matches!(guard.session, Some(SessionState::Active(_))) {
				let video = guard.pending_video_stream_context.take();
				let audio = guard.pending_audio_stream_context.take();
				let active = match guard.session.take() {
					Some(SessionState::Active(active)) => active,
					_ => unreachable!(),
				};
				guard.resume_request = None;
				Some((active, video, audio, guard.stop.clone()))
			} else {
				None
			}
		};

		if let Some((mut active, video, audio, stop)) = resume {
			let video =
				video.ok_or_else(|| tracing::error!("Reconnect PLAY received without a pending video context"))?;
			let audio =
				audio.ok_or_else(|| tracing::error!("Reconnect PLAY received without a pending audio context"))?;
			let decision = reconnect_decision(
				active.video_context(),
				&video,
				active.audio_context(),
				&audio,
				stop.is_shutdown_triggered(),
			);

			let result = match decision {
				ReconnectDecision::RejectShuttingDown => {
					tracing::warn!("Session is shutting down; rejecting reconnect PLAY");
					Err(())
				},
				ReconnectDecision::FastResume => {
					let result = active.reset_video_stream().await;
					tracing::info!("Reconnect stream configuration unchanged; using fast resume path");
					result
				},
				ReconnectDecision::Reconfigure {
					video_changed_fields,
					audio_changed,
				} => {
					let video_result = if video_changed_fields.is_empty() {
						active.reset_video_stream().await
					} else {
						tracing::info!(changed_fields = ?video_changed_fields, "Recreating video pipeline for changed reconnect configuration");
						active.reconfigure_video(video).await
					};
					let audio_result = if audio_changed {
						tracing::info!("Recreating audio epoch for changed reconnect configuration");
						active.reconfigure_audio(audio).await
					} else {
						Ok(())
					};
					video_result.and(audio_result)
				},
			};

			let mut guard = self.inner.lock().await;
			if guard.session.is_none() && !stop.is_shutdown_triggered() && !guard.stop.is_shutdown_triggered() {
				guard.session = Some(SessionState::Active(active));
			}
			return result;
		}

		let (launched, video_stream_context, audio_stream_context, stop) = {
			let mut guard = self.inner.lock().await;
			let video_stream_context = guard.pending_video_stream_context.take();
			let audio_stream_context = guard.pending_audio_stream_context.take();
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
					tracing::warn!("Concurrent reconnect transition already in progress");
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
		let video_config = guard.video_config.clone();
		let stream_timeout = guard.stream_timeout;
		let Some(authorization_rx) = guard.authorization_tx.as_ref().map(watch::Sender::subscribe) else {
			tracing::error!("Session has no stream authorization");
			guard.reset_session();
			return Err(());
		};
		match launched
			.start(
				video_config,
				stream_timeout,
				video_stream_context,
				audio_stream_context,
				stop,
				guard.inhibit_sleep,
				authorization_rx,
			)
			.await
		{
			Ok((active, video_notify, audio_notify)) => {
				guard.session = Some(SessionState::Active(active));
				guard.video_start_notify = Some(video_notify);
				guard.audio_start_notify = Some(audio_notify);
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

	/// Update keys and retain authenticated session-level resume parameters.
	///
	/// A resume starts a new authorization generation for `client_ip`: the
	/// previous client's RTSP, control and media discovery stop being accepted.
	pub(crate) async fn resume_session(
		&self,
		keys: SessionKeyData,
		request: ResumeRequest,
		client_ip: IpAddr,
	) -> Result<(), ()> {
		let mut guard = self.inner.lock().await;

		if guard.stop.is_shutdown_triggered() {
			tracing::warn!("Session is shutting down; rejecting resume key update.");
			return Err(());
		}

		if !matches!(guard.session.as_ref(), Some(SessionState::Active(_))) {
			tracing::warn!("No active streaming session to update keys for.");
			return Err(());
		}

		if guard.keys_tx.is_none() {
			tracing::warn!("Active streaming session has no key sender; rejecting resume.");
			return Err(());
		}
		guard.rotate_authorization(client_ip)?;
		// Keys were validated at the HTTP boundary; the ledger keeps nonce
		// allocation continuous when the client resumes with the same key.
		let keys = guard.key_ledger.publish(keys);
		if let Some(keys_tx) = &guard.keys_tx {
			keys_tx.send_replace(keys);
		}
		// Contexts announced under the previous generation must not be committed.
		guard.pending_video_stream_context = None;
		guard.pending_audio_stream_context = None;
		guard.pending_generation = None;
		guard.resume_request = Some(request);

		Ok(())
	}
}

#[cfg(test)]
impl SessionManager {
	/// Manager with default stream configuration, for RTSP/ingress tests.
	pub(crate) fn for_test(shutdown: ShutdownManager<ShutdownReason>) -> Self {
		Self::new(
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
			"127.0.0.1".into(),
			30,
			false,
			shutdown,
		)
		.unwrap()
	}

	/// Start an authorization generation without launching a session, as an
	/// authenticated `/launch` would.
	pub(crate) async fn authorize_client_for_test(&self, client_ip: IpAddr) -> StreamAuthorization {
		let mut guard = self.inner.lock().await;
		guard.rotate_authorization(client_ip).unwrap();
		guard.authorization_tx.as_ref().unwrap().borrow().clone()
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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::audio::{AudioChannels, AudioConfig};
	use crate::session::stream::video::{BitDepth, ChromaFormat, ColorRange, NegotiatedVideoFormat, VideoCodec};

	fn video(codec: VideoCodec) -> VideoStreamContext {
		VideoStreamContext {
			pyrowave_dialect: None,
			width: 1920,
			height: 1080,
			fps: 60,
			packet_size: 1392,
			bitrate: 20_000_000,
			minimum_fec_packets: 2,
			qos: true,
			format: NegotiatedVideoFormat::sdr(codec, ChromaFormat::Yuv420, BitDepth::Eight, ColorRange::Limited),
			max_reference_frames: 1,
			encrypt_video: false,
		}
	}

	fn audio() -> AudioStreamContext {
		AudioStreamContext {
			packet_duration_ms: 5,
			qos: true,
			audio_config: AudioConfig::from_channels(AudioChannels::Stereo, 0x3, true),
			encrypt_audio: false,
		}
	}

	fn assert_video_reconfigure(active: &VideoStreamContext, requested: &VideoStreamContext) {
		assert!(matches!(
			reconnect_decision(active, requested, &audio(), &audio(), false),
			ReconnectDecision::Reconfigure {
				audio_changed: false,
				..
			}
		));
	}

	#[tokio::test]
	async fn stream_grants_are_bound_to_client_and_generation() {
		let shutdown = ShutdownManager::new();
		let manager = SessionManager::for_test(shutdown);
		let client: IpAddr = "192.168.1.20".parse().unwrap();
		assert!(manager.authorize_stream(client).await.is_none(), "no launch yet");

		let first = manager.authorize_client_for_test(client).await;
		let mapped: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
		assert_eq!(manager.authorize_stream(mapped).await, Some(first.clone()));
		assert!(
			manager
				.authorize_stream("192.168.1.21".parse().unwrap())
				.await
				.is_none()
		);

		// A resume (new generation) invalidates grants handed out earlier, even
		// for the same client, and its PLAY cannot commit contexts it did not announce.
		let second = manager.authorize_client_for_test(client).await;
		assert!(second.generation() > first.generation());
		{
			let mut guard = manager.inner.lock().await;
			assert!(!guard.is_current(&first));
			assert!(guard.is_current(&second));
			guard.pending_generation = Some(first.generation());
		}
		assert!(manager.start_session(&first).await.is_err());
		assert!(manager.start_session(&second).await.is_err());
		assert_eq!(
			manager.inner.lock().await.pending_generation,
			Some(first.generation()),
			"rejected PLAY must not consume pending contexts"
		);
		assert!(
			manager
				.set_stream_context(&first, video(VideoCodec::H264), audio(), false)
				.await
				.is_err()
		);

		// Another client resuming takes the session over.
		let other: IpAddr = "192.168.1.30".parse().unwrap();
		let third = manager.authorize_client_for_test(other).await;
		assert!(manager.authorize_stream(client).await.is_none());
		assert_eq!(manager.authorize_stream(other).await, Some(third));
	}

	#[test]
	fn identical_reconnect_uses_fast_resume() {
		assert_eq!(
			reconnect_decision(
				&video(VideoCodec::H264),
				&video(VideoCodec::H264),
				&audio(),
				&audio(),
				false
			),
			ReconnectDecision::FastResume
		);
	}

	#[test]
	fn resolution_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.width = 3840;
		requested.height = 2160;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn fps_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.fps = 120;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn bitrate_only_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.bitrate *= 2;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn codec_transitions_reconfigure() {
		for (from, to) in [
			(VideoCodec::H264, VideoCodec::Hevc),
			(VideoCodec::Hevc, VideoCodec::Av1),
			(VideoCodec::Av1, VideoCodec::PyroWave),
			(VideoCodec::PyroWave, VideoCodec::H264),
		] {
			assert_video_reconfigure(&video(from), &video(to));
		}
	}

	#[test]
	fn dynamic_range_transitions_reconfigure() {
		let sdr = video(VideoCodec::Hevc);
		let mut hdr = sdr.clone();
		hdr.format = NegotiatedVideoFormat::hdr10(VideoCodec::Hevc, ChromaFormat::Yuv420, ColorRange::Limited);
		assert_video_reconfigure(&sdr, &hdr);
		assert_video_reconfigure(&hdr, &sdr);
	}

	#[test]
	fn chroma_and_bit_depth_changes_reconfigure() {
		let active = video(VideoCodec::Hevc);
		let mut chroma = active.clone();
		chroma.format.chroma = ChromaFormat::Yuv444;
		assert_video_reconfigure(&active, &chroma);
		let mut ten_bit = active.clone();
		ten_bit.format.bit_depth = BitDepth::Ten;
		assert_video_reconfigure(&active, &ten_bit);
	}

	#[test]
	fn packet_size_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.packet_size = 1200;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn packetizer_transport_reference_and_color_changes_reconfigure() {
		let active = video(VideoCodec::Hevc);
		for requested in [
			{
				let mut value = active.clone();
				value.minimum_fec_packets += 1;
				value
			},
			{
				let mut value = active.clone();
				value.qos = !value.qos;
				value
			},
			{
				let mut value = active.clone();
				value.max_reference_frames += 1;
				value
			},
			{
				let mut value = active.clone();
				value.format.range = ColorRange::Full;
				value
			},
		] {
			assert_video_reconfigure(&active, &requested);
		}
	}

	#[test]
	fn audio_context_change_reconfigures_audio() {
		let active_audio = audio();
		let mut requested_audio = active_audio.clone();
		requested_audio.audio_config = AudioConfig::from_channels(AudioChannels::Surround51, 0x3f, true);
		assert_eq!(
			reconnect_decision(
				&video(VideoCodec::H264),
				&video(VideoCodec::H264),
				&active_audio,
				&requested_audio,
				false,
			),
			ReconnectDecision::Reconfigure {
				video_changed_fields: Vec::new(),
				audio_changed: true,
			}
		);
	}

	#[test]
	fn encryption_mode_change_reconfigures_but_key_refresh_does_not() {
		let active = video(VideoCodec::H264);
		let mut encrypted = active.clone();
		encrypted.encrypt_video = true;
		assert_video_reconfigure(&active, &encrypted);
		// Session keys are refreshed through a watch channel and deliberately do
		// not participate in negotiated-context equality.
		assert_eq!(
			reconnect_decision(&active, &active, &audio(), &audio(), false),
			ReconnectDecision::FastResume
		);
	}

	#[test]
	fn shutdown_rejects_reconnect() {
		assert_eq!(
			reconnect_decision(
				&video(VideoCodec::H264),
				&video(VideoCodec::H264),
				&audio(),
				&audio(),
				true
			),
			ReconnectDecision::RejectShuttingDown
		);
	}
}
