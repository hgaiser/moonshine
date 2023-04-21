use std::path::{PathBuf, Path};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
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
	pub applications: Vec<ApplicationConfig>,
}

impl Config {
	pub fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
		let config = std::fs::read_to_string(file)
			.map_err(|e| log::error!("Failed to open configuration file: {}", e))?;
		let config: Config = toml::from_str(&config)
			.map_err(|e| log::error!("Failed to parse configuration file: {}", e))?;

		Ok(config)
	}
}

#[derive(Clone, Debug, Deserialize)]
pub struct WebserverConfig {
	/// Port number of the webserver.
	pub port: u16,

	/// Port number of the HTTPS webserver.
	pub port_https: u16,

	/// Path to the certificate chain for SSL encryption.
	pub certificate_chain: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApplicationConfig {
	/// Title of the application.
	pub title: String,
}

#[derive(Clone, Debug, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
pub struct VideoStreamConfig {
	/// Port to use for streaming video data.
	pub port: u16,

	/// Type of codec to use.
	pub codec: String,

	/// What percentage of data packets should be parity packets.
	pub fec_percentage: u8,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AudioStreamConfig {
	/// Port to use for streaming audio data.
	pub port: u16,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ControlStreamConfig {
	/// Port to use for streaming control data.
	pub port: u16,
}
