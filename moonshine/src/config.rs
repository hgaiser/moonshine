use std::{path::{PathBuf, Path}, collections::hash_map::DefaultHasher, hash::{Hash, Hasher}};
use serde::Deserialize;

fn default_stream_timeout() -> u64 { 60 }

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
	#[serde(rename = "application")]
	pub applications: Vec<ApplicationConfig>,

	/// List of scanners that dynamically adds applications when started.
	#[serde(rename = "application_scanner")]
	#[serde(skip_serializing_if = "Vec::is_empty", default)]
	pub application_scanners: Vec<ApplicationScannerConfig>,

	/// Time in seconds since last ping after which the stream closes.
	#[serde(default = "default_stream_timeout")]
	pub stream_timeout: u64,
}

impl Config {
	#[allow(clippy::result_unit_err)]
	pub fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
		let config = std::fs::read_to_string(file)
			.map_err(|e| log::error!("Failed to open configuration file: {e}"))?;
		let config: Config = toml::from_str(&config)
			.map_err(|e| log::error!("Failed to parse configuration file: {e}"))?;

		Ok(config)
	}
}

#[derive(Clone, Debug, Deserialize)]
pub struct WebserverConfig {
	/// Port of the webserver.
	pub port: u16,

	/// Port of the HTTPS webserver.
	pub port_https: u16,

	/// Path to the certificate chain for SSL encryption.
	pub certificate_chain: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ApplicationScannerConfig {
	/// Scans a 'libraryfolders.vdf' file from a Steam library directory.
	Steam(SteamApplicationScannerConfig),
}

#[derive(Clone, Debug, Deserialize)]
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

// pub trait AsStr {
// 	fn as_str(&self) -> &str;
// }

// /// Supported codecs for h264 encoding.
// #[derive(Clone, Debug, Deserialize)]
// #[serde(rename_all = "snake_case")]
// pub enum H264Codec {
// 	Nvenc,
// }

// impl AsStr for &H264Codec {
// 	fn as_str(&self) -> &str {
// 		match self {
// 			H264Codec::Nvenc => "h264_nvenc",
// 		}
// 	}
// }

// /// Supported codecs for hevc encoding.
// #[derive(Clone, Debug, Deserialize)]
// #[serde(rename_all = "snake_case")]
// pub enum HevcCodec {
// 	Nvenc,
// }

// impl AsStr for &HevcCodec {
// 	fn as_str(&self) -> &str {
// 		match self {
// 			HevcCodec::Nvenc => "hevc_nvenc",
// 		}
// 	}
// }

#[derive(Clone, Debug, Deserialize)]
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
