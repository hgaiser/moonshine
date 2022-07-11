use std::{ffi::CStr, ptr::{null_mut, null}, mem::MaybeUninit};

use ffmpeg_sys::{av_log_set_level, AVFormatContext, VideoQuality_HIGH, AVBufferRef, CUgraphicsResource, AVFrame, AVCodecContext, AVStream, CUcontext, AV_LOG_QUIET, AV_LOG_TRACE, av_hwdevice_ctx_init};

use crate::error::FfmpegError;

#[derive(Debug, PartialEq)]
pub enum Codec {
	H264,
	Hevc,
}

impl From<&str> for Codec {
	fn from(codec: &str) -> Self {
		match codec {
			"h264_nvenc" => Codec::H264,
			"hevc_nvenc" => Codec::Hevc,
			_ => panic!("Invalid codec '{}'", codec),
		}
	}
}

#[derive(Debug, PartialEq)]
pub enum VideoQuality {
	Low,
	Medium,
	High,
}

fn check_ret(error_code: i32) -> Result<(), FfmpegError> {
	if error_code != 0 {
		let error_message = get_error(error_code)
			.map_err(|_| FfmpegError::new(error_code, "Unknown error".into()))?;
		return Err(FfmpegError::new(error_code, error_message));
	}

	Ok(())
}

fn get_error(error_code: i32) -> Result<String, String> {
	const buffer_size: usize = 512;
	let mut buffer = [0u8; buffer_size];
	unsafe {
		if ffmpeg_sys::av_strerror(error_code, buffer.as_mut_ptr() as *mut _, buffer_size as u64) < 0 {
			return Err("Failed to get last ffmpeg error".into());
		}
	};

	Ok(
		CStr::from_bytes_until_nul(&buffer)
			.map_err(|e| format!("Failed to convert buffer to cstr: {}", e))?
			.to_str()
			.map_err(|e| format!("Failed to convert cstr to str: {}", e))?
			.to_string()
	)
}

fn create_video_codec_context(
	av_format_context: *mut AVFormatContext,
	video_quality: VideoQuality,
	width: u32,
	height: u32,
	fps: u32,
	codec_type: Codec,
) -> Result<*mut AVCodecContext, String> {
	unsafe {
		let codec: *const ffmpeg_sys::AVCodec = match codec_type {
			Codec::H264 => ffmpeg_sys::avcodec_find_encoder_by_name(
				CStr::from_bytes_with_nul(b"h264_nvenc\0")
					.map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()
			),
			Codec::Hevc => ffmpeg_sys::avcodec_find_encoder_by_name(
				CStr::from_bytes_with_nul(b"hevc_nvenc\0")
					.map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()
			),
		};
		if codec.is_null() {
			return Err(format!("Codec '{:?}' is not found in ffmpeg.", codec_type));
		}

		let codec_context: *mut AVCodecContext = ffmpeg_sys::avcodec_alloc_context3(codec);
		if codec.is_null() {
			return Err("Failed to create codec context.".into());
		}

		if (*codec).type_ != ffmpeg_sys::AVMediaType_AVMEDIA_TYPE_VIDEO {
			return Err(format!("Expected video encoder, but got type: {}", (*codec).type_));
		}
		(*codec_context).codec_id = (*codec).id;
		(*codec_context).width = width as i32;
		(*codec_context).height = height as i32;
		(*codec_context).bit_rate = 12500000i64 + ((*codec_context).width * (*codec_context).height) as i64 / 2;
		(*codec_context).time_base.num = 1;
		(*codec_context).time_base.den = ffmpeg_sys::AV_TIME_BASE as i32;
		(*codec_context).framerate.num = fps as i32;
		(*codec_context).framerate.den = 1;
		(*codec_context).sample_aspect_ratio.num = 0;
		(*codec_context).sample_aspect_ratio.den = 0;
		(*codec_context).gop_size = fps as i32 * 2;
		(*codec_context).max_b_frames = 0;
		(*codec_context).pix_fmt = ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA;
		(*codec_context).color_range = ffmpeg_sys::AVColorRange_AVCOL_RANGE_JPEG;
		match video_quality {
			VideoQuality::Low => {
				(*codec_context).bit_rate = 10000000i64 + ((*codec_context).width * (*codec_context).height) as i64 / 2;
				match codec_type {
					Codec::Hevc => {
						(*codec_context).qmin = 20;
						(*codec_context).qmax = 35;
					},
					Codec::H264 => {
						(*codec_context).qmin = 5;
						(*codec_context).qmax = 20;
					}
				};
			},
			VideoQuality::Medium => {
				match codec_type {
					Codec::Hevc => {
						(*codec_context).qmin = 17;
						(*codec_context).qmax = 30;
					},
					Codec::H264 => {
						(*codec_context).qmin = 5;
						(*codec_context).qmax = 15;
					}
				};
			},
			VideoQuality::High => {
				(*codec_context).bit_rate = 15000000i64 + ((*codec_context).width * (*codec_context).height) as i64 / 2;

				match codec_type {
					Codec::Hevc => {
						(*codec_context).qmin = 16;
						(*codec_context).qmax = 25;
					},
					Codec::H264 => {
						(*codec_context).qmin = 3;
						(*codec_context).qmax = 13;
					}
				};
			}
		};
		if (*codec_context).codec_type == ffmpeg_sys::AVCodecID_AV_CODEC_ID_MPEG1VIDEO as i32 {
			(*codec_context).mb_decision = 2;
		}

		// Some formats want stream headers to be seperate
		if ((*(*av_format_context).oformat).flags & ffmpeg_sys::AVFMT_GLOBALHEADER as i32) != 0 {
			(*av_format_context).flags |= ffmpeg_sys::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
		}

		Ok(codec_context)
	}
}

fn open_video(
	codec_context: *mut AVCodecContext,
	device_ctx: *mut *mut AVBufferRef,
	cuda_graphics_resource: *mut CUgraphicsResource,
	cuda_context: CUcontext,
) -> Result<(), FfmpegError> {
	unsafe {
		*device_ctx = ffmpeg_sys::av_hwdevice_ctx_alloc(ffmpeg_sys::AVHWDeviceType_AV_HWDEVICE_TYPE_CUDA);
		if (*device_ctx).is_null() {
			return Err(FfmpegError::new(-1, "Failed to create hardware device context.".into()));
		}

		let hw_device_context = (*(*device_ctx)).data as *mut ffmpeg_sys::AVHWDeviceContext;
		let cuda_device_context = (*hw_device_context).hwctx as *mut ffmpeg_sys::AVCUDADeviceContext;
		(*cuda_device_context).cuda_ctx = cuda_context;
		check_ret(ffmpeg_sys::av_hwdevice_ctx_init(*device_ctx))?;

		let frame_context = ffmpeg_sys::av_hwframe_ctx_alloc(*device_ctx) as *mut ffmpeg_sys::AVBufferRef;
		if frame_context.is_null() {
			return Err(FfmpegError::new(-1, "Failed to create hwframe context.".into()));
		}

		let hw_frame_context = (*frame_context).data as *mut ffmpeg_sys::AVHWFramesContext;
		(*hw_frame_context).width = (*codec_context).width;
		(*hw_frame_context).height = (*codec_context).height;
		(*hw_frame_context).sw_format = ffmpeg_sys::AV_PIX_FMT_0RGB32;
		(*hw_frame_context).format = (*codec_context).pix_fmt;
		(*hw_frame_context).device_ref = *device_ctx;
		(*hw_frame_context).device_ctx = (*(*device_ctx)).data as *mut ffmpeg_sys::AVHWDeviceContext;

		check_ret(ffmpeg_sys::av_hwframe_ctx_init(frame_context))?;

		(*codec_context).hw_device_ctx = *device_ctx;
		(*codec_context).hw_frames_ctx = frame_context;

		check_ret(ffmpeg_sys::avcodec_open2(codec_context, (*codec_context).codec, null_mut()))?;

		Ok(())
	}
}

fn create_stream(
	av_format_context: *mut AVFormatContext,
	codec_context: *mut AVCodecContext,
) -> Result<*mut AVStream, FfmpegError> {
	unsafe {
		let stream = ffmpeg_sys::avformat_new_stream(av_format_context, null());
		if stream.is_null() {
			return Err(FfmpegError::new(-1, "Could not allocate stream.".into()));
		}
		(*stream).id = (*av_format_context).nb_streams as i32 - 1;
		(*stream).time_base = (*codec_context).time_base;
		(*stream).avg_frame_rate = (*codec_context).framerate;
		return Ok(stream);
	}
}

fn receive_frames(
	av_codec_context: *mut AVCodecContext,
	stream_index: i32,
	stream: *mut AVStream,
	frame: *mut AVFrame,
	av_format_context: *mut AVFormatContext,
	// std::mutex &write_output_mutex
) -> Result<(), FfmpegError> {
	unsafe {
		let mut av_packet: ffmpeg_sys::AVPacket = MaybeUninit::zeroed().assume_init();
		loop {
			av_packet.data = null_mut();
			av_packet.size = 0;
			let res = ffmpeg_sys::avcodec_receive_packet(av_codec_context, &mut av_packet);
			if res == 0 { // we have a packet, send the packet to the muxer
				av_packet.stream_index = stream_index;
				av_packet.pts = (*frame).pts;
				av_packet.dts = (*frame).pts;

				// std::lock_guard<std::mutex> lock(write_output_mutex);
				ffmpeg_sys::av_packet_rescale_ts(&mut av_packet, (*av_codec_context).time_base, (*stream).time_base);
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
	pub frame: *mut AVFrame,
	pub format_context: *mut AVFormatContext,
	pub video_codec_context: *mut AVCodecContext,
	pub video_stream: *mut AVStream,
}

impl NvencEncoder {
	pub fn new(cuda_context: CUcontext, codec: Codec, width: u32, height: u32, fps: u32) -> Result<Self, Box<dyn std::error::Error>> {
		let filename = "test.mp4";
		unsafe {
			av_log_set_level(AV_LOG_TRACE as i32);
			let mut av_format_context: *mut AVFormatContext = null_mut();
			let res = ffmpeg_sys::avformat_alloc_output_context2(
				&mut av_format_context,
				null(), null(),
				CStr::from_bytes_with_nul(b"test.mp4\0").map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()
			);
			if res < 0 {
				panic!("Failed to create output format context: {}", res);
			}

			let video_codec_context = create_video_codec_context(
				av_format_context,
				VideoQuality::High,
				width,
				height,
				fps,
				codec,
			)?;

			let video_stream = create_stream(av_format_context, video_codec_context)?;

			let mut device_ctx: *mut AVBufferRef = null_mut();
			let mut cuda_graphics_resource: CUgraphicsResource = null_mut();
			open_video(
				video_codec_context,
				&mut device_ctx,
				&mut cuda_graphics_resource,
				cuda_context,
			)?;

			let res = ffmpeg_sys::avcodec_parameters_from_context((*video_stream).codecpar, video_codec_context);
			if res < 0 {
				panic!("Failed to set parameters from context: {}", res);
			}

			let res = ffmpeg_sys::avio_open(
				&mut (*av_format_context).pb,
				CStr::from_bytes_with_nul(b"test.mp4\0").map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr(),
				ffmpeg_sys::AVIO_FLAG_WRITE as i32
			);
			if res < 0 {
				panic!("Could not open '{}': {}", filename, get_error(res)?);
			}

			let res = ffmpeg_sys::avformat_write_header(av_format_context, null_mut());
			if res < 0 {
				panic!("Error occurred when writing header to output file: {}", get_error(res)?);
			}

			let frame: *mut AVFrame = ffmpeg_sys::av_frame_alloc();
			if frame.is_null() {
				panic!("Failed to allocate frame");
			}
			(*frame).format = (*video_codec_context).pix_fmt;
			(*frame).width = (*video_codec_context).width;
			(*frame).height = (*video_codec_context).height;
			(*frame).key_frame = 1;
			(*frame).hw_frames_ctx = (*video_codec_context).hw_frames_ctx;

			// println!("{:#?}", (*frame));
			// println!("Format: {}", ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA);
			// (*frame).hw_frames_ctx = ffmpeg_sys::av_hwframe_ctx_alloc((*video_codec_context).hw_device_ctx);
			// if (*frame).hw_frames_ctx.is_null() {
			// 	return Err("Failed to allocated a hardware frame context.".into());
			// }
			// check_ret(ffmpeg_sys::av_hwframe_ctx_init((*frame).hw_frames_ctx))?;
			if ffmpeg_sys::av_hwframe_get_buffer((*video_codec_context).hw_frames_ctx, frame, 0) < 0 {
				panic!("Failed to allocate hardware buffer");
			}

			// panic!("{:#?}", (*frame));

			// Recompute the linesize, because ffmpeg assumes the CUDA blob is pitched (aligned) memory, which it isn't.
			(*frame).linesize[0] = (*frame).width * 4;

			Ok(Self {
				frame,
				format_context: av_format_context,
				video_codec_context,
				video_stream,
			})
		}

	}

	pub fn encode(&self, device_buffer: usize, time: std::time::Duration) -> Result<(), String> {
		let video_stream_index = 0;
		unsafe {
			(*self.frame).data[0] = device_buffer as *mut u8;
			(*self.frame).pts = (time.as_secs_f64() * ffmpeg_sys::AV_TIME_BASE as f64) as i64;
			// next_recording_time = std::time::Instant::now() + std::time::Duration::from_secs_f64(time_step);

			let res = ffmpeg_sys::avcodec_send_frame(self.video_codec_context, self.frame);
			if res >= 0 {
				receive_frames(
					self.video_codec_context,
					video_stream_index,
					self.video_stream,
					self.frame,
					self.format_context,
				)
					.map_err(|e| format!("Failed to encode image: {}", e))?;
			} else {
				eprintln!("Error: avcodec_send_frame failed: {}", get_error(res)?);
			}
		};

		Ok(())
	}

	pub fn stop(&self) -> Result<(), String> {
		unsafe {
			if ffmpeg_sys::av_write_trailer(self.format_context) != 0 {
				panic!("Failed to write trailer");
			}
			if ffmpeg_sys::avio_close((*self.format_context).pb) != 0 {
				panic!("Failed to close file");
			}
		};
		Ok(())
	}
}
