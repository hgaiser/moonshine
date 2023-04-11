use std::ptr::{null_mut, null};

use crate::{util::check_ret, FfmpegError, Frame, Packet};

use super::Codec;

pub struct CodecContext {
	codec_context: *mut ffmpeg_sys::AVCodecContext,
}

impl CodecContext {
	fn new(codec_context: *mut ffmpeg_sys::AVCodecContext) -> Self {
		Self { codec_context }
	}

	pub fn send_frame(&mut self, frame: Option<&Frame>) -> Result<(), FfmpegError> {
		let frame = match frame {
			Some(frame) => frame.as_raw() as *const _,
			None => null(),
		};

		check_ret(unsafe { ffmpeg_sys::avcodec_send_frame(self.as_raw_mut(), frame) })
	}

	pub fn receive_packet(&mut self, packet: &mut Packet) -> Result<(), FfmpegError> {
		check_ret(unsafe { ffmpeg_sys::avcodec_receive_packet(self.as_raw_mut(), packet.as_raw_mut()) })
	}

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg_sys::AVCodecContext {
		unsafe { &mut *self.codec_context }
	}

	pub fn as_raw(&self) -> &ffmpeg_sys::AVCodecContext {
		unsafe { &*self.codec_context }
	}
}

// TODO: Check this is valid.
unsafe impl Send for CodecContext {}
unsafe impl Sync for CodecContext {}

impl Drop for CodecContext {
	fn drop(&mut self) {
		unsafe { ffmpeg_sys::avcodec_free_context(&mut self.codec_context) };
	}
}

pub struct CodecContextBuilder {
	codec_context: *mut ffmpeg_sys::AVCodecContext,
}

impl CodecContextBuilder {
	pub fn new(codec: &Codec) -> Result<Self, String> {
		let codec_context = unsafe { ffmpeg_sys::avcodec_alloc_context3(codec.as_raw()) };
		if codec_context.is_null() {
			return Err("could not allocate a codec context".to_string());
		}
		Ok(Self { codec_context })
	}

	pub fn open(mut self) -> Result<CodecContext, FfmpegError> {
		unsafe { check_ret(ffmpeg_sys::avcodec_open2(self.codec_context, self.as_raw().codec, null_mut()))?; }
		let result = Ok(CodecContext::new(self.codec_context));
		self.codec_context = null_mut();

		result
	}

	pub fn set_width(&mut self, width: u32) -> &mut Self {
		self.as_raw_mut().width = width as i32;
		self
	}

	pub fn set_height(&mut self, height: u32) -> &mut Self {
		self.as_raw_mut().height = height as i32;
		self
	}

	pub fn set_framerate(&mut self, fps: u32) -> &mut Self {
		self.as_raw_mut().time_base = ffmpeg_sys::AVRational { num: 1, den: fps as i32 };
		self.as_raw_mut().framerate = ffmpeg_sys::AVRational { num: fps as i32, den: 1 };
		self
	}

	pub fn set_max_b_frames(&mut self, max_b_frames: u32) -> &mut Self {
		self.as_raw_mut().max_b_frames = max_b_frames as i32;
		self
	}

	pub fn set_pix_fmt(&mut self, pix_fmt: i32) -> &mut Self {
		// TODO: Make pix_fmt an enum.
		self.as_raw_mut().pix_fmt = pix_fmt;
		self
	}

	pub fn set_bit_rate(&mut self, bit_rate: u64) -> &mut Self {
		self.as_raw_mut().bit_rate = bit_rate as i64;
		self
	}

	pub fn set_gop_size(&mut self, gop_size: u32) -> &mut Self {
		self.as_raw_mut().gop_size = gop_size as i32;
		self
	}

	pub fn set_sample_fmt(&mut self, sample_fmt: u32) -> &mut Self {
		// TODO: Make sample_fmt an enum.
		self.as_raw_mut().sample_fmt = sample_fmt as i32;
		self
	}

	pub fn set_sample_rate(&mut self, sample_rate: u32) -> &mut Self {
		self.as_raw_mut().sample_rate = sample_rate as i32;
		self
	}

	pub fn set_flags(&mut self, flags: u32) -> &mut Self {
		self.as_raw_mut().flags = flags as i32;
		self
	}

	pub fn set_flags2(&mut self, flags: u32) -> &mut Self {
		self.as_raw_mut().flags2 = flags as i32;
		self
	}

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg_sys::AVCodecContext {
		unsafe { &mut *self.codec_context }
	}

	pub fn as_raw(&self) -> &ffmpeg_sys::AVCodecContext {
		unsafe { &*self.codec_context }
	}
}

impl Drop for CodecContextBuilder {
	fn drop(&mut self) {
		if !self.codec_context.is_null(){
			unsafe { ffmpeg_sys::avcodec_free_context(&mut self.codec_context) };
		}
	}
}

unsafe impl Send for CodecContextBuilder { }
