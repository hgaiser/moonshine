//! Connects the web control panel (the `moonshine-ui` crate) to Moonshine.
//!
//! The control panel reads its data from a [`UiBackend`] and asks it to perform actions. Changes to
//! the configuration are written into `config.toml` with `toml_edit`, so the user's comments and
//! layout stay in place. Each setting takes effect now, at the next stream, or after a restart.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_shutdown::ShutdownManager;
use moonshine_ui::protocol::{
	Action, ApplicationForm, ApplicationInfo, DeviceInfo, FrameSample, HealthCheck, HealthInfo, PairingInfo,
	PairingRequestInfo, ScannedApplication, ScannerForm, ScannerInfo, ScannerKind, ServerInfo, SessionInfo, Setting,
	Snapshot, StatsEvent, StreamStart,
};
use moonshine_ui::{Backend, LogBuffer, Network, Options, UiServer};
use serde_json::Value;
use tokio::sync::broadcast;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use crate::ShutdownReason;
use crate::app_scanner::{ApplicationManager, ApplicationScannerConfig};
use crate::clients::ClientManager;
use crate::config::Config;
use crate::healthcheck::{self, CheckOutcome, HealthReport};
use crate::session::application::ApplicationConfig;
use crate::session::manager::{SessionManager, SessionPhase, SessionSettings};
use crate::session::stream::video::{FrameStats, StreamEvent, StreamInfo, VideoDynamicRange, VideoFormat};
use crate::webserver::Webserver;

/// When a changed setting takes effect.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
	Now,
	NextStream,
	Restart,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
	Bool,
	Text,
	/// Text that removes the key when empty.
	OptionalText,
	Number,
	Range,
	/// A list of address ranges.
	Networks,
}

struct SettingSpec {
	key: &'static str,
	path: &'static [&'static str],
	group: &'static str,
	label: &'static str,
	help: &'static str,
	kind: Kind,
	effect: Effect,
	unit: Option<&'static str>,
	min: i64,
	max: i64,
	placeholder: Option<&'static str>,
	advanced: bool,
}

const fn setting(
	key: &'static str,
	path: &'static [&'static str],
	group: &'static str,
	label: &'static str,
	help: &'static str,
	kind: Kind,
	effect: Effect,
) -> SettingSpec {
	SettingSpec {
		key,
		path,
		group,
		label,
		help,
		kind,
		effect,
		unit: None,
		min: 0,
		max: i64::MAX,
		placeholder: None,
		advanced: false,
	}
}

const fn bounded(mut spec: SettingSpec, min: i64, max: i64, unit: Option<&'static str>) -> SettingSpec {
	spec.min = min;
	spec.max = max;
	spec.unit = unit;
	spec
}

const fn advanced(mut spec: SettingSpec) -> SettingSpec {
	spec.advanced = true;
	spec
}

const fn placeholder(mut spec: SettingSpec, text: &'static str) -> SettingSpec {
	spec.placeholder = Some(text);
	spec
}

const GENERAL: &str = "General";
const VIDEO: &str = "Video";
const INPUT: &str = "Keyboard and controllers";
const NETWORK: &str = "Network and access";
const DIAGNOSTICS: &str = "Diagnostics";

const SETTINGS: &[SettingSpec] = &[
	setting(
		"name",
		&["name"],
		GENERAL,
		"Computer name",
		"The name Moonlight shows.",
		Kind::Text,
		Effect::Restart,
	),
	setting(
		"enable_pairing",
		&["webserver", "enable_pairing"],
		GENERAL,
		"Allow new devices to pair",
		"Paired devices still work when this is off.",
		Kind::Bool,
		Effect::Now,
	),
	setting(
		"inhibit_sleep",
		&["inhibit_sleep"],
		GENERAL,
		"Keep the computer awake while streaming",
		"Blocks suspend during a stream.",
		Kind::Bool,
		Effect::NextStream,
	),
	setting(
		"hdr",
		&["compositor", "hdr"],
		VIDEO,
		"HDR",
		"Stream in HDR10 when the client and the game support it.",
		Kind::Bool,
		Effect::Restart,
	),
	placeholder(
		setting(
			"gpu",
			&["compositor", "gpu"],
			VIDEO,
			"GPU",
			"The render node that renders the stream. Empty selects one automatically.",
			Kind::OptionalText,
			Effect::NextStream,
		),
		"/dev/dri/renderD128",
	),
	bounded(
		setting(
			"fec_percentage",
			&["stream", "video", "fec_percentage"],
			VIDEO,
			"Error correction",
			"Extra packets that repair lost packets, for example on Wi-Fi. A higher value uses more bandwidth.",
			Kind::Range,
			Effect::NextStream,
		),
		0,
		100,
		Some("%"),
	),
	setting(
		"encrypt",
		&["stream", "video", "encrypt"],
		VIDEO,
		"Encrypt video",
		"AES-128-GCM. Turn off only on a network you trust.",
		Kind::Bool,
		Effect::Restart,
	),
	setting(
		"keyboard_layout",
		&["compositor", "keyboard", "layout"],
		INPUT,
		"Keyboard layout",
		"XKB layout for the stream, for example us or de.",
		Kind::Text,
		Effect::NextStream,
	),
	bounded(
		setting(
			"hold_ms",
			&["stream", "control", "gamepad", "home_button", "hold_ms"],
			INPUT,
			"Hold Back for Home",
			"Hold Back this long to press Home. 0 turns this off.",
			Kind::Number,
			Effect::NextStream,
		),
		0,
		10_000,
		Some("ms"),
	),
	setting(
		"suppress_home",
		&["stream", "control", "gamepad", "home_button", "suppress_home"],
		INPUT,
		"Ignore the controller's Home button",
		"Use this when the client device uses Home for itself.",
		Kind::Bool,
		Effect::NextStream,
	),
	advanced(setting(
		"ui_allowed_networks",
		&["ui", "allowed_networks"],
		NETWORK,
		"Who can open this page",
		"Address ranges that can use this control panel.",
		Kind::Networks,
		Effect::Restart,
	)),
	advanced(setting(
		"address",
		&["address"],
		NETWORK,
		"Listen address",
		"Use 0.0.0.0 for IPv4 only, or :: for IPv4 and IPv6.",
		Kind::Text,
		Effect::Restart,
	)),
	advanced(bounded(
		setting(
			"http_port",
			&["webserver", "port"],
			NETWORK,
			"HTTP port",
			"Moonlight finds the computer on this port.",
			Kind::Number,
			Effect::Restart,
		),
		1,
		65535,
		None,
	)),
	advanced(bounded(
		setting(
			"ui_port",
			&["ui", "port"],
			NETWORK,
			"Control panel port",
			"The port of this page.",
			Kind::Number,
			Effect::Restart,
		),
		1,
		65535,
		None,
	)),
	advanced(bounded(
		setting(
			"max_packet_size",
			&["stream", "video", "max_packet_size"],
			NETWORK,
			"Maximum video packet size",
			"Lower it for VPNs such as Tailscale or WireGuard.",
			Kind::Number,
			Effect::Restart,
		),
		0,
		65_000,
		Some("bytes"),
	)),
	advanced(bounded(
		setting(
			"timeout",
			&["stream", "timeout"],
			NETWORK,
			"Disconnect timeout",
			"Close the stream when the client is silent this long.",
			Kind::Number,
			Effect::NextStream,
		),
		1,
		3600,
		Some("s"),
	)),
	advanced(bounded(
		setting(
			"pool_size",
			&["compositor", "pool_size"],
			DIAGNOSTICS,
			"Frame buffers",
			"Frames the encoder may hold at once.",
			Kind::Number,
			Effect::NextStream,
		),
		1,
		8,
		None,
	)),
	advanced(setting(
		"log_frame_spikes",
		&["stream", "video", "log_frame_spikes"],
		DIAGNOSTICS,
		"Log slow frames",
		"Write a warning when a frame takes longer than its frame time.",
		Kind::Bool,
		Effect::NextStream,
	)),
	advanced(placeholder(
		setting(
			"stats_socket",
			&["stats", "socket"],
			DIAGNOSTICS,
			"Stats socket",
			"Unix socket for external tools. Empty turns it off.",
			Kind::OptionalText,
			Effect::Restart,
		),
		"$XDG_RUNTIME_DIR/moonshine-stats.sock",
	)),
];

/// What the control panel needs to start.
pub struct UiContext {
	pub config: Config,
	pub config_path: PathBuf,
	pub session_manager: SessionManager,
	pub client_manager: ClientManager,
	pub application_manager: ApplicationManager,
	pub webserver: Webserver,
	pub health: Option<HealthReport>,
	pub supported_codecs: u32,
	pub hdr_supported: bool,
	pub logs: LogBuffer,
	/// Set when the user asks for a restart, before the shutdown starts.
	pub restart_requested: Arc<AtomicBool>,
	pub shutdown: ShutdownManager<ShutdownReason>,
}

/// Start the control panel, if the configuration enables it.
pub async fn spawn(context: UiContext) -> Result<Option<UiServer>, ()> {
	let ui = context.config.ui.clone();
	if !ui.enabled {
		return Ok(None);
	}

	let mut allowed = Vec::new();
	for range in &ui.allowed_networks {
		allowed.push(Network::parse(range).map_err(|e| tracing::error!("Invalid [ui] allowed_networks range: {e}"))?);
	}

	// Listen on loopback only, unless another address range may open the control panel.
	let listen: Vec<SocketAddr> = if allowed.iter().all(Network::is_loopback) {
		vec![
			SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ui.port),
			SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), ui.port),
		]
	} else {
		(context.config.address.as_str(), ui.port)
			.to_socket_addrs()
			.map_err(|e| tracing::error!("Failed to resolve the control panel address: {e}"))?
			.collect()
	};

	let host = gethostname::gethostname().to_string_lossy().to_string();
	let mut host_names = vec![host.clone(), format!("{host}.local")];
	host_names.extend(
		context
			.config
			.ui
			.allowed_hosts
			.iter()
			.map(|name| name.trim().to_string()),
	);

	let logs = context.logs.clone();
	let backend = Arc::new(UiBackend::new(context));
	let server = moonshine_ui::serve(
		backend,
		Options {
			listen,
			allowed_networks: allowed,
			host_names,
		},
		logs,
	)
	.await
	.map_err(|e| tracing::error!("{e}"))?;

	Ok(Some(server))
}

pub struct UiBackend {
	/// The configuration Moonshine started with.
	startup: Config,
	/// The configuration as last saved.
	current: Mutex<Config>,
	config_path: PathBuf,
	/// Serializes changes to the configuration file.
	edit_lock: tokio::sync::Mutex<()>,
	session_manager: SessionManager,
	client_manager: ClientManager,
	application_manager: ApplicationManager,
	webserver: Webserver,
	health: Arc<Mutex<HealthInfo>>,
	gpu_name: String,
	supported_codecs: u32,
	hdr_supported: bool,
	logs: LogBuffer,
	stats_tx: broadcast::Sender<StatsEvent>,
	stream: Arc<Mutex<Option<StreamStart>>>,
	started: u64,
	restart_requested: Arc<AtomicBool>,
	shutdown: ShutdownManager<ShutdownReason>,
}

fn unix(time: SystemTime) -> u64 {
	time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn health_info(report: &HealthReport) -> HealthInfo {
	HealthInfo {
		checked_at: unix(SystemTime::now()),
		running: false,
		checks: report
			.checks
			.iter()
			.map(|check| HealthCheck {
				name: check.name.to_string(),
				status: match check.outcome {
					CheckOutcome::Passed => "ok",
					CheckOutcome::Warning => "warn",
					CheckOutcome::Failed => "fail",
				}
				.to_string(),
				message: check.message.clone(),
			})
			.collect(),
	}
}

fn stream_start(info: &StreamInfo) -> StreamStart {
	StreamStart {
		width: info.width,
		height: info.height,
		fps: info.fps,
		bitrate_kbps: info.bitrate as u64 / 1000,
		codec: match info.video_format {
			VideoFormat::H264 => "H.264",
			VideoFormat::Hevc => "HEVC",
			VideoFormat::Av1 => "AV1",
		}
		.to_string(),
		hdr: info.dynamic_range == VideoDynamicRange::Hdr,
	}
}

fn micros(duration: std::time::Duration) -> u32 {
	duration.as_micros().min(u128::from(u32::MAX)) as u32
}

fn frame_sample(frame: &FrameStats) -> FrameSample {
	FrameSample {
		wait_us: micros(frame.channel_wait),
		import_us: micros(frame.import),
		convert_us: micros(frame.convert),
		encode_us: micros(frame.submit + frame.encode_wait),
		packetize_us: micros(frame.packetize),
		send_us: micros(frame.send),
		total_us: micros(frame.total),
		bytes: frame.encoded_bytes.min(u32::MAX as usize) as u32,
		key_frame: frame.is_key_frame,
	}
}

fn codec_names(codecs: u32) -> Vec<String> {
	let mut names = Vec::new();
	if codecs & healthcheck::CODEC_H264 != 0 {
		names.push("H.264".to_string());
	}
	if codecs & (healthcheck::CODEC_HEVC | healthcheck::CODEC_HEVC_MAIN10) != 0 {
		names.push("HEVC".to_string());
	}
	if codecs & (healthcheck::CODEC_AV1_MAIN8 | healthcheck::CODEC_AV1_MAIN10) != 0 {
		names.push("AV1".to_string());
	}
	names
}

impl UiBackend {
	fn new(context: UiContext) -> Self {
		let health = context.health.as_ref().map(health_info).unwrap_or_default();
		let gpu_name = context
			.health
			.as_ref()
			.map(|report| report.gpu_name.clone())
			.unwrap_or_default();

		// Translate the video statistics into the control panel's terms.
		let (stats_tx, _) = broadcast::channel(1024);
		let stream = Arc::new(Mutex::new(None));
		let (current, mut events) = context.session_manager.join_stats();
		*stream.lock().unwrap_or_else(PoisonError::into_inner) = current.as_ref().map(stream_start);
		tokio::spawn({
			let stats_tx = stats_tx.clone();
			let stream = stream.clone();
			async move {
				loop {
					let event = match events.recv().await {
						Ok(StreamEvent::Started(info)) => {
							let start = stream_start(&info);
							*stream.lock().unwrap_or_else(PoisonError::into_inner) = Some(start.clone());
							StatsEvent::Started(start)
						},
						Ok(StreamEvent::Frame(frame)) => StatsEvent::Frame(frame_sample(&frame)),
						Ok(StreamEvent::Stopped) => {
							*stream.lock().unwrap_or_else(PoisonError::into_inner) = None;
							StatsEvent::Stopped
						},
						Err(broadcast::error::RecvError::Lagged(_)) => continue,
						Err(broadcast::error::RecvError::Closed) => break,
					};
					let _ = stats_tx.send(event);
				}
			}
		});

		Self {
			current: Mutex::new(context.config.clone()),
			startup: context.config,
			config_path: context.config_path,
			edit_lock: tokio::sync::Mutex::new(()),
			session_manager: context.session_manager,
			client_manager: context.client_manager,
			application_manager: context.application_manager,
			webserver: context.webserver,
			health: Arc::new(Mutex::new(health)),
			gpu_name,
			supported_codecs: context.supported_codecs,
			hdr_supported: context.hdr_supported,
			logs: context.logs,
			stats_tx,
			stream,
			started: unix(SystemTime::now()),
			restart_requested: context.restart_requested,
			shutdown: context.shutdown,
		}
	}

	fn current(&self) -> Config {
		self.current.lock().unwrap_or_else(PoisonError::into_inner).clone()
	}

	fn device_names(&self) -> BTreeMap<String, String> {
		self.client_manager
			.paired_devices()
			.unwrap_or_default()
			.into_iter()
			.map(|device| (device.fingerprint, device.name))
			.collect()
	}

	async fn session_info(&self) -> Option<(SessionInfo, Option<String>)> {
		let status = self.session_manager.status().await?;
		let device = status.client.as_ref().map(|fingerprint| {
			self.device_names()
				.get(fingerprint)
				.filter(|name| !name.is_empty())
				.cloned()
				.unwrap_or_else(|| default_device_name(fingerprint))
		});
		let info = SessionInfo {
			phase: match status.phase {
				SessionPhase::Initialized => "starting",
				SessionPhase::Launched => "waiting",
				SessionPhase::Active => "streaming",
			}
			.to_string(),
			application: status.context.application.title.clone(),
			device,
			width: status.context.resolution.0,
			height: status.context.resolution.1,
			fps: status.context.refresh_rate,
			hdr: status.context.hdr,
			audio_channels: status.context.audio_channels as u32,
			stream: status.stream.as_ref().map(stream_start),
			started: unix(status.started),
		};
		Some((info, status.client))
	}

	fn settings(&self, config: &Config) -> Vec<Setting> {
		let values = serde_json::to_value(config).unwrap_or(Value::Null);
		SETTINGS
			.iter()
			.map(|spec| {
				let mut value = spec.path.iter().fold(&values, |value, key| &value[*key]).clone();
				if value.is_null() && matches!(spec.kind, Kind::Text | Kind::OptionalText) {
					value = Value::String(String::new());
				}
				Setting {
					key: spec.key.to_string(),
					group: spec.group.to_string(),
					label: spec.label.to_string(),
					help: spec.help.to_string(),
					kind: match spec.kind {
						Kind::Bool => "bool",
						Kind::Text | Kind::OptionalText => "text",
						Kind::Number => "number",
						Kind::Range => "range",
						Kind::Networks => "list",
					}
					.to_string(),
					value,
					effect: match spec.effect {
						Effect::Now => "now",
						Effect::NextStream => "next",
						Effect::Restart => "restart",
					}
					.to_string(),
					unit: spec.unit.map(str::to_string),
					min: matches!(spec.kind, Kind::Number | Kind::Range).then_some(spec.min),
					max: (matches!(spec.kind, Kind::Number | Kind::Range) && spec.max != i64::MAX).then_some(spec.max),
					placeholder: spec.placeholder.map(str::to_string),
					advanced: spec.advanced,
				}
			})
			.collect()
	}

	/// Labels of the settings that differ from the running configuration and need a restart.
	fn restart_needed(&self, config: &Config) -> Vec<String> {
		let saved = serde_json::to_value(config).unwrap_or(Value::Null);
		let running = serde_json::to_value(&self.startup).unwrap_or(Value::Null);
		SETTINGS
			.iter()
			.filter(|spec| spec.effect == Effect::Restart)
			.filter(|spec| {
				let at = |value: &Value| spec.path.iter().fold(value, |value, key| &value[*key]).clone();
				at(&saved) != at(&running)
			})
			.map(|spec| spec.label.to_string())
			.collect()
	}

	/// Change the configuration file, then load it again and apply what can be applied now.
	async fn edit_config(
		&self,
		edit: impl FnOnce(&mut DocumentMut, &Config) -> Result<(), String>,
	) -> Result<Config, String> {
		let _guard = self.edit_lock.lock().await;
		let current = self.current();

		let text = std::fs::read_to_string(&self.config_path).unwrap_or_default();
		let mut document: DocumentMut = text
			.parse()
			.map_err(|e| format!("config.toml has a syntax error. Fix it in a text editor first: {e}"))?;
		edit(&mut document, &current)?;

		let text = document.to_string();
		toml::from_str::<Config>(&text).map_err(|e| format!("The change makes config.toml invalid: {e}"))?;
		write_atomically(&self.config_path, &text).map_err(|e| format!("Failed to save config.toml: {e}"))?;

		let config = Config::load_or_create(&self.config_path)
			.map_err(|()| "Saved config.toml, but failed to load it again. Check the logs.".to_string())?;
		*self.current.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();

		self.webserver.set_pairing_enabled(config.webserver.enable_pairing);
		self.session_manager
			.update_settings(SessionSettings {
				compositor: config.compositor.clone(),
				video: config.stream.video.clone(),
				audio: config.stream.audio.clone(),
				control: config.stream.control.clone(),
				stream_timeout: config.stream.timeout,
				inhibit_sleep: config.inhibit_sleep,
			})
			.await;
		Ok(config)
	}

	async fn reload_applications(&self, config: &Config) {
		self.application_manager
			.reload(
				config.applications.clone(),
				config.application_scanners.clone(),
				config.hidden_applications.clone(),
			)
			.await;
	}

	async fn save_settings(&self, values: BTreeMap<String, Value>) -> Result<String, String> {
		let mut edits: Vec<(&SettingSpec, Value)> = Vec::new();
		for (key, value) in values {
			let spec = SETTINGS
				.iter()
				.find(|spec| spec.key == key)
				.ok_or_else(|| format!("Unknown setting '{key}'."))?;
			edits.push((spec, validate_setting(spec, value)?));
		}

		let config = self
			.edit_config(|document, _| {
				for (spec, value) in edits {
					set_setting(document, spec, value);
				}
				Ok(())
			})
			.await?;

		let restart = self.restart_needed(&config);
		Ok(if restart.is_empty() {
			"Saved to config.toml.".to_string()
		} else {
			format!(
				"Saved to config.toml. Restart Moonshine to apply: {}.",
				restart.join(", ")
			)
		})
	}

	fn scanner_kinds(&self) -> Vec<ScannerKind> {
		SCANNER_KINDS
			.iter()
			.map(|kind| {
				let config = default_scanner(kind.kind);
				let default_path = config.as_ref().map(scanner_path).unwrap_or_default();
				let found = !default_path.is_empty() && default_path.lines().any(path_exists);
				ScannerKind {
					kind: kind.kind.to_string(),
					name: kind.name.to_string(),
					description: kind.description.to_string(),
					path_label: kind.path_label.to_string(),
					multiline: kind.kind == "desktop",
					default_path,
					found,
				}
			})
			.collect()
	}
}

fn default_device_name(fingerprint: &str) -> String {
	format!("Device {}", fingerprint.get(..4).unwrap_or(fingerprint).to_uppercase())
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
	let temporary = path.with_extension("toml.tmp");
	std::fs::write(&temporary, text)?;
	if let Ok(metadata) = std::fs::metadata(path) {
		let _ = std::fs::set_permissions(&temporary, metadata.permissions());
	}
	std::fs::rename(&temporary, path)
}

fn path_exists(path: &str) -> bool {
	shellexpand::full(path.trim()).is_ok_and(|path| Path::new(path.as_ref()).exists())
}

fn validate_setting(spec: &SettingSpec, value: Value) -> Result<Value, String> {
	let label = spec.label;
	match spec.kind {
		Kind::Bool => value
			.as_bool()
			.map(Value::Bool)
			.ok_or_else(|| format!("{label} must be on or off.")),
		Kind::Number | Kind::Range => {
			let number = value
				.as_i64()
				.or_else(|| value.as_f64().map(|n| n.round() as i64))
				.or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
				.ok_or_else(|| format!("{label} must be a whole number."))?;
			if number < spec.min || number > spec.max {
				return Err(format!("{label} must be between {} and {}.", spec.min, spec.max));
			}
			Ok(Value::from(number))
		},
		Kind::Text | Kind::OptionalText => {
			let text = value
				.as_str()
				.ok_or_else(|| format!("{label} must be text."))?
				.trim()
				.to_string();
			if spec.kind == Kind::Text && text.is_empty() {
				return Err(format!("{label} cannot be empty."));
			}
			if spec.key == "address" && text.parse::<IpAddr>().is_err() {
				return Err(format!("{label} must be an IP address, such as 0.0.0.0 or ::."));
			}
			Ok(Value::String(text))
		},
		Kind::Networks => {
			let ranges = value
				.as_array()
				.ok_or_else(|| format!("{label} must be a list."))?
				.iter()
				.map(|range| {
					let range = range.as_str().unwrap_or_default().trim();
					Network::parse(range).map(|_| Value::String(range.to_string()))
				})
				.collect::<Result<Vec<_>, _>>()?;
			if ranges.is_empty() {
				return Err(format!(
					"{label} needs at least one address range, such as 127.0.0.1/32."
				));
			}
			Ok(Value::Array(ranges))
		},
	}
}

/// The table at `path`, created when it does not exist yet.
fn table_at<'a>(document: &'a mut DocumentMut, path: &[&str]) -> &'a mut Table {
	let new_table = || {
		// An implicit table writes no header of its own when it only holds other tables.
		let mut table = Table::new();
		table.set_implicit(true);
		Item::Table(table)
	};
	let mut table = document.as_table_mut();
	for key in path {
		let item = table.entry(key).or_insert_with(new_table);
		if !item.is_table() {
			*item = new_table();
		}
		table = item.as_table_mut().expect("item was just made a table");
	}
	table
}

/// Set `key` in `table`, keeping the comments and spacing around an existing value.
fn set_value(table: &mut Table, key: &str, value: impl Into<toml_edit::Value>) {
	let mut value = value.into();
	if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
		*value.decor_mut() = existing.decor().clone();
		*existing = value;
	} else {
		table.insert(key, Item::Value(value));
	}
}

fn set_setting(document: &mut DocumentMut, spec: &SettingSpec, value: Value) {
	let (key, parents) = spec.path.split_last().expect("setting paths are not empty");
	let table = table_at(document, parents);
	match value {
		Value::Bool(value) => {
			set_value(table, key, value);
		},
		Value::Number(number) => {
			set_value(table, key, number.as_i64().unwrap_or_default());
		},
		Value::String(text) if text.is_empty() && spec.kind == Kind::OptionalText => {
			table.remove(key);
		},
		Value::String(text) => {
			set_value(table, key, text);
		},
		Value::Array(items) => {
			let array: toml_edit::Array = items.iter().filter_map(Value::as_str).collect();
			set_value(table, key, array);
		},
		_ => {},
	}
}

/// Serialize `value` into a TOML table for an array of tables. Nested tables become inline
/// tables, so the entry stays in one block wherever it lands in the file.
fn to_table(value: &impl serde::Serialize) -> Result<Table, String> {
	let text = toml::to_string(value).map_err(|e| format!("Failed to write the entry: {e}"))?;
	let document: DocumentMut = text.parse().map_err(|e| format!("Failed to write the entry: {e}"))?;
	let mut table = document.as_table().clone();
	table.set_position(None);
	let nested: Vec<String> = table
		.iter()
		.filter(|(_, item)| item.is_table())
		.map(|(key, _)| key.to_string())
		.collect();
	for key in nested {
		if let Some(Item::Table(inner)) = table.remove(&key) {
			let mut inline = inner.into_inline_table();
			inline.fmt();
			table.insert(&key, Item::Value(toml_edit::Value::InlineTable(inline)));
		}
	}
	Ok(table)
}

/// The array of tables under `key`. When the file does not have it, it is created from the
/// entries Moonshine uses now, which can be defaults.
fn array_of_tables<'a, T: serde::Serialize>(
	document: &'a mut DocumentMut,
	key: &str,
	current: &[T],
) -> Result<&'a mut ArrayOfTables, String> {
	if !document.as_table().get(key).is_some_and(Item::is_array_of_tables) {
		let mut array = ArrayOfTables::new();
		for entry in current {
			array.push(to_table(entry)?);
		}
		document.as_table_mut().insert(key, Item::ArrayOfTables(array));
	}
	Ok(document[key]
		.as_array_of_tables_mut()
		.expect("item was just made an array of tables"))
}

fn join_command(command: &[String]) -> String {
	shlex::try_join(command.iter().map(String::as_str)).unwrap_or_else(|_| command.join(" "))
}

fn split_command(line: &str, what: &str) -> Result<Vec<String>, String> {
	shlex::split(line).ok_or_else(|| format!("The {what} has an unclosed quote."))
}

fn command_lines(commands: &[Vec<String>]) -> String {
	commands
		.iter()
		.map(|command| join_command(command))
		.collect::<Vec<_>>()
		.join("\n")
}

fn parse_command_lines(text: &str, what: &str) -> Result<Vec<Vec<String>>, String> {
	text.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty())
		.map(|line| split_command(line, what))
		.collect()
}

fn env_lines(env: &BTreeMap<String, String>) -> String {
	env.iter()
		.map(|(name, value)| format!("{name}={value}"))
		.collect::<Vec<_>>()
		.join("\n")
}

fn parse_env(text: &str) -> Result<BTreeMap<String, String>, String> {
	text.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty())
		.map(|line| match line.split_once('=') {
			Some((name, value)) if !name.trim().is_empty() => Ok((name.trim().to_string(), value.to_string())),
			_ => Err(format!("'{line}' is not NAME=value.")),
		})
		.collect()
}

fn application_form(application: &ApplicationConfig) -> ApplicationForm {
	ApplicationForm {
		title: application.title.clone(),
		command: join_command(&application.command),
		boxart: application
			.boxart
			.as_ref()
			.map(|path| path.display().to_string())
			.unwrap_or_default(),
		pre_command: command_lines(&application.pre_command),
		post_command: command_lines(&application.post_command),
		env: env_lines(&application.env),
	}
}

fn apply_application_form(
	form: &ApplicationForm,
	mut application: ApplicationConfig,
) -> Result<ApplicationConfig, String> {
	application.title = form.title.trim().to_string();
	if application.title.is_empty() {
		return Err("Type a name for the application.".to_string());
	}
	application.command = split_command(&form.command, "command")?;
	if application.command.is_empty() {
		return Err("Type the command that starts the application.".to_string());
	}
	let boxart = form.boxart.trim();
	application.boxart = (!boxart.is_empty()).then(|| PathBuf::from(boxart));
	application.pre_command = parse_command_lines(&form.pre_command, "before-launch command")?;
	application.post_command = parse_command_lines(&form.post_command, "after-exit command")?;
	application.env = parse_env(&form.env)?;
	Ok(application)
}

struct ScannerKindSpec {
	kind: &'static str,
	name: &'static str,
	description: &'static str,
	path_label: &'static str,
	/// The minimal configuration of a new scanner; serde fills in the rest.
	default: &'static str,
}

const SCANNER_KINDS: &[ScannerKindSpec] = &[
	ScannerKindSpec {
		kind: "steam",
		name: "Steam",
		description: "Installed games in your Steam libraries.",
		path_label: "Steam folder",
		default: r#"{"type":"steam","library":"$HOME/.local/share/Steam","command":["/usr/bin/steam","-bigpicture","steam://rungameid/{game_id}"]}"#,
	},
	ScannerKindSpec {
		kind: "desktop",
		name: "Desktop entries",
		description: "Apps from .desktop launchers, like your app menu.",
		path_label: "Folders",
		default: r#"{"type":"desktop","directories":["$HOME/.local/share/applications","/usr/share/applications"]}"#,
	},
	ScannerKindSpec {
		kind: "lutris",
		name: "Lutris",
		description: "Games in your Lutris library.",
		path_label: "Lutris database",
		default: r#"{"type":"lutris","command":["/usr/bin/lutris","lutris:rungame/{slug}"]}"#,
	},
	ScannerKindSpec {
		kind: "heroic",
		name: "Heroic",
		description: "Epic, GOG and Amazon games in Heroic.",
		path_label: "Heroic config folder",
		default: r#"{"type":"heroic","command":["heroic","--no-gui","heroic://launch?appName={app_name}&runner={runner}"]}"#,
	},
	ScannerKindSpec {
		kind: "bottles",
		name: "Bottles",
		description: "Programs in your bottles.",
		path_label: "Bottles data folder",
		default: r#"{"type":"bottles","command":["bottles-cli","run","-b","{bottle}","--program-id","{program_id}"],"umu_command":["bottles-cli","umu","run","--game","{umu_game}"]}"#,
	},
];

fn default_scanner(kind: &str) -> Option<ApplicationScannerConfig> {
	let spec = SCANNER_KINDS.iter().find(|spec| spec.kind == kind)?;
	serde_json::from_str(spec.default)
		.map_err(|e| tracing::warn!("Invalid default for the {kind} scanner: {e}"))
		.ok()
}

fn scanner_kind(config: &ApplicationScannerConfig) -> &'static str {
	match config {
		ApplicationScannerConfig::Steam(_) => "steam",
		ApplicationScannerConfig::Desktop(_) => "desktop",
		ApplicationScannerConfig::Lutris(_) => "lutris",
		ApplicationScannerConfig::Heroic(_) => "heroic",
		ApplicationScannerConfig::Bottles(_) => "bottles",
	}
}

/// The JSON key of the path a scanner reads.
fn scanner_path_key(kind: &str) -> &'static str {
	match kind {
		"steam" => "library",
		"desktop" => "directories",
		"lutris" => "pga_db",
		"heroic" => "config_dir",
		_ => "data_dir",
	}
}

fn scanner_path(config: &ApplicationScannerConfig) -> String {
	match config {
		ApplicationScannerConfig::Steam(config) => config.library.display().to_string(),
		ApplicationScannerConfig::Desktop(config) => config
			.directories
			.iter()
			.map(|directory| directory.display().to_string())
			.collect::<Vec<_>>()
			.join("\n"),
		ApplicationScannerConfig::Lutris(config) => config.pga_db.display().to_string(),
		ApplicationScannerConfig::Heroic(config) => config.config_dir.display().to_string(),
		ApplicationScannerConfig::Bottles(config) => config.data_dir.display().to_string(),
	}
}

fn scanner_form(config: &ApplicationScannerConfig) -> ScannerForm {
	let (pre, post, env) = match config {
		ApplicationScannerConfig::Steam(c) => (&c.pre_command, &c.post_command, &c.env),
		ApplicationScannerConfig::Desktop(c) => (&c.pre_command, &c.post_command, &c.env),
		ApplicationScannerConfig::Lutris(c) => (&c.pre_command, &c.post_command, &c.env),
		ApplicationScannerConfig::Heroic(c) => (&c.pre_command, &c.post_command, &c.env),
		ApplicationScannerConfig::Bottles(c) => (&c.pre_command, &c.post_command, &c.env),
	};
	ScannerForm {
		kind: scanner_kind(config).to_string(),
		path: scanner_path(config),
		include_terminal: matches!(config, ApplicationScannerConfig::Desktop(c) if c.include_terminal),
		pre_command: command_lines(pre),
		post_command: command_lines(post),
		env: env_lines(env),
	}
}

fn apply_scanner_form(form: &ScannerForm, base: ApplicationScannerConfig) -> Result<ApplicationScannerConfig, String> {
	let kind = scanner_kind(&base);
	let paths: Vec<String> = form
		.path
		.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty())
		.map(str::to_string)
		.collect();
	if paths.is_empty() {
		return Err("Type the folder or file that the scanner reads.".to_string());
	}
	if let Some(missing) = paths.iter().find(|path| !path_exists(path)) {
		return Err(format!(
			"Moonshine cannot find {missing}. Install the launcher, or type the place where it keeps its data."
		));
	}

	let mut value = serde_json::to_value(&base).map_err(|e| format!("Failed to edit the scanner: {e}"))?;
	value[scanner_path_key(kind)] = if kind == "desktop" {
		Value::from(paths)
	} else {
		Value::from(paths[0].clone())
	};
	if kind == "desktop" {
		value["include_terminal"] = Value::Bool(form.include_terminal);
	}
	value["pre_command"] = serde_json::to_value(parse_command_lines(&form.pre_command, "before-launch command")?)
		.map_err(|e| e.to_string())?;
	value["post_command"] = serde_json::to_value(parse_command_lines(&form.post_command, "after-exit command")?)
		.map_err(|e| e.to_string())?;
	value["env"] = serde_json::to_value(parse_env(&form.env)?).map_err(|e| e.to_string())?;
	serde_json::from_value(value).map_err(|e| format!("The scanner settings are not valid: {e}"))
}

impl Backend for UiBackend {
	async fn snapshot(&self) -> Snapshot {
		let config = self.current();
		let session = self.session_info().await;
		let streaming_client = session.as_ref().and_then(|(_, client)| client.clone());

		let devices = self
			.client_manager
			.paired_devices()
			.unwrap_or_default()
			.into_iter()
			.map(|device| DeviceInfo {
				streaming: streaming_client.as_deref() == Some(device.fingerprint.as_str()),
				name: if device.name.is_empty() {
					default_device_name(&device.fingerprint)
				} else {
					device.name
				},
				paired_at: device.paired_at.map(unix),
				last_seen: device.last_seen.map(unix),
				fingerprint: device.fingerprint,
			})
			.collect();

		let catalog = self.application_manager.catalog();
		let scanners = config
			.application_scanners
			.iter()
			.enumerate()
			.map(|(index, scanner)| ScannerInfo {
				index,
				form: scanner_form(scanner),
				applications: catalog
					.iter()
					.filter(|entry| entry.scanner == Some(index))
					.map(|entry| ScannedApplication {
						title: entry.application.title.clone(),
						hidden: entry.hidden,
					})
					.collect(),
			})
			.collect();

		Snapshot {
			server: ServerInfo {
				name: self.startup.name.clone(),
				version: env!("CARGO_PKG_VERSION").to_string(),
				gpu: self.gpu_name.clone(),
				codecs: codec_names(self.supported_codecs),
				hdr: self.hdr_supported,
				address: self.startup.address.clone(),
				config_path: self.config_path.display().to_string(),
				started: self.started,
			},
			session: session.map(|(info, _)| info),
			health: self.health.lock().unwrap_or_else(PoisonError::into_inner).clone(),
			pairing: PairingInfo {
				enabled: self.webserver.pairing_enabled(),
				requests: self
					.client_manager
					.pairing_requests()
					.into_iter()
					.map(|request| PairingRequestInfo {
						id: request.id,
						since: unix(request.since),
					})
					.collect(),
			},
			devices,
			applications: config
				.applications
				.iter()
				.enumerate()
				.map(|(index, application)| ApplicationInfo {
					index,
					form: application_form(application),
				})
				.collect(),
			scanners,
			scanner_kinds: self.scanner_kinds(),
			scanned_at: self.application_manager.scanned_at().map(unix),
			settings: self.settings(&config),
			restart_needed: self.restart_needed(&config),
		}
	}

	async fn perform(&self, action: Action) -> Result<String, String> {
		match action {
			Action::StopSession => {
				self.session_manager
					.stop_session()
					.await
					.map_err(|()| "Failed to stop the stream. Check the logs.".to_string())?;
				Ok("Stream stopped.".to_string())
			},
			Action::SubmitPin { id, pin } => {
				if pin.len() != 4 || !pin.chars().all(|c| c.is_ascii_digit()) {
					return Err("Type the 4 digits that Moonlight shows.".to_string());
				}
				self.client_manager.submit_pin(&id, &pin).map_err(|()| {
					"This pairing request is no longer waiting. Start pairing again in Moonlight.".to_string()
				})?;
				Ok("PIN sent. Moonlight finishes pairing. If the PIN was wrong, Moonlight shows an error.".to_string())
			},
			Action::SetPairing { enabled } => {
				self.edit_config(|document, _| {
					set_value(table_at(document, &["webserver"]), "enable_pairing", enabled);
					Ok(())
				})
				.await?;
				Ok(if enabled {
					"New devices can pair.".to_string()
				} else {
					"Pairing is off.".to_string()
				})
			},
			Action::RenameDevice { fingerprint, name } => {
				match self.client_manager.rename_device(&fingerprint, &name) {
					Ok(true) => Ok("Saved.".to_string()),
					Ok(false) => Err("This device is no longer paired.".to_string()),
					Err(()) => Err("Failed to save the name. Check the logs.".to_string()),
				}
			},
			Action::UnpairDevice { fingerprint } => {
				let streaming = self
					.session_manager
					.status()
					.await
					.is_some_and(|status| status.client.as_deref() == Some(fingerprint.as_str()));
				match self.client_manager.unpair_device(&fingerprint) {
					Ok(true) => {
						if streaming {
							let _ = self.session_manager.stop_session().await;
						}
						Ok("The device is unpaired.".to_string())
					},
					Ok(false) => Err("This device is no longer paired.".to_string()),
					Err(()) => Err("Failed to unpair the device. Check the logs.".to_string()),
				}
			},
			Action::SaveApplication { index, form } => {
				let config = self
					.edit_config(|document, current| {
						let base = index
							.and_then(|index| current.applications.get(index).cloned())
							.unwrap_or_default();
						let application = apply_application_form(&form, base)?;
						if current
							.applications
							.iter()
							.enumerate()
							.any(|(i, other)| Some(i) != index && other.title.eq_ignore_ascii_case(&application.title))
						{
							return Err(format!("An application named {} already exists.", application.title));
						}
						let table = to_table(&application)?;
						let array = array_of_tables(document, "application", &current.applications)?;
						match index {
							Some(index) => {
								let entry = array
									.get_mut(index)
									.ok_or_else(|| "This application no longer exists.".to_string())?;
								*entry = table;
							},
							None => array.push(table),
						}
						Ok(())
					})
					.await?;
				self.reload_applications(&config).await;
				Ok(format!(
					"Saved {}. Clients see it at their next refresh.",
					form.title.trim()
				))
			},
			Action::RemoveApplication { index } => {
				let config = self
					.edit_config(|document, current| {
						let array = array_of_tables(document, "application", &current.applications)?;
						if index >= array.len() {
							return Err("This application no longer exists.".to_string());
						}
						array.remove(index);
						Ok(())
					})
					.await?;
				self.reload_applications(&config).await;
				Ok("Removed from config.toml.".to_string())
			},
			Action::SaveScanner { index, form } => {
				let config = self
					.edit_config(|document, current| {
						let base = match index {
							Some(index) => current
								.application_scanners
								.get(index)
								.cloned()
								.ok_or_else(|| "This scanner no longer exists.".to_string())?,
							None => default_scanner(&form.kind)
								.ok_or_else(|| format!("Unknown scanner '{}'.", form.kind))?,
						};
						let scanner = apply_scanner_form(&form, base)?;
						let table = to_table(&scanner)?;
						let array = array_of_tables(document, "application_scanner", &current.application_scanners)?;
						match index {
							Some(index) => {
								*array
									.get_mut(index)
									.ok_or_else(|| "This scanner no longer exists.".to_string())? = table;
							},
							None => array.push(table),
						}
						Ok(())
					})
					.await?;
				self.reload_applications(&config).await;
				Ok("Saved. The scanner looked for applications again.".to_string())
			},
			Action::RemoveScanner { index } => {
				let config = self
					.edit_config(|document, current| {
						let array = array_of_tables(document, "application_scanner", &current.application_scanners)?;
						if index >= array.len() {
							return Err("This scanner no longer exists.".to_string());
						}
						array.remove(index);
						Ok(())
					})
					.await?;
				self.reload_applications(&config).await;
				Ok("The scanner is removed. Its applications are no longer listed.".to_string())
			},
			Action::SetHidden { title, hidden } => {
				let config = self
					.edit_config(|document, current| {
						let mut titles = current.hidden_applications.clone();
						titles.retain(|other| *other != title);
						if hidden {
							titles.push(title.clone());
						}
						titles.sort();
						if titles.is_empty() {
							document.as_table_mut().remove("hidden_applications");
						} else {
							let array: toml_edit::Array = titles.iter().map(String::as_str).collect();
							set_value(document.as_table_mut(), "hidden_applications", array);
						}
						Ok(())
					})
					.await?;
				self.application_manager.set_hidden(config.hidden_applications.clone());
				Ok(if hidden {
					format!("{title} is hidden. Clients stop showing it at their next refresh.")
				} else {
					format!("{title} is visible again.")
				})
			},
			Action::SaveSettings { values } => self.save_settings(values).await,
			Action::RunHealthCheck => {
				{
					let mut health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
					if health.running {
						return Ok("The checks are already running.".to_string());
					}
					health.running = true;
				}
				let health = self.health.clone();
				let config = self.current();
				tokio::spawn(async move {
					let report = tokio::task::spawn_blocking(move || healthcheck::run_healthcheck(Some(&config))).await;
					let mut health = health.lock().unwrap_or_else(PoisonError::into_inner);
					match report {
						Ok(report) => *health = health_info(&report),
						Err(e) => {
							tracing::warn!("Failed to run the health checks: {e}");
							health.running = false;
						},
					}
				});
				Ok("The checks run. This takes a few seconds.".to_string())
			},
			Action::Restart => {
				tracing::info!("Restart requested from the control panel.");
				self.restart_requested.store(true, Ordering::SeqCst);
				// Answer the request before the control panel stops.
				let shutdown = self.shutdown.clone();
				tokio::spawn(async move {
					tokio::time::sleep(std::time::Duration::from_millis(200)).await;
					let _ = shutdown.trigger_shutdown(ShutdownReason::AppQuit);
				});
				Ok("Moonshine restarts.".to_string())
			},
		}
	}

	fn stats(&self) -> (Option<StreamStart>, broadcast::Receiver<StatsEvent>) {
		let rx = self.stats_tx.subscribe();
		let current = self.stream.lock().unwrap_or_else(PoisonError::into_inner).clone();
		(current, rx)
	}

	async fn report(&self) -> String {
		use std::fmt::Write;

		let snapshot = self.snapshot().await;
		let mut report = String::new();
		let _ = writeln!(report, "Moonshine {}", snapshot.server.version);
		let _ = writeln!(report, "GPU: {}", snapshot.server.gpu);
		let _ = writeln!(report, "Codecs: {}", snapshot.server.codecs.join(", "));
		let _ = writeln!(report, "HDR: {}", snapshot.server.hdr);
		if let Ok(os) = std::fs::read_to_string("/etc/os-release")
			&& let Some(name) = os.lines().find_map(|line| line.strip_prefix("PRETTY_NAME="))
		{
			let _ = writeln!(report, "OS: {}", name.trim_matches('"'));
		}
		if let Ok(kernel) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
			let _ = writeln!(report, "Kernel: {}", kernel.trim());
		}

		let _ = writeln!(report, "\n## Health checks");
		for check in &snapshot.health.checks {
			let _ = writeln!(report, "[{}] {}: {}", check.status, check.name, check.message);
		}

		let _ = writeln!(report, "\n## Session");
		match &snapshot.session {
			Some(session) => {
				let _ = writeln!(
					report,
					"{} {} at {}x{}@{} (hdr: {})",
					session.phase, session.application, session.width, session.height, session.fps, session.hdr
				);
			},
			None => {
				let _ = writeln!(report, "None");
			},
		}

		let _ = writeln!(report, "\n## config.toml");
		let _ = writeln!(
			report,
			"{}",
			std::fs::read_to_string(&self.config_path).unwrap_or_else(|e| format!("Failed to read: {e}"))
		);

		let _ = writeln!(report, "\n## Log");
		for line in self.logs.tail(2000) {
			let _ = writeln!(
				report,
				"{} {:5} {}: {}",
				line.time, line.level, line.target, line.message
			);
		}
		report
	}

	async fn refresh(&self) {
		self.application_manager.refresh().await;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_scanners_are_valid() {
		for spec in SCANNER_KINDS {
			let scanner = default_scanner(spec.kind).expect("default scanner parses");
			assert_eq!(scanner_kind(&scanner), spec.kind);
			assert!(!scanner_path(&scanner).is_empty());
		}
	}

	#[test]
	fn settings_edit_the_document_in_place() {
		let mut document: DocumentMut = "# My server\nname = \"old\" # keep\n\n[stream.video]\nport = 47998\n"
			.parse()
			.unwrap();
		let name = SETTINGS.iter().find(|spec| spec.key == "name").unwrap();
		let fec = SETTINGS.iter().find(|spec| spec.key == "fec_percentage").unwrap();
		let gpu = SETTINGS.iter().find(|spec| spec.key == "gpu").unwrap();
		set_setting(&mut document, name, Value::from("gaming-pc"));
		set_setting(&mut document, fec, Value::from(30));
		set_setting(&mut document, gpu, Value::from(""));

		let text = document.to_string();
		assert!(text.starts_with("# My server\n"));
		assert!(text.contains("name = \"gaming-pc\""));
		assert!(text.contains("fec_percentage = 30"));
		assert!(text.contains("port = 47998"));
		assert!(!text.contains("gpu"));
	}

	#[test]
	fn applications_with_nested_tables_stay_valid() {
		let mut document: DocumentMut =
			"[[application]]\ntitle = \"Desktop\"\ncommand = [\"true\"]\n\n[stream]\nport = 1\n"
				.parse()
				.unwrap();
		let application = ApplicationConfig {
			title: "Kodi".to_string(),
			command: vec!["kodi".to_string()],
			env: BTreeMap::from([("MANGOHUD".to_string(), "1".to_string())]),
			..Default::default()
		};
		let table = to_table(&application).unwrap();
		array_of_tables(&mut document, "application", &Vec::<ApplicationConfig>::new())
			.unwrap()
			.push(table);
		let hold = SETTINGS.iter().find(|spec| spec.key == "hold_ms").unwrap();
		set_setting(&mut document, hold, Value::from(500));

		let text = document.to_string();
		let config: Config = toml::from_str(&text).unwrap();
		assert_eq!(config.applications.len(), 2);
		assert_eq!(config.applications[1].env["MANGOHUD"], "1");
		assert_eq!(config.stream.control.gamepad.home_button.hold_ms, 500);
		assert!(!text.contains("[stream.control.gamepad]\n"), "{text}");
	}

	#[test]
	fn settings_are_validated() {
		let fec = SETTINGS.iter().find(|spec| spec.key == "fec_percentage").unwrap();
		assert!(validate_setting(fec, Value::from(101)).is_err());
		assert_eq!(validate_setting(fec, Value::from(25)).unwrap(), Value::from(25));

		let allowed = SETTINGS.iter().find(|spec| spec.key == "ui_allowed_networks").unwrap();
		assert!(validate_setting(allowed, serde_json::json!(["192.168.1.0/24"])).is_ok());
		assert!(validate_setting(allowed, serde_json::json!(["nope"])).is_err());
		assert!(validate_setting(allowed, serde_json::json!([])).is_err());
	}

	#[test]
	fn every_setting_exists_in_the_config() {
		let values = serde_json::to_value(Config::default()).unwrap();
		for spec in SETTINGS {
			if spec.kind == Kind::OptionalText {
				continue;
			}
			let value = spec.path.iter().fold(&values, |value, key| &value[*key]);
			assert!(!value.is_null(), "{} has no value at {:?}", spec.key, spec.path);
		}
	}

	#[test]
	fn application_forms_round_trip() {
		let application = ApplicationConfig {
			title: "Steam".to_string(),
			command: vec!["/usr/bin/steam".to_string(), "steam://open/big picture".to_string()],
			pre_command: vec![vec!["echo".to_string(), "hi there".to_string()]],
			env: BTreeMap::from([("MANGOHUD".to_string(), "1".to_string())]),
			..Default::default()
		};
		let form = application_form(&application);
		let parsed = apply_application_form(&form, ApplicationConfig::default()).unwrap();
		assert_eq!(parsed.command, application.command);
		assert_eq!(parsed.pre_command, application.pre_command);
		assert_eq!(parsed.env, application.env);
	}
}
