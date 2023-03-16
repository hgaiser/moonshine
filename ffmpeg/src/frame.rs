use std::ptr::null_mut;

use crate::{FfmpegError, check_ret};

pub struct Frame {
	frame: *mut ffmpeg_sys::AVFrame,
}

impl Frame {
	fn new(frame: *mut ffmpeg_sys::AVFrame) -> Self {
		Self { frame }
	}

	pub fn make_writable(&self) -> Result<(), FfmpegError> {
		check_ret(unsafe { ffmpeg_sys::av_frame_make_writable(self.frame) })
	}

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg_sys::AVFrame {
		unsafe { &mut *self.frame }
	}

	pub fn as_raw(&self) -> &ffmpeg_sys::AVFrame {
		unsafe { &*self.frame }
	}
}

impl Drop for Frame {
	fn drop(&mut self) {
		unsafe { ffmpeg_sys::av_frame_free(&mut self.frame) };
	}
}

pub struct FrameBuilder {
	frame: *mut ffmpeg_sys::AVFrame,
}

impl FrameBuilder {
	pub fn new() -> Result<Self, String> {
		let frame = unsafe { ffmpeg_sys::av_frame_alloc() };
		if frame.is_null() {
			return Err("could not allocate a frame".to_string());
		}

		Ok(Self { frame })
	}

	pub fn allocate(mut self, align: i32) -> Result<Frame, FfmpegError> {
		check_ret(unsafe { ffmpeg_sys::av_frame_get_buffer(self.frame, align) })?;
		let result = Ok(Frame::new(self.frame));
		self.frame = null_mut();

		result
	}

	pub fn set_format(&mut self, format: i32) -> &mut Self {
		// TODO: Make format an enum.
		self.as_raw_mut().format = format;
		self
	}

	pub fn set_width(&mut self, width: u32) -> &mut Self {
		self.as_raw_mut().width = width as i32;
		self
	}

	pub fn set_height(&mut self, height: u32) -> &mut Self {
		self.as_raw_mut().height = height as i32;
		self
	}

	pub fn set_nb_samples(&mut self, nb_samples: u32) -> &mut Self {
		self.as_raw_mut().nb_samples = nb_samples as i32;
		self
	}

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg_sys::AVFrame {
		unsafe { &mut *self.frame }
	}

	pub fn as_raw(&self) -> &ffmpeg_sys::AVFrame {
		unsafe { &*self.frame }
	}
}

impl Drop for FrameBuilder {
	fn drop(&mut self) {
		if !self.frame.is_null() {
			unsafe { ffmpeg_sys::av_frame_free(&mut self.frame) };
		}
	}
}
