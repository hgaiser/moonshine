use serde::{Deserialize, Serialize};
use std::{
	collections::hash_map::DefaultHasher,
	hash::{Hash, Hasher},
	path::{Path, PathBuf},
};

fn default_true() -> bool {
	true
}

fn default_false() -> bool {
	false
}

fn default_launch_timeout() -> u64 {
	2
}

fn default_stream_use_ipv6() -> StreamUseIpv6 {
	StreamUseIpv6::No
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamUseIpv6 {
	/// Always advertise an IPv4 session URL to clients.
	No,

	/// Always advertise an IPv6 session URL to clients.
	Yes,

	/// Advertise the same address family the client used for the launch/resume web request.
	/// If Moonlight launches over IPv4, it receives an IPv4 RTSP URL; if it launches
	/// over IPv6, it receives an IPv6 RTSP URL.
	Auto,
}

impl Default for StreamUseIpv6 {
	fn default() -> Self {
		Self::No
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
	/// Name of the Moonshine host.
	pub name: String,

	/// Address to bind to.
	pub address: String,

	/// Whether to advertise IPv6 session URLs to clients.
	#[serde(default = "default_stream_use_ipv6")]
	pub stream_use_ipv6: StreamUseIpv6,

	/// Configuration for the webserver.
	pub webserver: WebserverConfig,

	/// Configuration for the streams.
	pub stream: StreamConfig,

	/// List of applications to expose to clients.
	#[serde(rename = "application")]
	pub applications: Vec<ApplicationConfig>,

	/// List of scanners that dynamically adds applications when started.
	#[serde(rename = "application_scanner")]
	#[serde(skip_serializing_if = "Vec::is_empty", default)]
	pub application_scanners: Vec<ApplicationScannerConfig>,

	/// Configuration for the compositor.
	pub compositor: CompositorConfig,

	/// Time in seconds since last ping after which the stream closes.
	pub(crate) stream_timeout: u64,
}

impl Config {
	#[allow(clippy::result_unit_err)]
	pub(crate) fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
		let config =
			std::fs::read_to_string(file).map_err(|e| tracing::warn!("Failed to open configuration file: {e}"))?;
		let config: Config =
			toml::from_str(&config).map_err(|e| tracing::warn!("Failed to parse configuration file: {e}"))?;

		Ok(config)
	}

	pub fn load_or_create(path: &PathBuf) -> Result<Config, ()> {
		let mut config = if path.exists() {
			Self::read_from_file(path)?
		} else {
			tracing::info!(
				"No config file found at {}, creating a default config file.",
				path.display()
			);
			let config = Self::default();

			let serialized =
				toml::to_string_pretty(&config).map_err(|e| tracing::error!("Failed to serialize config: {e}"))?;

			let dir = path
				.parent()
				.ok_or_else(|| tracing::error!("Failed to get parent directory of config file."))?;
			std::fs::create_dir_all(dir).map_err(|e| tracing::error!("Failed to create config directory: {e}"))?;
			std::fs::write(path, serialized).map_err(|e| tracing::error!("Failed to save config file: {e}"))?;

			config
		};

		config.resolve_paths()?;
		Ok(config)
	}

	fn resolve_paths(&mut self) -> Result<(), ()> {
		let cert_path = self.webserver.certificate.to_string_lossy().to_string();
		let cert_path =
			shellexpand::full(&cert_path).map_err(|e| tracing::warn!("Failed to expand certificate path: {e}"))?;
		self.webserver.certificate = cert_path.to_string().into();

		let private_key_path = self.webserver.private_key.to_string_lossy().to_string();
		let private_key_path = shellexpand::full(&private_key_path)
			.map_err(|e| tracing::warn!("Failed to expand private key path: {e}"))?;
		self.webserver.private_key = private_key_path.to_string().into();

		Ok(())
	}
}

impl Default for Config {
	fn default() -> Self {
		Self {
			name: "Moonshine".to_string(),
			// Bind dual-stack by default so clients can reach us over IPv4 or IPv6.
			// The webserver disables IPV6_V6ONLY, so this single address covers both.
			address: "::".to_string(),
			stream_use_ipv6: StreamUseIpv6::default(),
			webserver: Default::default(),
			stream: Default::default(),
			applications: vec![ApplicationConfig {
				title: "Steam".to_string(),
				command: vec!["/usr/bin/steam".to_string(), "steam://open/bigpicture".to_string()],
				boxart: None,
				..Default::default()
			}],
			application_scanners: vec![ApplicationScannerConfig::Steam(SteamApplicationScannerConfig {
				library: "$HOME/.local/share/Steam".into(),
				command: vec![
					"/usr/bin/steam".to_string(),
					"-bigpicture".to_string(),
					"steam://rungameid/{game_id}".to_string(),
				],
				pre_command: Vec::new(),
				post_command: Vec::new(),
				stdout: None,
				stderr: None,
				launch_timeout_secs: 2,
			})],
			compositor: CompositorConfig::default(),
			stream_timeout: 60,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebserverConfig {
	/// Port of the webserver.
	pub port: u16,

	/// Port of the HTTPS webserver.
	pub port_https: u16,

	/// Whether to allow new clients to pair.
	#[serde(default = "default_true")]
	pub enable_pairing: bool,

	/// Path to the certificate for SSL encryption.
	pub certificate: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

impl Default for WebserverConfig {
	fn default() -> Self {
		Self {
			port: 47989,
			port_https: 47984,
			enable_pairing: true,
			certificate: "$HOME/.config/moonshine/cert.pem".into(),
			private_key: "$HOME/.config/moonshine/key.pem".into(),
		}
	}
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ApplicationConfig {
	/// Title of the application.
	pub title: String,

	/// Path to a boxart image.
	pub(crate) boxart: Option<PathBuf>,

	/// The command to run.
	pub command: Vec<String>,

	/// Commands to run before launching the application.
	/// Each inner Vec is a separate command; they execute in order.
	/// Runs synchronously — the application launch waits for all to finish.
	/// Useful for killing conflicting processes, setting GPU power states, etc.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub(crate) pre_command: Vec<Vec<String>>,

	/// Commands to run after the streaming session ends.
	/// Each inner Vec is a separate command; they execute in order.
	/// Runs synchronously — the server waits for all to finish before accepting new connections.
	/// Useful for restoring system state (e.g. GPU power management).
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub(crate) post_command: Vec<Vec<String>>,

	/// Path to redirect application stdout to. If not set, stdout is discarded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub(crate) stdout: Option<PathBuf>,

	/// Path to redirect application stderr to. If not set, stderr is discarded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub(crate) stderr: Option<PathBuf>,

	/// Seconds to wait for the application to reach an active state after launch.
	#[serde(default = "default_launch_timeout")]
	pub(crate) launch_timeout_secs: u64,
}

impl ApplicationConfig {
	pub(crate) fn id(&self) -> i32 {
		let mut hasher = DefaultHasher::new();
		self.title.hash(&mut hasher);
		hasher.finish() as i32
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ApplicationScannerConfig {
	/// Scans a 'libraryfolders.vdf' file from a Steam library directory.
	Steam(SteamApplicationScannerConfig),

	/// Scans directories containing freedesktop .desktop launchers.
	Desktop(DesktopApplicationScannerConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteamApplicationScannerConfig {
	/// Path to a Steam library (ie. `~/.local/share/Steam`).
	pub library: PathBuf,

	/// The command to run.
	pub command: Vec<String>,

	/// Commands to run before launching each scanned application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after each scanned application's session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// Path to redirect application stdout to. If not set, stdout is discarded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<PathBuf>,

	/// Path to redirect application stderr to. If not set, stderr is discarded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<PathBuf>,

	/// Seconds to wait for each scanned application to reach an active state after launch.
	#[serde(default = "default_launch_timeout")]
	pub launch_timeout_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopApplicationScannerConfig {
	/// Directories to scan recursively for `.desktop` files.
	pub directories: Vec<PathBuf>,

	/// Whether terminal-based entries should be included.
	#[serde(default = "default_false")]
	pub include_terminal: bool,

	/// Whether to resolve desktop entry icons into Moonshine boxart paths.
	#[serde(default = "default_true")]
	pub resolve_icons: bool,

	/// Commands to run before launching each scanned application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after each scanned application's session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// Path to redirect application stdout to. If not set, stdout is discarded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<PathBuf>,

	/// Path to redirect application stderr to. If not set, stderr is discarded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<PathBuf>,

	/// Seconds to wait for each scanned application to reach an active state after launch.
	#[serde(default = "default_launch_timeout")]
	pub launch_timeout_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamConfig {
	/// Port to bind the RTSP server to.
	pub(crate) port: u16,

	/// Configuration for the video stream.
	pub(crate) video: VideoStreamConfig,

	/// Configuration for the audio stream.
	pub(crate) audio: AudioStreamConfig,

	/// Configuration for the control stream.
	pub(crate) control: ControlStreamConfig,
}

impl Default for StreamConfig {
	fn default() -> Self {
		Self {
			port: 48010,
			video: Default::default(),
			audio: Default::default(),
			control: Default::default(),
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoStreamConfig {
	/// Port to use for streaming video data.
	pub(crate) port: u16,

	/// What percentage of data packets should be parity packets.
	pub(crate) fec_percentage: u8,

	/// Whether to enable video stream encryption (AES-128-GCM).
	#[serde(default = "default_false")]
	pub(crate) encrypt: bool,

	/// Whether to emit a WARN log when a single frame takes longer to encode and
	#[serde(default = "default_false")]
	pub(crate) log_frame_spikes: bool,
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			fec_percentage: 20,
			encrypt: false,
			log_frame_spikes: false,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStreamConfig {
	/// Port to use for streaming audio data.
	pub(crate) port: u16,
}

impl Default for AudioStreamConfig {
	fn default() -> Self {
		Self { port: 48000 }
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlStreamConfig {
	/// Port to use for streaming control data.
	pub(crate) port: u16,
}

impl Default for ControlStreamConfig {
	fn default() -> Self {
		Self { port: 47999 }
	}
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct KeyboardConfig {
	pub layout: String,
	pub variant: String,
	pub model: String,
	pub options: Option<String>,
}

impl Default for KeyboardConfig {
	fn default() -> Self {
		Self {
			layout: "us".to_string(),
			variant: String::new(),
			model: String::new(),
			options: None,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CompositorConfig {
	pub gpu: Option<String>,
	pub hdr: bool,
	pub keyboard: KeyboardConfig,
}

impl Default for CompositorConfig {
	fn default() -> Self {
		Self {
			gpu: None,
			hdr: true,
			keyboard: KeyboardConfig::default(),
		}
	}
}
