use std::{ffi::{CStr, CString}, os::raw::c_char};

use crate::error::FfmpegError;

use self::codec::Codec;
pub use self::codec::VideoQuality;
pub use self::codec::CodecType;

use self::muxer::Muxer;

use video_frame::VideoFrame;

mod codec;
mod muxer;
mod video_frame;

fn check_ret(error_code: i32) -> Result<(), FfmpegError> {
	if error_code != 0 {
		let error_message = get_error(error_code)
			.map_err(|_| FfmpegError::new(error_code, "Unknown error".into()))?;
		return Err(FfmpegError::new(error_code, error_message));
	}

	Ok(())
}

unsafe fn parse_c_str<'a>(data: *const c_char) -> Result<&'a str, String> {
	CStr::from_ptr(data)
		.to_str()
		.map_err(|_e| "invalid UTF-8".to_string())
}

fn to_c_str(data: &str) -> Result<CString, ()> {
	CString::new(data)
		.map_err(|e| log::error!("Failed to create CString: {}", e))
}

fn get_error(error_code: i32) -> Result<String, String> {
	let mut buffer = [0 as c_char; ffmpeg_sys::AV_ERROR_MAX_STRING_SIZE as usize];
	unsafe {
		// Don't use check_ret here, because this function is called by check_ret.
		if ffmpeg_sys::av_strerror(error_code, buffer.as_mut_ptr() as *mut _, ffmpeg_sys::AV_ERROR_MAX_STRING_SIZE as u64) < 0 {
			return Err("Failed to get last ffmpeg error".into());
		}

		Ok(
			parse_c_str(buffer.as_ptr())
				.map_err(|e| format!("Failed to parse error message: {}", e))?
				.to_string()
		)
	}
}

pub struct NvencEncoder {
	frame: VideoFrame,
	codec: Codec,
	muxer: Muxer,
}

impl NvencEncoder {
	pub fn new(
		port: u16,
		width: u32,
		height: u32,
		codec_type: CodecType,
		quality: VideoQuality,
		cuda_context: ffmpeg_sys::CUcontext,
	) -> Result<Self, ()> {
		unsafe {
			// ffmpeg_sys::av_log_set_level(ffmpeg_sys::AV_LOG_TRACE as i32);
			ffmpeg_sys::av_log_set_level(ffmpeg_sys::AV_LOG_QUIET as i32);

			let codec = Codec::new(
				width,
				height,
				codec_type,
				quality,
				cuda_context,
			)?;

			let muxer = Muxer::new(port, &codec)?;

			let frame = VideoFrame::new(&codec)?;

			Ok(Self {
				frame,
				codec,
				muxer,
			})
		}

	}

	pub fn encode(&mut self, device_buffer: usize, time: std::time::Duration) -> Result<(), String> {
		// self.frame.set_buffer(device_buffer, time);
		// self.codec.send_frame(&self.frame, &self.muxer)
		// 	.map_err(|e| format!("Failed to send frame to codec: {}", e))?;
		Ok(())
	}

	pub fn start(&mut self) -> Result<(), ()> {
		self.muxer.start()?;
		Ok(())
	}

	pub fn stop(&self) -> Result<(), ()> {
		self.muxer.stop()?;
		Ok(())
	}

	pub fn local_rtp_port(&self) -> i64 {
		self.muxer.local_rtp_port()
	}

	pub fn local_rtcp_port(&self) -> i64 {
		self.muxer.local_rtcp_port()
	}
}
