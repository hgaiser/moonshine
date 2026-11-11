use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use crate::app_scanner::ApplicationScannerConfig;
pub use crate::app_scanner::lutris::LutrisApplicationScannerConfig;
pub use crate::app_scanner::steam::SteamApplicationScannerConfig;
pub use crate::session::application::ApplicationConfig;
use crate::session::compositor::CompositorConfig;
use crate::session::stream::StreamConfig;
pub use crate::session::stream::control::input::gamepad::GamepadConfig;
pub use crate::stats_socket::StatsConfig;
use crate::webserver::WebserverConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
	/// Name of the Moonshine host.
	pub name: String,

	/// Address to bind to.
	///
	/// Use IPv4 (eg. `0.0.0.0`) for IPv4-only, or IPv6 (eg. `::`) for dual-stack (IPv4 + IPv6).
	pub address: String,

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

	/// Inhibit system sleep (suspend) while a session is active.
	///
	/// Moonshine asks logind to block sleep for the duration of a stream. This
	/// requires the user to be in the `moonshine` group and the polkit rule
	/// shipped with Moonshine to be installed (handled by the packages). Set to
	/// `false` to disable.
	#[serde(default = "default_inhibit_sleep")]
	pub inhibit_sleep: bool,

	/// Statistics for monitoring and benchmark tools.
	#[serde(default, skip_serializing_if = "StatsConfig::is_default")]
	pub stats: StatsConfig,

	/// Titles of scanned applications that clients don't see.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub hidden_applications: Vec<String>,

	/// The web control panel.
	#[serde(default)]
	pub ui: UiConfig,
}

/// The `[ui]` table: the web control panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
	/// Whether to serve the control panel.
	pub enabled: bool,

	/// Port of the control panel.
	pub port: u16,

	/// Address ranges (CIDR) that can open the control panel.
	///
	/// Anyone in these ranges can change the configuration and add applications, which run
	/// commands as the user Moonshine runs as. Only loopback is allowed by default.
	pub allowed_networks: Vec<String>,

	/// More host names the control panel can be opened by, such as the name of a reverse proxy.
	///
	/// The IP addresses, `localhost` and this computer's own name always work. Other names are
	/// refused, so that a website cannot reach the control panel by DNS rebinding.
	pub allowed_hosts: Vec<String>,
}

impl Default for UiConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			port: 47990,
			allowed_networks: vec!["127.0.0.1/32".to_string(), "::1/128".to_string()],
			allowed_hosts: Vec::new(),
		}
	}
}

fn default_inhibit_sleep() -> bool {
	true
}

impl Config {
	#[allow(clippy::result_unit_err)]
	pub fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
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

		if let Some(socket) = &self.stats.socket {
			let path = shellexpand::full(&socket.to_string_lossy())
				.map_err(|e| tracing::warn!("Failed to expand statistics socket path: {e}"))?
				.to_string();
			self.stats.socket = Some(path.into());
		}

		Ok(())
	}
}

impl Default for Config {
	fn default() -> Self {
		Self {
			name: "Moonshine".to_string(),
			// IPv4-only by default; set to `::` to bind dual-stack (the webserver
			// disables IPV6_V6ONLY, so that single address covers both).
			address: "0.0.0.0".to_string(),
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
				env: Default::default(),
			})],
			compositor: CompositorConfig::default(),
			inhibit_sleep: true,
			stats: StatsConfig::default(),
			hidden_applications: Vec::new(),
			ui: UiConfig::default(),
		}
	}
}
