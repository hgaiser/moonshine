use std::{fmt::Debug, ffi::CStr};

use crate::to_c_str;

mod context;
pub use context::{CodecContextBuilder, CodecContext};

pub struct Codec {
	codec: *mut ffmpeg_sys::AVCodec,
}

impl Codec {
	pub fn new(codec_name: &str) -> Result<Self, String> {
		let codec = unsafe {
			ffmpeg_sys::avcodec_find_encoder_by_name(to_c_str(codec_name)?.as_ptr())
		};
		if codec.is_null() {
			return Err(format!("codec '{codec_name}' is not found in ffmpeg"));
		}

		Ok(Self { codec: codec as *mut _ })
	}

	pub fn as_raw(&self) -> &ffmpeg_sys::AVCodec {
		unsafe { &*self.codec }
	}

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg_sys::AVCodec {
		unsafe { &mut *self.codec }
	}
}

impl Debug for Codec {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		unsafe {
			write!(f, "{:?}", CStr::from_ptr(self.as_raw().name))
		}
	}
}

unsafe impl Send for Codec { }
