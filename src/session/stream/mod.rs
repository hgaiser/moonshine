pub use self::{
	audio::{AudioStreamContext, AudioStream},
	video::{VideoStreamContext, VideoStream},
	control::ControlStream,
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

impl RtpHeader {
	fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.header.to_be_bytes());
		buffer.extend(self.packet_type.to_be_bytes());
		buffer.extend(self.sequence_number.to_be_bytes());
		buffer.extend(self.timestamp.to_be_bytes());
		buffer.extend(self.ssrc.to_be_bytes());
	}
}
