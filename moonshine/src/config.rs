use std::path::PathBuf;
use serde::Deserialize;

use ffmpeg::{CodecType, VideoQuality};

#[derive(Debug, Deserialize)]
pub struct Config {
	/// Name of the service.
	pub name: String,

	/// Address to bind to.
	pub address: String,

	/// Port number to bind RTSP server to.
	pub port: u16,

	/// Config for SSL certificates.
	pub tls: TlsConfig,

	/// List of applications to expose to clients.
	pub applications: Vec<ApplicationConfig>,

	/// Configuration for sessions with clients.
	pub session: SessionConfig,
}

#[derive(Debug, Deserialize)]
pub struct TlsConfig {
	/// Path to the certificate chain for SSL encryption.
	pub certificate_chain: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct ApplicationConfig {
	/// Title of the application.
	pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionConfig {
	/// Target frames per second for the stream.
	pub fps: u32,

	/// Type of codec to use.
	pub codec: CodecType,

	/// Quality for the stream.
	pub video_quality: VideoQuality,
}
