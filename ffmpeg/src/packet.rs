pub struct Packet {
	packet: *mut ffmpeg_sys::AVPacket,
}

impl Packet {
	pub fn new() -> Result<Self, String> {
		let packet = unsafe { ffmpeg_sys::av_packet_alloc() };
		if packet.is_null() {
			return Err("could not allocate packet".to_string());
		}

		Ok(Self { packet })
	}

	pub fn data(&self) -> &[u8] {
		unsafe { std::slice::from_raw_parts(self.as_raw().data, self.as_raw().size as usize) }
	}

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg_sys::AVPacket {
		unsafe { &mut *self.packet }
	}

	pub fn as_raw(&self) -> &ffmpeg_sys::AVPacket {
		unsafe { &*self.packet }
	}
}

impl Drop for Packet {
	fn drop(&mut self) {
		unsafe { ffmpeg_sys::av_packet_free(&mut self.packet) };
	}
}

// TODO: Check if this is correct.
unsafe impl Send for Packet { }
