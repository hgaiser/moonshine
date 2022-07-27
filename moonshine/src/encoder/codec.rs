use std::{ptr::null_mut, mem::MaybeUninit};

use crate::error::FfmpegError;

use super::{to_c_str, video_frame::VideoFrame, check_ret, muxer::Muxer, get_error};

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub enum VideoQuality {
	Slowest,
	Slower,
	Slow,
	Medium,
	Fast,
	Faster,
	Fastest,
}

impl From<VideoQuality> for &str {
	fn from(value: VideoQuality) -> Self {
		match value {
			VideoQuality::Slowest => "p7",
			VideoQuality::Slower => "p6",
			VideoQuality::Slow => "p5",
			VideoQuality::Medium => "p4",
			VideoQuality::Fast => "p3",
			VideoQuality::Faster => "p2",
			VideoQuality::Fastest => "p1",
		}
	}
}

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
	pub(super) fn new(
		width: u32,
		height: u32,
		codec_type: CodecType,
		quality: VideoQuality,
		cuda_context: ffmpeg_sys::CUcontext,
	) -> Result<Self, String> {
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
			// codec_context.gop_size = 0; // Only intra frames.

			let device_ctx = ffmpeg_sys::av_hwdevice_ctx_alloc(ffmpeg_sys::AVHWDeviceType_AV_HWDEVICE_TYPE_CUDA);
			if device_ctx.is_null() {
				return Err("Failed to create hardware device context.".to_string());
			}

			let hw_device_context = (*device_ctx).data as *mut ffmpeg_sys::AVHWDeviceContext;
			let cuda_device_context = (*hw_device_context).hwctx as *mut ffmpeg_sys::AVCUDADeviceContext;
			(*cuda_device_context).cuda_ctx = cuda_context;
			check_ret(ffmpeg_sys::av_hwdevice_ctx_init(device_ctx))
				.map_err(|e| format!("Failed to initialize hardware device: {}", e))?;

			let frame_context = ffmpeg_sys::av_hwframe_ctx_alloc(device_ctx) as *mut ffmpeg_sys::AVBufferRef;
			if frame_context.is_null() {
				return Err("Failed to create hwframe context.".to_string());
			}

			let hw_frame_context = &mut *((*frame_context).data as *mut ffmpeg_sys::AVHWFramesContext);
			hw_frame_context.width = codec_context.width;
			hw_frame_context.height = codec_context.height;
			hw_frame_context.sw_format = ffmpeg_sys::AV_PIX_FMT_0RGB32;
			hw_frame_context.format = codec_context.pix_fmt;

			check_ret(ffmpeg_sys::av_hwframe_ctx_init(frame_context))
				.map_err(|e| format!("Failed to initialize hardware frame context: {}", e))?;

			codec_context.hw_frames_ctx = frame_context;

			let mut options: *mut ffmpeg_sys::AVDictionary = null_mut();
			check_ret(ffmpeg_sys::av_dict_set(
				&mut options,
				to_c_str("zerolatency")?.as_ptr(),
				to_c_str("1")?.as_ptr(),
				0
			))
				.map_err(|e| format!("Failed to set dictionary with options: {}", e))?;
			check_ret(ffmpeg_sys::av_dict_set(
				&mut options,
				to_c_str("preset")?.as_ptr(),
				to_c_str(quality.into())?.as_ptr(),
				0
			))
				.map_err(|e| format!("Failed to set dictionary with options: {}", e))?;

			check_ret(ffmpeg_sys::avcodec_open2(codec_context, codec_context.codec, &mut options))
				.map_err(|e| format!("Failed to open codec: {}", e))?;

			Ok(Self { codec_context })
		}
	}

	pub(super) fn send_frame(&mut self, frame: &VideoFrame, muxer: &Muxer) -> Result<(), FfmpegError> {
		unsafe {
			check_ret(ffmpeg_sys::avcodec_send_frame(self.as_ptr(), frame.as_ptr()))?;

			loop {
				let mut packet: ffmpeg_sys::AVPacket = MaybeUninit::zeroed().assume_init();
				let res = ffmpeg_sys::avcodec_receive_packet(self.as_ptr(), &mut packet);
				if res == 0 { // we have a packet, send the packet to the muxer
					packet.pts = frame.as_ref().pts;
					packet.dts = frame.as_ref().pts;

					ffmpeg_sys::av_packet_rescale_ts(&mut packet, self.as_ref().time_base, (*muxer.video_stream()).time_base);
					check_ret(ffmpeg_sys::av_interleaved_write_frame(muxer.as_ptr(), &mut packet))?;
				} else if res == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
					// This means we can't encode the frame yet.
					break;
				} else if res == ffmpeg_sys::AVERROR_EOF {
					let error_message = get_error(res)
						.map_err(|_| FfmpegError::new(res, "End of stream".into()))?;
					return Err(FfmpegError::new(res, error_message));
				} else {
					let error_message = get_error(res)
						.map_err(|_| FfmpegError::new(res, "Unknown error".into()))?;
					return Err(FfmpegError::new(res, error_message));
				}
			}

			Ok(())
		}
	}

	pub(super) fn as_ptr(&self) -> *mut ffmpeg_sys::AVCodecContext {
		self.codec_context
	}

	pub(super) unsafe fn as_ref(&self) -> &ffmpeg_sys::AVCodecContext {
		&*self.codec_context
	}
}

impl Drop for Codec {
	fn drop(&mut self) {
		unsafe { ffmpeg_sys::avcodec_free_context(&mut self.codec_context); };
	}
}
