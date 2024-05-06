use std::{path::{PathBuf, Path}, collections::hash_map::DefaultHasher, hash::{Hash, Hasher}};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
	/// Name of the Moonshine host.
	pub name: String,

	/// Address to bind to.
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

	/// Time in seconds since last ping after which the stream closes.
	pub stream_timeout: u64,
}

impl Config {
	#[allow(clippy::result_unit_err)]
	pub fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
		let config = std::fs::read_to_string(file)
			.map_err(|e| tracing::error!("Failed to open configuration file: {e}"))?;
		let config: Config = toml::from_str(&config)
			.map_err(|e| tracing::error!("Failed to parse configuration file: {e}"))?;

		Ok(config)
	}
}

impl Default for Config {
	fn default() -> Self {
		Self {
			name: "Moonshine".to_string(),
			address: "0.0.0.0".to_string(),
			webserver: Default::default(),
			stream: Default::default(),
			applications: vec![
				ApplicationConfig {
					title: "Desktop".to_string(),
					run_before: Some(vec![
						vec![
							"$HOME/.local/bin/resolution".to_string(),
							"{width}".to_string(),
							"{height}".to_string(),
						],
					]),
					run_after: Some(vec![
						vec!["$HOME/.local/bin/resolution".to_string()],
					]),
					boxart: None,
				},

				ApplicationConfig {
					title: "Steam".to_string(),
					run_before: Some(vec![
						vec![
							"$HOME/.local/bin/resolution".to_string(),
							"{width}".to_string(),
							"{height}".to_string(),
						],
						vec![
							"/usr/bin/steam".to_string(),
							"steam://open/bigpicture".to_string(),
						],
					]),
					run_after: Some(vec![
						vec!["$HOME/.local/bin/resolution".to_string()],
					]),
					boxart: None,
				},
			],
			application_scanners: vec![
				ApplicationScannerConfig::Steam(SteamApplicationScannerConfig {
					library: "$HOME/.local/share/Steam".into(),
					run_before: Some(vec![
						vec![
							"$HOME/.local/bin/resolution".to_string(),
							"{width}".to_string(),
							"{height}".to_string(),
						],
						vec![
							"/usr/bin/steam".to_string(),
							"steam://open/bigpicture".to_string(),
						],
						vec![
							"/usr/bin/steam".to_string(),
							"steam://rungameid/{game_id}".to_string(),
						],
					]),
					run_after: Some(vec![
						vec!["$HOME/.local/bin/resolution".to_string()],
					]),
				}),
			],
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
	pub boxart: Option<PathBuf>,

	/// If provided, run this command before starting this application.
	///
	/// Note that multiple entries can be provided, in which case they will be executed in that same order.
	pub run_before: Option<Vec<Vec<String>>>,

	/// If provided, run this command after stopping this application.
	///
	/// Note that multiple entries can be provided, in which case they will be executed in that same order.
	pub run_after: Option<Vec<Vec<String>>>,
}

impl ApplicationConfig {
	pub fn id(&self) -> i32 {
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteamApplicationScannerConfig {
	/// Path to a Steam library (ie. `~/.local/share/Steam`).
	pub library: PathBuf,

	/// If provided, run this command before starting an application.
	///
	/// Note that multiple entries can be provided, in which case they will be executed in that same order.
	pub run_before: Option<Vec<Vec<String>>>,

	/// If provided, run this command after stopping an application.
	///
	/// Note that multiple entries can be provided, in which case they will be executed in that same order.
	pub run_after: Option<Vec<Vec<String>>>,

}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamConfig {
	/// Port to bind the RTSP server to.
	pub port: u16,

	/// Configuration for the video stream.
	pub video: VideoStreamConfig,

	/// Configuration for the audio stream.
	pub audio: AudioStreamConfig,

	/// Configuration for the control stream.
	pub control: ControlStreamConfig,
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
	pub port: u16,

	/// Type of codec to use for h264.
	pub codec_h264: String,

	/// Type of codec to use for h264.
	pub codec_hevc: String,

	/// What percentage of data packets should be parity packets.
	pub fec_percentage: u8,
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			codec_h264: "h264_nvenc".to_string(),
			codec_hevc: "hevc_nvenc".to_string(),
			fec_percentage: 20,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStreamConfig {
	/// Port to use for streaming audio data.
	pub port: u16,
}

impl Default for AudioStreamConfig {
	fn default() -> Self {
		Self { port: 48000 }
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlStreamConfig {
	/// Port to use for streaming control data.
	pub port: u16,
}

impl Default for ControlStreamConfig {
	fn default() -> Self {
		Self { port: 47999 }
	}
}
