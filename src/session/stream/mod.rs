pub use self::{
	audio::{AudioConfig, AudioStream, AudioStreamContext, ALL_STREAM_CONFIGS},
	control::ControlStream,
	video::{VideoChromaSampling, VideoDynamicRange, VideoFormat, VideoStream, VideoStreamContext},
};

mod audio;
mod control;
mod video;

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
