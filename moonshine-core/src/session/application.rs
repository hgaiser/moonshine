use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use zbus::proxy::SignalStream;
use zbus::{Connection, MatchRule, MessageStream, Proxy};
use zvariant::OwnedObjectPath;

pub fn default_launch_timeout() -> u64 {
	2
}

/// Configuration for a single application that can be launched in a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplicationConfig {
	/// Title of the application.
	pub title: String,

	/// Path to a boxart image.
	pub boxart: Option<PathBuf>,

	/// The command to run.
	pub command: Vec<String>,

	/// Commands to run before launching the application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after the streaming session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// systemd StandardOutput value. If not set, defaults to "null".
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<String>,

	/// systemd StandardError value. If not set, defaults to "null".
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<String>,

	/// Seconds to wait for the application to reach an active state after launch.
	#[serde(default = "default_launch_timeout")]
	pub launch_timeout_secs: u64,
}

impl Default for ApplicationConfig {
	fn default() -> Self {
		Self {
			title: String::new(),
			boxart: None,
			command: Vec::new(),
			pre_command: Vec::new(),
			post_command: Vec::new(),
			stdout: None,
			stderr: None,
			launch_timeout_secs: default_launch_timeout(),
		}
	}
}

impl ApplicationConfig {
	pub fn id(&self) -> i32 {
		let mut hasher = DefaultHasher::new();
		self.title.hash(&mut hasher);
		hasher.finish() as i32
	}
}

use crate::session::manager::SessionShutdownReason;

const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const PROPERTIES_CHANGED: &str = "PropertiesChanged";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const ACTIVE_STATE_PROPERTY: &str = "ActiveState";

const STOP_JOB_TIMEOUT: Duration = Duration::from_secs(2);
const UNIT_REMOVED_TIMEOUT: Duration = Duration::from_secs(2);

/// systemd only emits JobRemoved/UnitRemoved/PropertiesChanged bus signals to
/// connections that have called org.freedesktop.systemd1.Manager.Subscribe().
/// Without it, waiting for those signals only works when some *other* client
/// on the user bus happens to hold a subscription. Call this once for every
/// new session bus connection, before waiting on any systemd signals.
async fn subscribe_to_systemd_signals(conn: &Connection) -> Result<(), ()> {
	conn.call_method(Some(SYSTEMD_BUS), SYSTEMD_PATH, Some(SYSTEMD_MANAGER), "Subscribe", &())
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to systemd signals: {e}"))?;
	Ok(())
}

#[derive(Clone)]
pub(crate) struct LaunchOptions<'a> {
	pub unit_name: &'a str,
	pub program: &'a str,
	pub args: &'a [String],
	pub envs: &'a [String],
	pub timeout: Duration,
	pub pre_commands: &'a Vec<Vec<String>>,
	pub post_commands: &'a Vec<Vec<String>>,
	pub stdout_value: &'a Option<String>,
	pub stderr_value: &'a Option<String>,
}

/// Runtime context required to launch an application.
pub(crate) struct ApplicationContext {
	/// systemd transient unit name (e.g. `"moonshine-session.service"`).
	pub unit_name: String,
	/// Path to the PulseAudio socket created by the audio stream.
	pub pulse_socket_path: PathBuf,
	/// X11 display number reported by XWayland (e.g. `0` → `":0"`).
	pub xdisplay: u32,
	/// Wayland socket name reported by the compositor.
	pub wayland_display: String,
	/// Effective HDR mode — `true` only when the compositor confirmed an HDR-capable DMA-BUF format is in use.
	pub hdr: bool,
	/// Environment variables to pass on.
	pub extra_env: HashMap<String, String>,
}

pub(crate) struct Application {
	unit_name: String,
	config: ApplicationConfig,
	exit_monitor: Option<JoinHandle<()>>,
}

impl Application {
	pub async fn spawn(
		config: ApplicationConfig,
		context: ApplicationContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		let Some(program) = config.command.first() else {
			tracing::error!("Application command is empty.");
			return Err(());
		};
		let args = &config.command[1..];
		let envs = make_envs(&context)?;

		tracing::info!(program, ?args, "Launching application.");

		// Connect to the user session bus.
		let conn = Connection::session()
			.await
			.map_err(|e| tracing::error!("Failed to connect to session bus: {e}"))?;
		subscribe_to_systemd_signals(&conn).await?;

		// Stop any leftover unit from a previous session.
		let _ = stop_unit(&conn, &context.unit_name).await;

		// Launch the application as a transient systemd service unit.
		let options = LaunchOptions {
			unit_name: &context.unit_name,
			program,
			args,
			envs: &envs,
			timeout: Duration::from_secs(config.launch_timeout_secs),
			pre_commands: &config.pre_command,
			post_commands: &config.post_command,
			stdout_value: &config.stdout,
			stderr_value: &config.stderr,
		};

		let unit_path = match start_transient_service(&conn, &options).await {
			Ok(unit_path) => unit_path,
			Err(_) => {
				// Best effort cleanup on launch failure.
				stop_unit(&conn, &context.unit_name).await.ok();
				return Err(());
			},
		};
		let exit_monitor = spawn_unit_exit_monitor(conn.clone(), context.unit_name.clone(), unit_path, stop);

		Ok(Self {
			unit_name: context.unit_name,
			config,
			exit_monitor: Some(exit_monitor),
		})
	}
}

impl Drop for Application {
	fn drop(&mut self) {
		tracing::info!("Application '{}' is exiting.", self.config.title);
		if let Some(handle) = self.exit_monitor.take() {
			handle.abort();
		}

		// Unfortunately we have no `drop_async` yet, so we must spawn an async runtime to call stop_unit.
		let unit_name = self.unit_name.clone();
		std::thread::spawn(move || {
			let rt = tokio::runtime::Runtime::new().unwrap();
			rt.block_on(stop_unit_owned(unit_name)).ok();
		})
		.join()
		.unwrap();
	}
}

/// Build environment variables for the application based on the context (e.g. display, PulseAudio socket).
fn make_envs(context: &ApplicationContext) -> Result<Vec<String>, ()> {
	// Build environment variables as "KEY=value" strings for systemd.
	let mut envs: Vec<String> = vec![
		format!("PULSE_SERVER=unix:{}", context.pulse_socket_path.display()),
		format!(
			"PULSE_RUNTIME_PATH={}",
			context
				.pulse_socket_path
				.parent()
				.ok_or_else(|| tracing::error!("Failed to get parent directory of PulseAudio socket."))?
				.to_string_lossy()
		),
		format!("DISPLAY=:{}", context.xdisplay),
		format!("WAYLAND_DISPLAY={}", context.wayland_display),
		format!("MOONSHINE_WAYLAND_DISPLAY={}", context.wayland_display),
		// Activate the moonshine WSI Vulkan layer.
		"ENABLE_MOONSHINE_WSI=1".to_string(),
	];

	if context.hdr {
		// DXVK's dxgi.dll gates HDR color space exposure on this env var.
		// Without it, both DX11 (DXVK) and DX12 (vkd3d-proton via DXVK dxgi)
		// games will not see HDR as available.
		envs.push("DXVK_HDR=1".to_string());
		// Signal HDR mode to the moonshine-wsi layer so it can advertise HDR
		// surface formats correctly (the factory global is always present for
		// SDR sessions too, so we need an explicit capability signal).
		envs.push("MOONSHINE_HDR=1".to_string());
	}

	for (key, value) in &context.extra_env {
		envs.push(format!("{key}={value}"));
	}

	Ok(envs)
}

/// Wait for a `JobRemoved` signal matching the given job path, accepting only `"done"` as success.
async fn wait_for_job_signal(
	job_stream: &mut SignalStream<'_>,
	job_path: &OwnedObjectPath,
	timeout: Duration,
	label: &str,
) -> Result<(), ()> {
	let result = tokio::time::timeout(timeout, async {
		while let Some(message) = job_stream.next().await {
			let body: (u32, OwnedObjectPath, String, String) = message
				.body()
				.deserialize()
				.map_err(|e| tracing::error!("Failed to deserialize JobRemoved signal: {e}"))?;
			let (_, ref path, ref unit, ref result) = body;

			if path != job_path {
				continue;
			}

			return match result.as_str() {
				"done" => Ok(()),
				other => {
					tracing::warn!(result = other, unit = unit, "{label} job finished unsuccessfully.");
					Err(())
				},
			};
		}
		Err(())
	})
	.await;

	match result {
		Ok(Ok(())) => Ok(()),
		Ok(Err(())) => {
			tracing::warn!(label, "Received failure result for {label} job.");
			Err(())
		},
		Err(_) => {
			tracing::warn!(timeout_secs = timeout.as_secs(), "Timed out waiting for {label} job.");
			Err(())
		},
	}
}

/// Stop a unit via the user session bus.
///
/// Waits for the stop job to complete and the unit to be removed, with a timeout.
async fn stop_unit(conn: &Connection, unit_name: &str) -> Result<(), ()> {
	// Subscribe to both JobRemoved and UnitRemoved before calling StopUnit to avoid races.
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut job_removed_stream = proxy
		.receive_signal("JobRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to JobRemoved signals: {e}"))?;
	let mut unit_removed_stream = proxy
		.receive_signal("UnitRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to UnitRemoved signals: {e}"))?;

	// Call StopUnit, which queues a stop job but does not wait for it to complete.
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"StopUnit",
			&(unit_name, "replace"),
		)
		.await
		.map_err(|e| match e {
			zbus::Error::MethodError(ref err_name, ..)
				if err_name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
			{
				tracing::debug!("Unit was already stopped.");
			},
			e => {
				tracing::error!("Failed to get unit: {e}");
			},
		})?;

	let job_path = reply
		.body()
		.deserialize()
		.map_err(|e| tracing::warn!("Failed to deserialize StopUnit reply for {unit_name}: {e}"))?;

	// Wait for JobRemoved with result "done" — the stop job completed.
	wait_for_job_signal(&mut job_removed_stream, &job_path, STOP_JOB_TIMEOUT, "Stop").await?;

	// Stop job succeeded — wait for the unit to be collected and removed.
	let unit_result = tokio::time::timeout(UNIT_REMOVED_TIMEOUT, async {
		while let Some(message) = unit_removed_stream.next().await {
			let (id, _path): (String, OwnedObjectPath) = message
				.body()
				.deserialize()
				.map_err(|e| tracing::error!("Failed to deserialize UnitRemoved signal: {e}"))?;

			if id == unit_name {
				return Ok(());
			}
		}
		Err(())
	})
	.await;

	match unit_result {
		Ok(Ok(())) => {
			tracing::debug!("Leftover unit {unit_name} stopped and unloaded.");
			Ok(())
		},
		Ok(Err(())) => Err(()),
		Err(_) => {
			tracing::warn!(
				timeout_secs = UNIT_REMOVED_TIMEOUT.as_secs(),
				unit = unit_name,
				"Timed out waiting for leftover unit to be removed."
			);
			Err(())
		},
	}
}

/// Stop a unit with a new session bus connection.
async fn stop_unit_owned(unit_name: String) -> Result<(), ()> {
	let conn = Connection::session().await.map_err(|e| {
		tracing::error!("Failed to connect to session bus: {e}");
	})?;
	subscribe_to_systemd_signals(&conn).await?;
	stop_unit(&conn, &unit_name).await
}

fn spawn_unit_exit_monitor(
	conn: Connection,
	unit_name: String,
	unit_path: OwnedObjectPath,
	stop: ShutdownManager<SessionShutdownReason>,
) -> JoinHandle<()> {
	tokio::spawn(async move {
		tokio::select! {
			state = wait_for_unit_terminal_state(&conn, &unit_name, &unit_path) => {
				match state {
					Ok(state) => {
						tracing::info!(unit = unit_name, state, "Application unit exited; stopping session.");
						let _ = stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
					},
					Err(()) => {
						tracing::warn!(unit = unit_name, "Application unit monitor stopped unexpectedly.");
					},
				}
			},
			_ = stop.wait_shutdown_triggered() => {},
		}
	})
}

async fn wait_for_unit_terminal_state(
	conn: &Connection,
	unit_name: &str,
	unit_path: &OwnedObjectPath,
) -> Result<String, ()> {
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut unit_removed_stream = proxy
		.receive_signal("UnitRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to UnitRemoved signals: {e}"))?;

	let rule = MatchRule::builder()
		.msg_type(zbus::message::Type::Signal)
		.sender(SYSTEMD_BUS)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.path(unit_path)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.interface(PROPERTIES_INTERFACE)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.member(PROPERTIES_CHANGED)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.build();
	let mut state_stream = MessageStream::for_match_rule(rule, conn, None)
		.await
		.map_err(|e| tracing::error!("Failed to create state stream: {e}"))?;

	match current_unit_state(conn, unit_path).await {
		Ok(state) => {
			if let Some(state) = terminal_state(state.as_str()) {
				return Ok(state.to_string());
			}
		},
		Err(()) => return Ok("removed".to_string()),
	}

	loop {
		tokio::select! {
			message = state_stream.next() => {
				let Some(Ok(signal)) = message else {
					return Err(());
				};
				let body = signal.body();
				let (iface, changed, _): (String, HashMap<String, zvariant::Value<'_>>, Vec<String>) =
					body.deserialize()
						.map_err(|e| tracing::error!("Failed to deserialize PropertiesChanged signal: {e}"))?;
				if iface != UNIT_INTERFACE {
					continue;
				}
				if let Some(zvariant::Value::Str(state)) = changed.get(ACTIVE_STATE_PROPERTY) {
					if let Some(state) = terminal_state(state.as_str()) {
						return Ok(state.to_string());
					}
				}
			},
			message = unit_removed_stream.next() => {
				let Some(message) = message else {
					return Err(());
				};
				let (id, _path): (String, OwnedObjectPath) = message
					.body()
					.deserialize()
					.map_err(|e| tracing::error!("Failed to deserialize UnitRemoved signal: {e}"))?;
				if id == unit_name {
					return Ok("removed".to_string());
				}
			},
		}
	}
}

async fn current_unit_state(conn: &Connection, unit_path: &OwnedObjectPath) -> Result<String, ()> {
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			unit_path,
			Some(PROPERTIES_INTERFACE),
			"Get",
			&(UNIT_INTERFACE, ACTIVE_STATE_PROPERTY),
		)
		.await
		.map_err(|e| tracing::error!("Failed to get unit state: {e}"))?;

	let body = reply.body();
	let (variant,): (zvariant::Value<'_>,) = body.deserialize().map_err(|e| {
		tracing::error!("Failed to deserialize unit state: {e}");
	})?;
	match variant {
		zvariant::Value::Str(state) => Ok(state.to_string()),
		_ => Ok("unknown".to_string()),
	}
}

fn terminal_state(state: &str) -> Option<&'static str> {
	match state {
		"inactive" => Some("inactive"),
		"failed" => Some("failed"),
		_ => None,
	}
}

/// Launch the application as a transient systemd service unit via D-Bus.
async fn start_transient_service(conn: &Connection, options: &LaunchOptions<'_>) -> Result<OwnedObjectPath, ()> {
	// Resolve exec entries in a blocking task — `which::which` does filesystem lookups.
	let (pre_entries, main_entry, post_entries) = tokio::task::spawn_blocking({
		let pre_commands = options.pre_commands.clone();
		let post_commands = options.post_commands.clone();
		let main_program = options.program.to_string();
		let args = options.args.to_vec();
		move || -> Result<_, ()> {
			let program_for_error = main_program.clone();
			Ok((
				build_exec_entries(&pre_commands),
				build_exec_entry(main_program, args.clone()).ok_or_else(move || {
					tracing::error!("Main program '{}' not found in PATH.", program_for_error);
				})?,
				build_exec_entries(&post_commands),
			))
		}
	})
	.await
	.map_err(|e| tracing::error!("spawn_blocking panicked: {e}"))??;

	tracing::debug!(?pre_entries, ?main_entry, ?post_entries, "Building transient service");

	// Properties: a(sv) — array of (property_name: s, value: v)
	// zvariant::Value has D-Bus type 'v' (variant), so Vec<(&str, Value)> serialises as a(sv).
	//
	// IMPORTANT: do NOT use zvariant::Array::from(Vec<Value>) — it always produces `av`
	// (array of variant). Build typed arrays with Array::new(signature) + append() instead.
	let mut properties: Vec<(&str, zvariant::Value<'_>)> = vec![
		("Type", zvariant::Value::Str("exec".into())),
		("Slice", zvariant::Value::Str("moonshine.slice".into())),
		// Environment: as
		("Environment", zvariant::Value::from(options.envs.to_vec())),
		// ExecStart: a(sasb)
		("ExecStart", build_exec_array(&[main_entry])?),
		("TimeoutStopUSec", zvariant::Value::U64(5_000_000)),
		("CollectMode", zvariant::Value::Str("inactive-or-failed".into())),
		// StandardOutput/StandardError: systemd expects `s` (string)
		// Valid values: inherit, null, tty, journal, kmsg, journal+console,
		// file:path, append:path, truncate:path, socket, fd:name
		(
			"StandardOutput",
			zvariant::Value::Str(options.stdout_value.as_deref().unwrap_or("null").into()),
		),
		(
			"StandardError",
			zvariant::Value::Str(options.stderr_value.as_deref().unwrap_or("null").into()),
		),
	];

	// Only include ExecStartPre/ExecStopPost when non-empty: an empty a(sasb) array still
	// needs a valid element signature, and omitting absent properties is cleaner.
	if !pre_entries.is_empty() {
		properties.push(("ExecStartPre", build_exec_array(&pre_entries)?));
	}
	if !post_entries.is_empty() {
		properties.push(("ExecStopPost", build_exec_array(&post_entries)?));
	}

	// Aux units: empty a(sa(sv))
	let aux: Vec<(&str, Vec<(&str, zvariant::Value)>)> = Vec::new();

	// Subscribe to JobRemoved before calling StartTransientUnit to avoid a race condition.
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut job_stream = proxy
		.receive_signal("JobRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to JobRemoved signals: {e}"))?;

	// Call StartTransientUnit.
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"StartTransientUnit",
			&(options.unit_name, "replace", &properties, &aux),
		)
		.await
		.map_err(|e| tracing::warn!("Failed to start transient service: {e}"))?;

	let (job_path,): (OwnedObjectPath,) = reply
		.body()
		.deserialize()
		.map_err(|e| tracing::warn!("Failed to deserialize StartTransientUnit reply: {e}"))?;

	// Wait for the launch job to complete.
	wait_for_job_signal(&mut job_stream, &job_path, options.timeout, "Application launch").await?;

	// Get the unit object path — now that the job is done, the unit should exist.
	let unit_path = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnit",
			&options.unit_name,
		)
		.await
		.map_err(|e| tracing::error!("Failed to get unit object path: {e}"))?;
	let (path,): (OwnedObjectPath,) = unit_path
		.body()
		.deserialize()
		.map_err(|e| tracing::error!("Failed to deserialize unit object path: {e}"))?;

	// Check current state before subscribing — catches apps that exit immediately.
	let state = current_unit_state(conn, &path).await?;
	if terminal_state(&state).is_some() {
		tracing::warn!(state = state, "Application exited immediately after launch.");
		return Err(());
	}

	// Subscribe to PropertiesChanged on this unit object.
	let rule = MatchRule::builder()
		.msg_type(zbus::message::Type::Signal)
		.sender(SYSTEMD_BUS)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.path(&path)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.interface(PROPERTIES_INTERFACE)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.member(PROPERTIES_CHANGED)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.build();
	let mut state_stream = MessageStream::for_match_rule(rule, conn, None)
		.await
		.map_err(|e| tracing::error!("Failed to create state stream: {e}"))?;

	// Wait for ActiveState to change to a terminal failure state.
	let failure_result = tokio::time::timeout(options.timeout, async {
		while let Some(Ok(signal)) = state_stream.next().await {
			let body = signal.body();
			let (iface, changed, _): (String, HashMap<String, zvariant::Value<'_>>, Vec<String>) =
				body.deserialize()
					.map_err(|e| tracing::error!("Failed to deserialize PropertiesChanged signal: {e}"))?;
			if iface != UNIT_INTERFACE {
				continue;
			}
			if let Some(zvariant::Value::Str(state)) = changed.get(ACTIVE_STATE_PROPERTY) {
				return match state.to_string().as_str() {
					"failed" | "inactive" => {
						tracing::warn!(state = state.to_string(), "Application exited shortly after launch.");
						Err(())
					},
					_ => continue, // e.g. "activating" → "active" — keep waiting
				};
			}
		}
		Ok(())
	})
	.await;

	match failure_result {
		Ok(Ok(())) => {
			// Timeout expired — unit is still alive, launch succeeded.
			tracing::info!("Application launched in service {}", options.unit_name);
			Ok(path)
		},
		Ok(Err(())) => Err(()),
		Err(_) => {
			// Timeout expired — unit is still alive, launch succeeded.
			tracing::info!("Application launched in service {}", options.unit_name);
			Ok(path)
		},
	}
}

/// Build a list of exec command entries from a list of command configs.
/// Each entry is (absolute_path, argv, ignore_errors=false).
fn build_exec_entries(commands: &[Vec<String>]) -> Vec<(String, Vec<String>, bool)> {
	commands
		.iter()
		.filter_map(|cmd| {
			let first = cmd.first()?;
			let abs = which::which(first).ok()?;
			let abs_str = abs.to_str()?.to_string();
			let argv: Vec<String> = std::iter::once(abs_str.clone())
				.chain(cmd[1..].iter().cloned())
				.collect();
			Some((abs_str, argv, false))
		})
		.collect()
}

/// Build a single exec command entry from a program path and args.
/// Returns (absolute_path, argv, ignore_errors=false), or None if the program is not found.
fn build_exec_entry(program: String, args: Vec<String>) -> Option<(String, Vec<String>, bool)> {
	let abs = which::which(&program).ok()?;
	let abs_str = abs.to_str()?.to_string();
	let argv: Vec<String> = std::iter::once(abs_str.clone()).chain(args.iter().cloned()).collect();
	Some((abs_str, argv, false))
}

/// Build a properly-typed `a(sasb)` array for systemd ExecStart/ExecStartPre/ExecStopPost.
///
/// Must use `Array::new(signature)` + `append()` rather than `Array::from(Vec<Value>)`,
/// which always produces `av` regardless of the element type.
fn build_exec_array(entries: &[(String, Vec<String>, bool)]) -> Result<zvariant::Value<'static>, ()> {
	let element_sig = zvariant::Signature::structure([
		zvariant::Signature::Str,
		zvariant::Signature::array(zvariant::Signature::Str),
		zvariant::Signature::Bool,
	]);
	let mut arr = zvariant::Array::new(&element_sig);
	for entry in entries {
		arr.append(zvariant::Value::from(entry.clone()))
			.map_err(|e| tracing::error!("Failed to append exec entry: {e}"))?;
	}
	Ok(zvariant::Value::Array(arr))
}
