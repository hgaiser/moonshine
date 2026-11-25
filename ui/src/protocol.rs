//! The data the control panel exchanges with the browser and with its [`Backend`](crate::Backend).
//!
//! Times are in seconds since the Unix epoch unless a name says otherwise.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Everything the control panel shows, except statistics and logs.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Snapshot {
	pub server: ServerInfo,
	pub session: Option<SessionInfo>,
	pub health: HealthInfo,
	pub pairing: PairingInfo,
	pub devices: Vec<DeviceInfo>,
	pub applications: Vec<ApplicationInfo>,
	/// What a client can stream: the configured and the scanned applications, as Moonlight sees them.
	pub library: Vec<LibraryApplication>,
	pub scanners: Vec<ScannerInfo>,
	/// The kinds of scanner that can be added.
	pub scanner_kinds: Vec<ScannerKind>,
	/// When the applications were last scanned.
	pub scanned_at: Option<u64>,
	pub settings: Vec<Setting>,
	/// Labels of saved settings that take effect after a restart.
	pub restart_needed: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ServerInfo {
	pub name: String,
	pub version: String,
	pub gpu: String,
	pub codecs: Vec<String>,
	pub hdr: bool,
	/// Whether the control panel can stream to the browser (`[ui] streaming`).
	pub browser_streaming: bool,
	pub address: String,
	pub config_path: String,
	/// When Moonshine started.
	pub started: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionInfo {
	/// `starting`, `waiting` (for the client to start the streams) or `streaming`.
	pub phase: String,
	pub application: String,
	/// Name of the device that streams, if it is known.
	pub device: Option<String>,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub hdr: bool,
	pub audio_channels: u32,
	/// The negotiated video stream, once it runs.
	pub stream: Option<StreamStart>,
	pub started: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HealthInfo {
	/// When the checks last ran.
	pub checked_at: u64,
	/// Whether the checks run now.
	pub running: bool,
	pub checks: Vec<HealthCheck>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HealthCheck {
	pub name: String,
	/// `ok`, `warn` or `fail`.
	pub status: String,
	pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PairingInfo {
	pub enabled: bool,
	pub requests: Vec<PairingRequestInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PairingRequestInfo {
	pub id: String,
	pub since: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeviceInfo {
	pub fingerprint: String,
	/// Empty until the user names the device.
	pub name: String,
	pub paired_at: Option<u64>,
	pub last_seen: Option<u64>,
	pub streaming: bool,
}

/// An application that a client can stream.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LibraryApplication {
	/// The ID that Moonlight uses.
	pub id: i32,
	pub title: String,
}

/// An application from the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ApplicationInfo {
	/// Position in the configuration file.
	pub index: usize,
	pub form: ApplicationForm,
}

/// An application as the form edits it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApplicationForm {
	pub title: String,
	/// The command line, with shell quoting.
	pub command: String,
	/// Path to a box art image, or empty.
	pub boxart: String,
	/// The folder the application starts in, or empty for the home folder.
	#[serde(default)]
	pub working_directory: String,
	/// Whether Steam chooses which window is shown, for Steam itself.
	#[serde(default)]
	pub steam_integration: bool,
	/// Commands to run before launch, one for each line.
	pub pre_command: String,
	/// Commands to run after exit, one for each line.
	pub post_command: String,
	/// Environment variables, one `NAME=value` for each line.
	pub env: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScannerInfo {
	/// Position in the configuration file.
	pub index: usize,
	pub form: ScannerForm,
	pub applications: Vec<ScannedApplication>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScannedApplication {
	pub title: String,
	pub hidden: bool,
}

/// A scanner as the form edits it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerForm {
	/// `steam`, `desktop`, `lutris`, `heroic`, `bottles` or `faugus`.
	pub kind: String,
	/// What the scanner reads: one path, or one folder for each line for desktop entries.
	pub path: String,
	/// Desktop entries only: whether terminal applications are included.
	#[serde(default)]
	pub include_terminal: bool,
	/// Faugus only: whether games hidden in its library are included.
	#[serde(default)]
	pub include_hidden: bool,
	pub pre_command: String,
	pub post_command: String,
	pub env: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScannerKind {
	pub kind: String,
	pub name: String,
	pub description: String,
	/// Label of the path field.
	pub path_label: String,
	/// Whether the path field holds one folder for each line.
	pub multiline: bool,
	/// The path a new scanner of this kind starts with.
	pub default_path: String,
	/// Whether the default path exists on this computer.
	pub found: bool,
}

/// One setting of the settings page.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Setting {
	pub key: String,
	pub group: String,
	pub label: String,
	pub help: String,
	/// `bool`, `text`, `number`, `range`, `list` or `choice`.
	pub kind: String,
	pub value: serde_json::Value,
	/// The values a `choice` can take, in order.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub options: Vec<SettingOption>,
	/// `now`, `next` (stream) or `restart`.
	pub effect: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub unit: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub min: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub placeholder: Option<String>,
	/// Whether the setting is in a collapsed group.
	pub advanced: bool,
	/// Shown, but only config.toml can change it.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub read_only: bool,
}

/// One value of a `choice` setting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SettingOption {
	/// The value written to the configuration.
	pub value: String,
	/// What the page shows.
	pub label: String,
}

/// Something the user asks the control panel to do.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
	StopSession,
	SubmitPin {
		id: String,
		pin: String,
	},
	SetPairing {
		enabled: bool,
	},
	RenameDevice {
		fingerprint: String,
		name: String,
	},
	UnpairDevice {
		fingerprint: String,
	},
	SaveApplication {
		index: Option<usize>,
		form: ApplicationForm,
	},
	RemoveApplication {
		index: usize,
	},
	SaveScanner {
		index: Option<usize>,
		form: ScannerForm,
	},
	RemoveScanner {
		index: usize,
	},
	SetHidden {
		title: String,
		hidden: bool,
	},
	SaveSettings {
		values: BTreeMap<String, serde_json::Value>,
	},
	RunHealthCheck,
	Restart,
}

/// The video stream as negotiated with the client.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StreamStart {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub bitrate_kbps: u64,
	pub codec: String,
	pub hdr: bool,
}

/// The timings of one encoded frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSample {
	pub wait_us: u32,
	pub import_us: u32,
	pub convert_us: u32,
	pub encode_us: u32,
	pub packetize_us: u32,
	pub send_us: u32,
	pub total_us: u32,
	pub bytes: u32,
	pub key_frame: bool,
}

/// What the backend reports about the video stream.
#[derive(Clone, Debug)]
pub enum StatsEvent {
	Started(StreamStart),
	Frame(FrameSample),
	Stopped,
}
