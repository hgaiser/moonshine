#[repr(u8)]
pub(super) enum RtpFlag {
	ContainsPicData = 0x1,
	EndOfFrame = 0x2,
	StartOfFrame = 0x4,
}

pub(super) struct VideoFrameHeader {
	pub(super) header_type: u8,
	pub(super) padding1: u16,
	pub(super) frame_type: u8,
	pub(super) padding2: u32,
}

impl VideoFrameHeader {
	pub(super) fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.header_type.to_le_bytes());
		buffer.extend(self.padding1.to_le_bytes());
		buffer.extend(self.frame_type.to_le_bytes());
		buffer.extend(self.padding2.to_le_bytes());
	}
}

pub(super) struct RtpHeader {
	pub(super) header: u8,
	pub(super) packet_type: u8,
	pub(super) sequence_number: u16,
	pub(super) timestamp: u32,
	pub(super) ssrc: u32,
	pub(super) padding: u32,
}

impl RtpHeader {
	pub(super) fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.header.to_be_bytes());
		buffer.extend(self.packet_type.to_be_bytes());
		buffer.extend(self.sequence_number.to_be_bytes());
		buffer.extend(self.timestamp.to_be_bytes());
		buffer.extend(self.ssrc.to_be_bytes());
		buffer.extend(self.padding.to_be_bytes());
	}
}

pub(super) struct NvVideoPacket {
	pub(super) stream_packet_index: u32,
	pub(super) frame_index: u32,
	pub(super) flags: u8,
	pub(super) reserved: u8,
	pub(super) multi_fec_flags: u8,
	pub(super) multi_fec_blocks: u8,
	pub(super) fec_info: u32,
}

impl NvVideoPacket {
	pub(super) fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.stream_packet_index.to_le_bytes());
		buffer.extend(self.frame_index.to_le_bytes());
		buffer.extend(self.flags.to_le_bytes());
		buffer.extend(self.reserved.to_le_bytes());
		buffer.extend(self.multi_fec_flags.to_le_bytes());
		buffer.extend(self.multi_fec_blocks.to_le_bytes());
		buffer.extend(self.fec_info.to_le_bytes());
	}
}
