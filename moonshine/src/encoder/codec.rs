use crate::error::FfmpegError;

use super::{to_c_str, video_frame::VideoFrame, check_ret};

#[derive(Debug, PartialEq, Eq)]
pub enum CodecType {
	H264,
	Hevc,
}

impl From<&str> for CodecType {
	fn from(codec: &str) -> Self {
		match codec {
			"h264_nvenc" => CodecType::H264,
			"hevc_nvenc" => CodecType::Hevc,
			_ => panic!("Invalid codec '{}'", codec),
		}
	}
}

pub(super) struct Codec {
	pub(super) codec_context: *mut ffmpeg_sys::AVCodecContext,
}

impl Codec {
	pub(super) fn new(width: u32, height: u32, codec_type: CodecType) -> Result<Self, String> {
		unsafe {
			// Find the right codec.
			let codec = match codec_type {
				CodecType::H264 => ffmpeg_sys::avcodec_find_encoder_by_name(to_c_str("h264_nvenc")?.as_ptr()),
				CodecType::Hevc => ffmpeg_sys::avcodec_find_encoder_by_name(to_c_str("hevc_nvenc")?.as_ptr()),
			};
			if codec.is_null() {
				return Err(format!("Codec '{:?}' is not found in ffmpeg.", codec_type));
			}
			let codec = &*codec;

			// Allocate a video codec context.
			let codec_context = ffmpeg_sys::avcodec_alloc_context3(codec);
			if codec_context.is_null() {
				return Err("Failed to create codec context.".into());
			}
			let codec_context = &mut *codec_context;
			if codec.type_ != ffmpeg_sys::AVMediaType_AVMEDIA_TYPE_VIDEO {
				return Err(format!("Expected video encoder, but got type: {}", (*codec).type_));
			}

			// Configure the codec context.
			codec_context.width = width as i32;
			codec_context.height = height as i32;
			codec_context.time_base.num = 1;
			codec_context.time_base.den = ffmpeg_sys::AV_TIME_BASE as i32;
			codec_context.pix_fmt = ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA;

			Ok(Self { codec_context })
		}
	}

	pub(super) fn send_frame(&self, frame: &VideoFrame) -> Result<(), FfmpegError> {
		unsafe {
			check_ret(ffmpeg_sys::avcodec_send_frame(self.as_ptr(), frame.as_ptr()))
		}
	}

	pub(super) fn as_ptr(&self) -> *mut ffmpeg_sys::AVCodecContext {
		self.codec_context
	}

	pub(super) unsafe fn as_ref(&self) -> &ffmpeg_sys::AVCodecContext {
		&*self.codec_context
	}

	pub(super) unsafe fn as_mut(&self) -> &mut ffmpeg_sys::AVCodecContext {
		&mut *self.codec_context
	}
}

impl Drop for Codec {
	fn drop(&mut self) {
		unsafe { ffmpeg_sys::avcodec_free_context(&mut self.codec_context); };
	}
}
