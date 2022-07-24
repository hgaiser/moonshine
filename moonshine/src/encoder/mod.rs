use std::{ffi::{CStr, CString}, ptr::{null_mut, NonNull}, mem::MaybeUninit, os::raw::c_char};

use ffmpeg_sys::{av_log_set_level, AVFormatContext, AVBufferRef, AVFrame, AVStream, CUcontext, AV_LOG_TRACE};

use crate::error::FfmpegError;

use self::codec::Codec;
pub use self::codec::CodecType;

use self::muxer::Muxer;

mod codec;
mod muxer;

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

fn to_c_str(data: &str) -> Result<CString, String> {
	CString::new(data)
		.map_err(|e| format!("Failed to create CString: {}", e))
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

fn open_video(
	codec_context: &Codec,
	cuda_context: CUcontext,
) -> Result<NonNull<AVBufferRef>, FfmpegError> {
	unsafe {
		let device_ctx = ffmpeg_sys::av_hwdevice_ctx_alloc(ffmpeg_sys::AVHWDeviceType_AV_HWDEVICE_TYPE_CUDA);
		if device_ctx.is_null() {
			return Err(FfmpegError::new(-1, "Failed to create hardware device context.".into()));
		}

		let hw_device_context = (*device_ctx).data as *mut ffmpeg_sys::AVHWDeviceContext;
		let cuda_device_context = (*hw_device_context).hwctx as *mut ffmpeg_sys::AVCUDADeviceContext;
		(*cuda_device_context).cuda_ctx = cuda_context;
		check_ret(ffmpeg_sys::av_hwdevice_ctx_init(device_ctx))?;

		let frame_context = ffmpeg_sys::av_hwframe_ctx_alloc(device_ctx) as *mut ffmpeg_sys::AVBufferRef;
		if frame_context.is_null() {
			return Err(FfmpegError::new(-1, "Failed to create hwframe context.".into()));
		}

		let hw_frame_context = (*frame_context).data as *mut ffmpeg_sys::AVHWFramesContext;
		(*hw_frame_context).width = codec_context.as_ref().width;
		(*hw_frame_context).height = codec_context.as_ref().height;
		(*hw_frame_context).sw_format = ffmpeg_sys::AV_PIX_FMT_0RGB32;
		(*hw_frame_context).format = codec_context.as_ref().pix_fmt;
		(*hw_frame_context).device_ref = device_ctx;
		(*hw_frame_context).device_ctx = (*device_ctx).data as *mut ffmpeg_sys::AVHWDeviceContext;

		check_ret(ffmpeg_sys::av_hwframe_ctx_init(frame_context))?;
		let frames_context2 = &*hw_frame_context;

		println!("{:#?}", frames_context2);

		codec_context.as_mut().hw_device_ctx = device_ctx;
		codec_context.as_mut().hw_frames_ctx = frame_context;

		let mut options: *mut ffmpeg_sys::AVDictionary = null_mut();
		check_ret(ffmpeg_sys::av_dict_set(
			&mut options,
			to_c_str("preset").map_err(|e| FfmpegError::new(-1, e))?.as_ptr(),
			to_c_str("slow").map_err(|e| FfmpegError::new(-1, e))?.as_ptr(),
			0
		))?;
		// check_ret(ffmpeg_sys::av_opt_set(
		// 	(*codec_context).priv_data,
		//	to_c_str("tune").map_err(|e| FfmpegError::new(-1, e))?.as_ptr(),
		//	to_c_str("zerolatency").map_err(|e| FfmpegError::new(-1, e))?.as_ptr(),
		// 	0
		// ))?;

		check_ret(ffmpeg_sys::avcodec_open2(codec_context.as_ptr(), codec_context.as_ref().codec, &mut options))?;

		Ok(NonNull::new(device_ctx).unwrap())
	}
}

fn receive_frames(
	av_codec_context: &Codec,
	stream_index: i32,
	stream: *const AVStream,
	frame: NonNull<AVFrame>,
	av_format_context: *mut AVFormatContext,
	// std::mutex &write_output_mutex
) -> Result<(), FfmpegError> {
	unsafe {
		let mut av_packet: ffmpeg_sys::AVPacket = MaybeUninit::zeroed().assume_init();
		loop {
			av_packet.data = null_mut();
			av_packet.size = 0;
			let res = ffmpeg_sys::avcodec_receive_packet(av_codec_context.as_ptr(), &mut av_packet);
			if res == 0 { // we have a packet, send the packet to the muxer
				av_packet.stream_index = stream_index;
				av_packet.pts = frame.as_ref().pts;
				av_packet.dts = frame.as_ref().pts;

				// std::lock_guard<std::mutex> lock(write_output_mutex);
				ffmpeg_sys::av_packet_rescale_ts(&mut av_packet, av_codec_context.as_ref().time_base, (*stream).time_base);
				av_packet.stream_index = (*stream).index;
				check_ret(ffmpeg_sys::av_interleaved_write_frame(av_format_context, &mut av_packet))?;
				ffmpeg_sys::av_packet_unref(&mut av_packet);
			} else if res == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) { // we have no packet
				// fprintf(stderr, "No packet!\n");
				break;
			} else if res == ffmpeg_sys::AVERROR_EOF { // this is the end of the stream
				let error_message = get_error(res)
					.map_err(|_| FfmpegError::new(res, "End of stream".into()))?;
				return Err(FfmpegError::new(res, error_message));
			} else {
				let error_message = get_error(res)
					.map_err(|_| FfmpegError::new(res, "Unknown error".into()))?;
				return Err(FfmpegError::new(res, error_message));
			}
		}
		//av_packet_unref(&av_packet);

		Ok(())
	}
}

pub struct NvencEncoder {
	frame: NonNull<AVFrame>,
	codec: Codec,
	muxer: Muxer,
}

impl NvencEncoder {
	pub fn new(
		cuda_context: CUcontext,
		codec_type: CodecType,
		width: u32,
		height: u32,
	) -> Result<Self, String> {
		unsafe {
			av_log_set_level(AV_LOG_TRACE as i32);

			let codec = Codec::new(
				width,
				height,
				codec_type,
			)?;

			let muxer = Muxer::new(&codec)?;

			let device_ctx = open_video(
				&codec,
				cuda_context,
			)
				.map_err(|e| format!("Failed to open video: {}", e))?;

			let frame = NonNull::new(ffmpeg_sys::av_frame_alloc());
			let mut frame = frame.ok_or_else(|| "Failed to allocate frame".to_string())?;
			frame.as_mut().format = codec.as_ref().pix_fmt;
			frame.as_mut().width = codec.as_ref().width;
			frame.as_mut().height = codec.as_ref().height;
			frame.as_mut().key_frame = 1;
			frame.as_mut().hw_frames_ctx = codec.as_ref().hw_frames_ctx;
			// println!("Format: {}", ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA);
			// frame.as_mut().hw_frames_ctx = ffmpeg_sys::av_hwframe_ctx_alloc(codec.as_ref().hw_device_ctx);
			// if frame.as_ref().hw_frames_ctx.is_null() {
			// 	return Err("Failed to allocated a hardware frame context.".into());
			// }
			// let frames_context = &mut *((*codec.as_ref().hw_frames_ctx).data as *mut ffmpeg_sys::AVHWFramesContext);
			// frames_context.format = ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA;
			// frames_context.sw_format = codec.as_ref().sw_pix_fmt;
			// frames_context.width = frame.as_mut().width;
			// frames_context.height = frame.as_mut().height;
			// check_ret(ffmpeg_sys::av_hwframe_ctx_init(frame.as_ref().hw_frames_ctx))
			// 	.map_err(|e| format!("Failed to initialize hwframe context: {}", e))?;
			let frames_context2 = &mut *((*frame.as_mut().hw_frames_ctx).data as *mut ffmpeg_sys::AVHWFramesContext);
			println!("{:#?}", frames_context2);
			if ffmpeg_sys::av_hwframe_get_buffer(codec.as_ref().hw_frames_ctx, frame.as_ptr(), 0) < 0 {
				panic!("Failed to allocate hardware buffer");
			}
			let frames_context2 = &mut *((*frame.as_mut().hw_frames_ctx).data as *mut ffmpeg_sys::AVHWFramesContext);
			println!("{:#?}", frames_context2);

			// panic!("{:#?}", (*frame));

			// Recompute the linesize, because ffmpeg assumes the CUDA blob is pitched (aligned) memory, which it isn't.
			frame.as_mut().linesize[0] = frame.as_ref().width * 4;

			Ok(Self {
				frame,
				codec,
				muxer,
			})
		}

	}

	pub fn encode(&mut self, device_buffer: usize, time: std::time::Duration) -> Result<(), String> {
		let video_stream_index = 0;
		unsafe {
			self.frame.as_mut().data[0] = device_buffer as *mut u8;
			self.frame.as_mut().pts = (time.as_secs_f64() * ffmpeg_sys::AV_TIME_BASE as f64) as i64;
			// next_recording_time = std::time::Instant::now() + std::time::Duration::from_secs_f64(time_step);

			let res = ffmpeg_sys::avcodec_send_frame(self.codec.as_ptr(), self.frame.as_ptr());
			if res >= 0 {
				receive_frames(
					&self.codec,
					video_stream_index,
					self.muxer.video_stream(),
					self.frame,
					self.muxer.as_ptr(),
				)
					.map_err(|e| format!("Failed to encode image: {}", e))?;
			} else {
				eprintln!("Error: avcodec_send_frame failed: {}", get_error(res)?);
			}
		};

		Ok(())
	}

	pub fn stop(&self) -> Result<(), String> {
		self.muxer.stop()?;
		Ok(())
	}
}
