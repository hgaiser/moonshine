use std::cell::RefCell;
use std::ops::Deref;
use std::process::Command;
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use enet::Enet;
use manager::SessionShutdownReason;
use pulse::context::{Context, FlagSet};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::Proplist;
use tokio::sync::mpsc;

use crate::{
	config::{ApplicationConfig, Config},
	session::stream::{AudioStream, ControlStream, VideoStream},
};

use self::compositor::frame::ExportedFrame;
use self::compositor::input::CompositorInputEvent;
use self::stream::{AudioStreamContext, VideoStreamContext};
pub use manager::SessionManager;

pub mod compositor;
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
#[allow(dead_code)]
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

fn create_pulse_context() -> Result<(Rc<RefCell<Mainloop>>, Rc<RefCell<Context>>), ()> {
	let mainloop = Rc::new(RefCell::new(
		Mainloop::new().ok_or_else(|| tracing::warn!("Failed to create PulseAudio mainloop."))?,
	));

	let mut proplist =
		Proplist::new().ok_or_else(|| tracing::warn!("Failed to create PulseAudio proplist."))?;
	proplist
		.set_str(pulse::proplist::properties::APPLICATION_NAME, "Moonshine")
		.map_err(|()| tracing::warn!("Failed to set PulseAudio application name."))?;

	let context = Rc::new(RefCell::new(
		Context::new_with_proplist(mainloop.borrow().deref(), "Moonshine context", &proplist)
			.ok_or_else(|| tracing::warn!("Failed to create PulseAudio context."))?,
	));

	context
		.borrow_mut()
		.connect(None, FlagSet::NOFLAGS, None)
		.map_err(|e| tracing::warn!("Failed to connect to PulseAudio server: {e}"))?;

	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				tracing::warn!("PulseAudio mainloop failed.");
				return Err(());
			},
			IterateResult::Success(_) => {},
		}

		match context.borrow().get_state() {
			pulse::context::State::Unconnected
			| pulse::context::State::Connecting
			| pulse::context::State::Authorizing
			| pulse::context::State::SettingName => {},
			pulse::context::State::Failed | pulse::context::State::Terminated => {
				tracing::warn!("PulseAudio context failed.");
				return Err(());
			},
			pulse::context::State::Ready => break,
		}
	}

	Ok((mainloop, context))
}

fn wait_for_operation(
	mainloop: &Rc<RefCell<Mainloop>>,
	operation: pulse::operation::Operation<dyn FnMut(bool)>,
) -> Result<(), ()> {
	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				tracing::warn!("PulseAudio mainloop failed during operation.");
				return Err(());
			},
			IterateResult::Success(_) => {},
		}

		match operation.get_state() {
			pulse::operation::State::Running => {},
			pulse::operation::State::Cancelled => {
				tracing::warn!("PulseAudio operation was cancelled.");
				return Err(());
			},
			pulse::operation::State::Done => return Ok(()),
		}
	}
}

fn create_audio_sink(name: &str) -> Result<u32, ()> {
	let (mainloop, context) = create_pulse_context()?;
	let argument = format!("sink_name={name} sink_properties=device.description={name}");
	let result: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));

	let operation = {
		let result = result.clone();
		context
			.borrow_mut()
			.introspect()
			.load_module("module-null-sink", &argument, move |module_index| {
				*result.borrow_mut() = Some(module_index);
			})
	};

	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				tracing::warn!("PulseAudio mainloop failed while loading module-null-sink.");
				return Err(());
			},
			IterateResult::Success(_) => {},
		}

		match operation.get_state() {
			pulse::operation::State::Running => {},
			pulse::operation::State::Cancelled => {
				tracing::warn!("load_module module-null-sink operation was cancelled.");
				return Err(());
			},
			pulse::operation::State::Done => break,
		}
	}

	let module_id = result
		.take()
		.ok_or_else(|| tracing::warn!("Failed to get module index for module-null-sink."))?;

	if module_id == u32::MAX {
		tracing::warn!("Failed to load module-null-sink (invalid index returned).");
		return Err(());
	}

	tracing::debug!(module_id, "Loaded module-null-sink");
	Ok(module_id)
}

fn unload_audio_sink(module_id: u32) {
	let Ok((mainloop, context)) = create_pulse_context() else {
		return;
	};
	let operation = context
		.borrow_mut()
		.introspect()
		.unload_module(module_id, |_success| {});
	let _ = wait_for_operation(&mainloop, operation);
}

fn create_audio_loopback(source: &str, sink: &str) -> Result<u32, ()> {
	let (mainloop, context) = create_pulse_context()?;
	let argument = format!("source={source}.monitor sink={sink}");
	let result: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));

	let operation = {
		let result = result.clone();
		context
			.borrow_mut()
			.introspect()
			.load_module("module-loopback", &argument, move |module_index| {
				*result.borrow_mut() = Some(module_index);
			})
	};

	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				tracing::warn!("PulseAudio mainloop failed while loading module-loopback.");
				return Err(());
			},
			IterateResult::Success(_) => {},
		}

		match operation.get_state() {
			pulse::operation::State::Running => {},
			pulse::operation::State::Cancelled => {
				tracing::warn!("load_module module-loopback operation was cancelled.");
				return Err(());
			},
			pulse::operation::State::Done => break,
		}
	}

	let module_id = result
		.take()
		.ok_or_else(|| tracing::warn!("Failed to get module index for module-loopback."))?;

	if module_id == u32::MAX {
		tracing::warn!("Failed to load module-loopback (invalid index returned).");
		return Err(());
	}

	tracing::debug!(module_id, "Loaded module-loopback");
	Ok(module_id)
}

fn get_default_sink() -> Result<String, ()> {
	let (mainloop, context) = create_pulse_context()?;
	let result = Rc::new(RefCell::new(None));

	let operation = {
		let result = result.clone();
		context.borrow().introspect().get_server_info(move |info| {
			if let Some(name) = info.default_sink_name.as_ref() {
				*result.borrow_mut() = Some(name.to_string());
			}
		})
	};

	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				tracing::warn!("PulseAudio mainloop failed while getting default sink.");
				return Err(());
			},
			IterateResult::Success(_) => {},
		}

		match operation.get_state() {
			pulse::operation::State::Running => {},
			pulse::operation::State::Cancelled => {
				tracing::warn!("get_server_info operation was cancelled.");
				return Err(());
			},
			pulse::operation::State::Done => break,
		}
	}

	result
		.take()
		.ok_or_else(|| tracing::warn!("Failed to get default sink name."))
}

fn set_default_sink(name: &str) -> Result<(), ()> {
	let (mainloop, context) = create_pulse_context()?;
	let operation = context
		.borrow_mut()
		.set_default_sink(name, |success| {
			if !success {
				tracing::warn!("set_default_sink callback reported failure.");
			}
		});
	wait_for_operation(&mainloop, operation)
}

#[allow(clippy::result_unit_err)]
impl Session {
	pub fn new(
		config: Config,
		context: SessionContext,
		stop_session_signal: ShutdownManager<SessionShutdownReason>,
		enet: Arc<Enet>,
	) -> Result<Self, ()> {
		let default_sink = get_default_sink().ok().filter(|s| s != "auto_null");
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

		// Start the headless compositor.
		let compositor_config = compositor::CompositorConfig {
			width: context.resolution.0,
			height: context.resolution.1,
			refresh_rate: context._refresh_rate,
			gpu: config.gpu.clone(),
		};
		let (frame_rx, input_tx, xdisplay_rx) =
			compositor::start_compositor(compositor_config, stop_session_signal.clone())
				.map_err(|e| tracing::warn!("Failed to start compositor: {e}"))?;

		// Launch the application in a background thread that waits for
		// XWayland to become ready. We must not block Session::new()
		// because the session manager processes commands sequentially
		// and stalling it would prevent the control stream from being
		// established on time.
		let app_context = context.clone();
		let app_sink = sink_name.clone();
		std::thread::Builder::new()
			.name("app-launcher".to_string())
			.spawn(move || -> Result<Child, ()> {
				let xdisplay = xdisplay_rx
					.recv_timeout(std::time::Duration::from_secs(5))
					.map_err(|e| tracing::warn!("Timed out waiting for XWayland display: {e}"))?;
				launch_application(&app_context, &app_sink, xdisplay)
			})
			.map_err(|e| tracing::warn!("Failed to spawn app launcher thread: {e}"))?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = SessionInner {
			config,
			video_stream: None,
			audio_stream: None,
			control_stream: None,
			frame_rx: Some(frame_rx),
			input_tx: Some(input_tx),
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

struct SessionInner {
	config: Config,
	video_stream: Option<VideoStream>,
	audio_stream: Option<AudioStream>,
	control_stream: Option<ControlStream>,
	frame_rx: Option<std::sync::mpsc::Receiver<ExportedFrame>>,
	input_tx: Option<calloop::channel::Sender<CompositorInputEvent>>,
	audio_sink_module_id: Option<u32>,
	audio_loopback_module_id: Option<u32>,
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
					let frame_rx = self.frame_rx.take();
					let video_stream = match VideoStream::new(
						self.config.clone(),
						video_stream_context,
						frame_rx,
						stop_session_manager.clone(),
					)
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
					let input_tx = self.input_tx.take().expect("Input sender already consumed");
					let control_stream = match ControlStream::new(
						self.config.clone(),
						video_stream.clone(),
						audio_stream.clone(),
						session_context.clone(),
						stop_session_manager.clone(),
						self.enet.clone(),
						input_tx,
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

		// Stop the systemd scope to kill the application and all of its
		// descendants. The scope was created with TimeoutStopSec=5, so
		// this blocks at most 5 seconds before systemd sends SIGKILL.
		let _ = Command::new("systemctl")
			.args(["--user", "stop", "moonshine-session.scope"])
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status();

		if let Some(module_id) = self.audio_loopback_module_id {
			unload_audio_sink(module_id);
		}

		if let Some(module_id) = self.audio_sink_module_id {
			unload_audio_sink(module_id);
		}

		tracing::debug!("Session stopped.");
	}
}

/// Launch the application as a child process.
///
/// The compositor has already set `WAYLAND_DISPLAY` in the process
/// environment, so the child inherits it and connects to our
/// headless compositor automatically.
fn launch_application(context: &SessionContext, sink_name: &str, xdisplay: u32) -> Result<Child, ()> {
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

	Command::new("systemd-run")
		.args(["--user", "--scope", "--collect", "--unit", "moonshine-session", "--property=TimeoutStopSec=5", "--"])
		.arg(program)
		.args(args)
		.env("PULSE_SINK", sink_name)
		.env("DISPLAY", format!(":{xdisplay}"))
		.stdout(
			log_file
				.try_clone()
				.map_err(|e| tracing::warn!("Failed to clone log file handle: {e}"))?,
		)
		.stderr(log_file)
		.stdin(Stdio::null())
		.spawn()
		.map_err(|e| tracing::warn!("Failed to launch application: {e}"))
}
