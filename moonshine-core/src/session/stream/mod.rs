use serde::Deserialize;
use serde::Serialize;

use crate::session::stream::audio::AudioStreamConfig;
use crate::session::stream::control::ControlStreamConfig;
use crate::session::stream::video::VideoStreamConfig;

pub mod audio;
pub mod control;
pub mod video;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamConfig {
	/// Port to bind the RTSP server to.
	pub port: u16,

	/// Configuration for the video stream.
	pub video: VideoStreamConfig,

	/// Configuration for the audio stream.
	pub audio: AudioStreamConfig,

	/// Configuration for the control stream.
	pub control: ControlStreamConfig,

	/// Time in seconds since last ping after which the stream closes.
	pub timeout: u64,
}

impl Default for StreamConfig {
	fn default() -> Self {
		Self {
			port: 48010,
			video: Default::default(),
			audio: Default::default(),
			control: Default::default(),
			timeout: 60,
		}
	}
}

#[derive(Debug)]
#[repr(C)]
struct RtpHeader {
	header: u8,
	packet_type: u8,
	sequence_number: u16,
	timestamp: u32,
	ssrc: u32,
}

impl RtpHeader {}
