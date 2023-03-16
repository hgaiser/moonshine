use std::path::PathBuf;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
	/// Name of the service.
	pub name: String,

	/// Address to bind to.
	pub address: String,

	/// Configuration for the webserver.
	pub webserver: WebserverConfig,

	/// Configuration for the RTSP server.
	pub rtsp: RtspConfig,

	/// List of applications to expose to clients.
	pub applications: Vec<ApplicationConfig>,

	/// Configuration for sessions with clients.
	pub session: SessionConfig,
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
pub struct SessionConfig {
	/// Target frames per second for the stream.
	pub fps: u32,

	/// Type of codec to use.
	pub codec: String,

	// /// Quality for the stream.
	// pub video_quality: VideoQuality,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RtspConfig {
	/// Port to bind the RTSP server to.
	pub port: u16,
}
