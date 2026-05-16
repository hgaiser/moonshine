use std::process::Command;
use std::process::{Child, Stdio};

use async_shutdown::ShutdownManager;
use manager::{AppLaunchError, SessionShutdownReason};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
	config::{ApplicationConfig, Config},
	session::stream::{AudioStream, ControlStream, VideoStream},
};

use self::compositor::frame::HdrModeState;
use self::stream::VideoDynamicRange;
use self::stream::{AudioStreamContext, VideoStreamContext};
pub use manager::SessionManager;

pub mod compositor;
pub mod manager;
pub mod stream;

/// Grace period to wait after spawning the app before declaring it alive.
const APP_GRACE_PERIOD_SECS: u64 = 2;
/// Poll interval while waiting for early app exit.
const APP_GRACE_POLL_MS: u64 = 100;
/// Timeout for the HTTP /launch handler waiting for app launch result (seconds).
/// Covers up to 5 s XWayland wait + 2 s grace period + some margin.
pub(crate) const APP_LAUNCH_HTTP_TIMEOUT_SECS: u64 = 10;

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
	pub refresh_rate: u32,

	/// Encryption keys for encoding traffic.
	pub keys: SessionKeys,

	/// Audio channel count (2, 6, or 8).
	pub audio_channels: u8,

	/// Audio channel mask (Windows SPEAKER_ bitmask).
	pub audio_channel_mask: u32,

	/// HDR mode requested by the client at launch time.
	pub hdr: bool,
}

enum SessionCommand {
	/// Spawn the app-launcher thread and wait for compositor + app to be ready.
	/// The result is sent back via the enclosed oneshot sender.
	Launch(
		oneshot::Sender<Result<(), AppLaunchError>>,
		ShutdownManager<SessionShutdownReason>,
	),
	Start(VideoStreamContext, AudioStreamContext),
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
		stop_session_signal: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		// Create the socket directory for the PulseAudio server.
		let runtime_dir =
			std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
		let pulse_dir = std::path::Path::new(&runtime_dir).join("moonshine/pulse");
		std::fs::create_dir_all(&pulse_dir)
			.map_err(|e| tracing::error!("Failed to create pulse socket directory: {e}"))?;
		let socket_path = pulse_dir.join("native");

		// Remove any stale socket file from a previous session.
		let _ = std::fs::remove_file(&socket_path);

		// Bind the PulseAudio socket before launching the application so that
		// the app can connect as soon as it starts.
		let listener = std::os::unix::net::UnixListener::bind(&socket_path)
			.map_err(|e| tracing::error!("Failed to bind PulseAudio socket: {e}"))?;

		// Compositor and application launch are deferred to SessionCommand::Launch
		// (triggered at HTTP /launch time) so that errors can be surfaced to the
		// client before the RTSP handshake begins.

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = SessionInner {
			config,
			video_stream: None,
			audio_stream: None,
			control_stream: None,
			listener: Some(listener),
			pulse_dir,
			// Compositor handles are stored here after Launch and consumed by Start.
			compositor_frame_rx: None,
			compositor_input_tx: None,
			// HDR mode flag agreed during Start, used to configure streaming.
			compositor_hdr: false,
		};
		tokio::spawn(inner.run(command_rx, context.clone(), stop_session_signal));
		Ok(Self {
			command_tx,
			context,
			running: false,
		})
	}

	/// Spawn the app-launcher thread (compositor + XWayland + app launch + grace period).
	/// Sends the result back via `result_tx`.  Called at HTTP `/launch` time.
	pub async fn launch(
		&mut self,
		result_tx: oneshot::Sender<Result<(), AppLaunchError>>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		if let Err(e) = self
			.command_tx
			.send(SessionCommand::Launch(result_tx, stop_session_manager))
			.await
		{
			tracing::warn!("Failed to send Launch command to session: {e}");
			// Recover the dropped sender and send an explicit error so the
			// caller gets a proper `AppLaunchError` instead of a `RecvError`.
			let SessionCommand::Launch(recovered_tx, _) = e.0 else {
				return;
			};
			let _ = recovered_tx.send(Err(AppLaunchError::SpawnFailed));
		}
	}

	pub async fn start(
		&mut self,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
	) -> Result<(), ()> {
		tracing::info!("Starting session.");
		self.running = true;
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

/// Type aliases for compositor handle channels (mirrors what `compositor::start_compositor` returns).
type CompositorFrameRx = std::sync::mpsc::Receiver<compositor::frame::ExportedFrame>;
type CompositorInputTx = calloop::channel::Sender<compositor::input::CompositorInputEvent>;

struct SessionInner {
	config: Config,
	video_stream: Option<VideoStream>,
	audio_stream: Option<AudioStream>,
	control_stream: Option<ControlStream>,
	listener: Option<std::os::unix::net::UnixListener>,
	pulse_dir: std::path::PathBuf,
	/// Frame receiver from the compositor, populated after Launch, consumed by Start.
	compositor_frame_rx: Option<CompositorFrameRx>,
	/// Input sender to the compositor, populated after Launch, consumed by Start.
	compositor_input_tx: Option<CompositorInputTx>,
	/// HDR mode flag agreed during Start, used to configure streaming.
	compositor_hdr: bool,
}

impl SessionInner {
	async fn run(
		mut self,
		mut command_rx: mpsc::Receiver<SessionCommand>,
		mut session_context: SessionContext,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		// Holds a shutdown trigger: when this token is dropped (at the end of
		// `run()`), it fires `SessionShutdownReason::SessionStopped` on the
		// manager's shutdown manager, signalling that the session has ended.
		let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::SessionStopped);
		// Prevents the shutdown manager from completing until this token is
		// dropped, giving the rest of `run()` time to finish cleanup before
		// the manager considers the session fully stopped.
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		while let Ok(Some(command)) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
			match command {
				SessionCommand::Launch(result_tx, app_shutdown_manager) => {
					// HDR mode is known at /launch time from the client's query string.
					let compositor_config = compositor::CompositorConfig {
						width: session_context.resolution.0,
						height: session_context.resolution.1,
						refresh_rate: session_context.refresh_rate,
						gpu: self.config.gpu.clone(),
						hdr: session_context.hdr,
					};

					// Channel to pass compositor handles back from the thread to SessionInner.
					let (handles_tx, handles_rx) = oneshot::channel::<(CompositorFrameRx, CompositorInputTx)>();

					let app_context = session_context.clone();
					let app_pulse_dir = self.pulse_dir.clone();
					let keyboard_config = self.config.keyboard.clone();
					// Clone before the closure moves `app_shutdown_manager`, so we retain
					// a handle to trigger shutdown if spawning the thread itself fails,
					// or if waiting for handles times out.
					let spawn_failure_shutdown = app_shutdown_manager.clone();
					let timeout_shutdown = app_shutdown_manager.clone();

					// Spawn the blocking app-launcher thread. It starts the compositor,
					// waits for XWayland, launches the application, polls the grace period,
					// and then sends the result via result_tx. On any failure inside the
					// closure, `result_tx` is sent `Err(...)` and `handles_tx` is dropped
					// (causing `handles_rx` to resolve immediately with an error).
					if let Err(e) = std::thread::Builder::new()
						.name("app-launcher".to_string())
						.spawn(move || {
							let result = (|| -> Result<(CompositorFrameRx, CompositorInputTx, Child, std::path::PathBuf), AppLaunchError> {
								run_hook("pre_command", &app_context.application.pre_command);

						let (frame_rx, input_tx, ready_rx) =
								compositor::start_compositor(keyboard_config, compositor_config, app_shutdown_manager.clone())
									.map_err(|e| {
											tracing::error!("Failed to start compositor: {e}");
											AppLaunchError::CompositorFailed
										})?;

								let ready = ready_rx
									.recv_timeout(std::time::Duration::from_secs(5))
									.map_err(|e| {
										tracing::warn!("Timed out waiting for XWayland display: {e}");
										AppLaunchError::XWaylandTimeout
									})?;

						let (mut child, log_path) =
								launch_application(&app_context, &app_pulse_dir, &ready, ready.hdr)
									.map_err(|()| AppLaunchError::SpawnFailed)?;

								// Poll for early exit during the grace period.
								let grace_duration = std::time::Duration::from_secs(APP_GRACE_PERIOD_SECS);
								let poll_interval = std::time::Duration::from_millis(APP_GRACE_POLL_MS);
								let deadline = std::time::Instant::now() + grace_duration;
								loop {
									match child.try_wait() {
										Ok(Some(_)) => {
											// App exited within the grace period.
											tracing::info!(
												"Application exited unexpectedly. stdout/stderr saved to: {}",
												log_path.display()
											);
											let _ = app_shutdown_manager
												.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
									return Err(AppLaunchError::ExitedEarly);
										},
										Ok(None) => {},
										Err(e) => {
											tracing::warn!("Failed to poll application status: {e}");
											break;
										},
									}
									if std::time::Instant::now() >= deadline {
										break;
									}
									std::thread::sleep(poll_interval);
								}
								// One final check after the deadline to catch exits that
								// occurred in the last poll interval before the break.
								if let Ok(Some(_)) = child.try_wait() {
									tracing::info!(
										"Application exited unexpectedly. stdout/stderr saved to: {}",
										log_path.display()
									);
									let _ = app_shutdown_manager
										.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
							return Err(AppLaunchError::ExitedEarly);
							}

							Ok((frame_rx, input_tx, child, log_path))
							})();

							match result {
								Ok((frame_rx, input_tx, mut child, _log_path)) => {
									// Send compositor handles back to SessionInner so Start can use them.
									let _ = handles_tx.send((frame_rx, input_tx));
									// Notify the HTTP handler that launch succeeded.
									let _ = result_tx.send(Ok(()));

									// Wait for the application to exit.
									if let Err(e) = child.wait() {
										tracing::error!("Failed to wait for application: {e}");
									}
									tracing::info!("Application exited.");

									// Stop the session when the application exits.
									let _ = app_shutdown_manager
										.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
								},
								Err(err) => {
									// handles_tx is dropped here — handles_rx will return an error.
									// Trigger shutdown so the session manager clears active_session
									// and allows a clean retry. ExitedEarly already triggered shutdown
									// above, but we trigger it here for all other error paths.
									let reason = match &err {
										AppLaunchError::CompositorFailed | AppLaunchError::XWaylandTimeout => {
											SessionShutdownReason::CompositorStopped
										},
										AppLaunchError::SpawnFailed | AppLaunchError::ExitedEarly => {
											SessionShutdownReason::ApplicationStopped
										},
									};
									let _ = app_shutdown_manager.trigger_shutdown(reason);
									let _ = result_tx.send(Err(err));
								},
							}
						}) {
						tracing::error!("Failed to spawn app launcher thread: {e}");
						// `result_tx` and `handles_tx` were moved into the closure before
						// the spawn failed, so both are dropped here. The HTTP handler's
						// `launch_session()` receiver will get a `RecvError` and map it to
						// `AppLaunchError::SpawnFailed`. Trigger session shutdown so the
						// manager clears the active session and allows a clean retry.
						let _ = spawn_failure_shutdown.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
						continue;
					}

					// Await the compositor handles (or failure) with a bounded timeout,
					// also cancellable via the session stop signal so we never block the
					// command loop indefinitely when the session is torn down during launch.
					let handles_result = tokio::select! {
						result = tokio::time::timeout(
							std::time::Duration::from_secs(APP_LAUNCH_HTTP_TIMEOUT_SECS),
							handles_rx,
						) => Some(result),
						_ = stop_session_manager.wait_shutdown_triggered() => None,
					};
					match handles_result {
						Some(Ok(Ok((frame_rx, input_tx)))) => {
							self.compositor_frame_rx = Some(frame_rx);
							self.compositor_input_tx = Some(input_tx);
							self.compositor_hdr = session_context.hdr;
						},
						Some(Ok(Err(_))) => {
							// Thread sent an error via result_tx; handles were not produced.
							tracing::debug!("App-launcher thread reported launch failure; no compositor handles.");
						},
						Some(Err(_)) => {
							tracing::error!("Timed out waiting for compositor handles from app-launcher thread.");
							// The app-launcher thread may still be running. Trigger shutdown
							// to stop it and clear active_session so a retry is possible.
							let _ = timeout_shutdown.trigger_shutdown(SessionShutdownReason::CompositorStopped);
						},
						None => {
							// Session shutdown was triggered while waiting for the app-launcher.
							tracing::debug!(
								"Session shutdown triggered while awaiting compositor handles; aborting launch."
							);
						},
					}
				},

				SessionCommand::Start(video_stream_context, audio_stream_context) => {
					// Retrieve compositor handles that were stored during Launch.
					let frame_rx = match self.compositor_frame_rx.take() {
						Some(rx) => rx,
						None => {
							tracing::error!("No compositor frame receiver available at Start; was Launch called?");
							continue;
						},
					};
					let input_tx = match self.compositor_input_tx.take() {
						Some(tx) => tx,
						None => {
							tracing::error!("No compositor input sender available at Start; was Launch called?");
							continue;
						},
					};

					// HDR mode from RTSP ANNOUNCE (which arrives before PLAY / Start).
					let hdr = video_stream_context.dynamic_range == VideoDynamicRange::Hdr;
					self.compositor_hdr = hdr;

					// HDR metadata watch channel.
					let (hdr_metadata_tx, hdr_metadata_rx) = watch::channel(HdrModeState {
						enabled: hdr,
						metadata: None,
					});

					let video_stream = match VideoStream::new(
						self.config.clone(),
						video_stream_context.clone(),
						Some(frame_rx),
						if video_stream_context.encrypt_video {
							Some(session_context.keys.remote_input_key.clone())
						} else {
							None
						},
						stop_session_manager.clone(),
						hdr_metadata_tx,
					)
					.await
					{
						Ok(video_stream) => video_stream,
						Err(()) => {
							tracing::error!("Failed to create video stream, stopping session.");
							let _ = stop_session_manager.trigger_shutdown(SessionShutdownReason::CompositorStopped);
							continue;
						},
					};
					let Some(listener) = self.listener.take() else {
						tracing::error!("No listener available for audio stream.");
						continue;
					};
					let audio_stream = match AudioStream::new(
						self.config.clone(),
						audio_stream_context,
						listener,
						stop_session_manager.clone(),
					)
					.await
					{
						Ok(audio_stream) => audio_stream,
						Err(()) => {
							tracing::error!("Failed to create audio stream, stopping session.");
							let _ = stop_session_manager.trigger_shutdown(SessionShutdownReason::CompositorStopped);
							continue;
						},
					};
					let control_stream = match ControlStream::new(
						self.config.clone(),
						video_stream.clone(),
						audio_stream.clone(),
						session_context.clone(),
						stop_session_manager.clone(),
						input_tx,
						hdr,
						hdr_metadata_rx,
					) {
						Ok(control_stream) => control_stream,
						Err(()) => {
							tracing::error!("Failed to create control stream, killing session.");
							let _ = stop_session_manager.trigger_shutdown(SessionShutdownReason::CompositorStopped);
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

		// Stop the systemd scope to kill the application and all of its
		// descendants. The scope was created with TimeoutStopSec=5, so
		// this blocks at most 5 seconds before systemd sends SIGKILL.
		let _ = Command::new("systemctl")
			.args(["--user", "stop", "moonshine-session.scope"])
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status();

		run_hook("post_command", &session_context.application.post_command);

		tracing::debug!("Session stopped.");
	}
}

/// Run configured hook commands, logging the result of each.
/// Commands are executed in order; skips silently if the list is empty.
fn run_hook(name: &str, commands: &[Vec<String>]) {
	for (i, command) in commands.iter().enumerate() {
		if command.is_empty() {
			continue;
		}

		let Some(program) = command.first() else {
			continue;
		};
		let args = &command[1..];

		tracing::info!("{name}[{i}]: running {:?}", command);
		match Command::new(program)
			.args(args)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
		{
			Ok(status) => {
				if !status.success() {
					tracing::warn!("{name}[{i}]: exited with {status}");
				}
			},
			Err(e) => tracing::warn!("{name}[{i}]: failed to run: {e}"),
		}
	}
}

/// Launch the application as a child process.
///
/// Returns `(child, log_path)` on success.
///
/// The child gets both `DISPLAY` for X11/XWayland clients and
/// `WAYLAND_DISPLAY` for Wayland-native clients, both pointing at the
/// session compositor rather than the host desktop session.
/// `MOONSHINE_WAYLAND_DISPLAY` tells the layer which Wayland socket to
/// connect to, and `ENABLE_MOONSHINE_WSI=1` activates the layer.
/// When HDR is active, `DXVK_HDR` and `MOONSHINE_HDR` are also set for
/// DXVK/VKD3D-Proton HDR detection.
fn launch_application(
	context: &SessionContext,
	pulse_dir: &std::path::Path,
	ready: &compositor::CompositorReady,
	hdr: bool,
) -> Result<(Child, std::path::PathBuf), ()> {
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

	// Stop any leftover scope from a previous session before starting a new one.
	let _ = Command::new("systemctl")
		.args(["--user", "stop", "moonshine-session.scope"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status();

	let mut cmd = Command::new("systemd-run");
	cmd.args([
		"--user",
		"--scope",
		"--collect",
		"--unit",
		"moonshine-session",
		"--property=TimeoutStopSec=5",
		"--",
	])
	.arg(program)
	.args(args)
	.env("PULSE_SERVER", format!("unix:{}", pulse_dir.join("native").display()))
	.env("PULSE_RUNTIME_PATH", pulse_dir)
	.env("DISPLAY", format!(":{}", ready.xdisplay))
	.env("WAYLAND_DISPLAY", &ready.wayland_display)
	// The moonshine_swapchain Vulkan layer connects to the compositor via
	// this socket to obtain the moonshine_swapchain_factory_v2 global.
	.env("MOONSHINE_WAYLAND_DISPLAY", &ready.wayland_display)
	// Activate the moonshine WSI Vulkan layer (implicit layer gated by
	// ENABLE_MOONSHINE_WSI=1 in the layer manifest).
	.env("ENABLE_MOONSHINE_WSI", "1")
	// Tell Mesa (and potentially other ICDs) not to block waiting for
	// wl_surface readiness.  We present to bare wl_surfaces without
	// xdg_toplevel roles for XWayland bypass.  Set here rather than in the
	// layer to avoid calling std::env::set_var after threads are spawned.
	.env("vk_xwayland_wait_ready", "false");
	if hdr {
		// DXVK's dxgi.dll gates HDR color space exposure on this env var.
		// Without it, both DX11 (DXVK) and DX12 (vkd3d-proton via DXVK dxgi)
		// games will not see HDR as available.
		cmd.env("DXVK_HDR", "1");
		// Signal HDR mode to the moonshine-wsi layer so it can advertise HDR
		// surface formats correctly (the factory global is always present for
		// SDR sessions too, so we need an explicit capability signal).
		cmd.env("MOONSHINE_HDR", "1");

		// Create the gamescope frame limiter file so that `frameLimiterAware`
		// Vulkan applications (DXVK ≥ 2.3, VKD3D-Proton ≥ 2.12) switch to
		// `vkWaitForPresentKHR`-based frame pacing.  Without this, those apps
		// would render uncapped even though we send `wp_presentation_feedback`
		// at the target refresh rate.
		//
		// Value 1 = force FIFO (expose only FIFO present mode to the app and
		// redirect `vkWaitForPresentKHR` to block until compositor ack).
		//
		// Use XDG_RUNTIME_DIR (mode 0700, user-private) to avoid the symlink
		// attack that is possible with a predictable path in world-writable /tmp.
		// If XDG_RUNTIME_DIR is not available, skip creating the file rather than
		// falling back to /tmp which would be world-writable.
		if let Some(limiter_dir) = std::env::var_os("XDG_RUNTIME_DIR")
			.map(std::path::PathBuf::from)
			.filter(|p| p.is_dir())
		{
			let limiter_path = limiter_dir.join("moonshine-gamescope-limiter");
			match std::fs::write(&limiter_path, 1u32.to_ne_bytes()) {
				Ok(()) => {
					cmd.env("MOONSHINE_LIMITER_FILE", &limiter_path);
					tracing::debug!("Created gamescope limiter file: {}", limiter_path.display());
				},
				Err(e) => tracing::warn!("Failed to create gamescope limiter file: {e}"),
			}
		} else {
			tracing::warn!("XDG_RUNTIME_DIR is not set or not a directory; skipping gamescope limiter file");
		}
	}

	let child = cmd
		.stdout(
			log_file
				.try_clone()
				.map_err(|e| tracing::warn!("Failed to clone log file handle: {e}"))?,
		)
		.stderr(log_file)
		.stdin(Stdio::null())
		.spawn()
		.map_err(|e| tracing::warn!("Failed to launch application: {e}"))?;

	Ok((child, log_path))
}
