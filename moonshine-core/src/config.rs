use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use crate::app_scanner::steam::SteamApplicationScannerConfig;
pub use crate::app_scanner::ApplicationScannerConfig;
pub use crate::session::application::ApplicationConfig;
use crate::session::compositor::CompositorConfig;
use crate::session::stream::StreamConfig;
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
			})],
			compositor: CompositorConfig::default(),
		}
	}
}
