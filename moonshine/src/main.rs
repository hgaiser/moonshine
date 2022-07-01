// use std::{mem::ManuallyDrop};
use std::{fs::File, error::Error, str::FromStr, slice::from_raw_parts, ffi::CStr, ptr::{null, null_mut}, intrinsics::transmute};

use moonshine_ffmpeg::{AVFormatContext, VideoQuality_HIGH, AVBufferRef, CUgraphicsResource, CUctx_st};
use nvfbc::{SystemCapturer, BufferFormat, CudaCapturer};
use rustacuda::{CudaFlags, device::Device, context::{Context, ContextFlags}};
// use image::{Rgb, ImageBuffer};
// use nvfbc::{BufferFormat, CudaCapturer};
// use rustacuda::{
// 	CudaFlags,
// 	device::Device,
// };
// use rustacuda::context::{Context, ContextFlags};
// use rustacuda::prelude::{DeviceBuffer, CopyDestination};
// use rustacuda::memory::LockedBuffer;
// use rustacuda_core::DevicePointer;


// enum VideoQuality {
// 	High,
// 	Medium,
// 	Low,
// }

// struct VideoFrame {
// 	raw: *mut AVFrame,
// }

// impl VideoFrame {
// 	fn new(width: u32, height: u32) -> Result<Self, String> {
// 		unsafe {
// 			let pixel_format = av_get_pix_fmt(
// 				CStr::from_bytes_with_nul(b"rgb24\0").map_err(|e| format!("failed to create pixel format cstr: {}", e))?.as_ptr()
// 			);
// 			let frame = av_frame_alloc();

// 			(*frame).format = pixel_format as i32;
// 			(*frame).width = width as i32;
// 			(*frame).height = height as i32;
// 			// (*frame).pts = (start_time.elapsed().as_secs_f64() * AV_TIME_BASE as f64) as i64;

// 			let res = av_frame_get_buffer(frame, 0);
// 			if res != 0 {
// 				panic!("Failed to allocate a frame buffer.");
// 			}

// 			av_image_fill_black(
// 				(*frame).data.as_mut_ptr(),
// 				(*frame).linesize.as_ptr() as *const isize,
// 				pixel_format,
// 				ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG,
// 				(*frame).width,
// 				(*frame).height
// 			);

// 			Ok(Self { raw: frame })
// 		}
// 	}
// }

// impl Drop for VideoFrame {
// 	fn drop(&mut self) {
// 		unsafe {
// 			av_frame_free(&mut self.raw);
// 		};
// 	}
// }

//fn create_video_codec_context(
//	av_format_context: *mut AVFormatContext,
//	video_quality: VideoQuality,
//	width: u32,
//	height: u32,
//	fps: i32,
//	use_hevc: bool,
//) -> Result<(), String> {
//	unsafe {
//		let codec = match use_hevc {
//			true => avcodec_find_encoder_by_name(CStr::from_bytes_with_nul(b"hevc_nvenc\0").map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()),
//			false => avcodec_find_encoder_by_name(CStr::from_bytes_with_nul(b"h264_nvenc\0").map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()),
//		};

//		let mut codec_context: *mut AVCodecContext = avcodec_alloc_context3(codec);

//		assert!((*codec_context).codec_type == AVMediaType::AVMEDIA_TYPE_VIDEO);

//		(*codec_context).codec_id = (*codec).id;
//		(*codec_context).width = (width & !1) as i32;
//		(*codec_context).height = (height & !1) as i32;
//		(*codec_context).bit_rate = 12500000 + (((*codec_context).width * (*codec_context).height) / 2) as i64;
//		// Timebase: This is the fundamental unit of time (in seconds) in terms
//		// of which frame timestamps are represented. For fixed-fps content,
//		// timebase should be 1/framerate and timestamp increments should be
//		// identical to 1
//		(*codec_context).time_base.num = 1;
//		(*codec_context).time_base.den = AV_TIME_BASE;
//		(*codec_context).framerate.num = fps;
//		(*codec_context).framerate.den = 1;
//		(*codec_context).sample_aspect_ratio.num = 0;
//		(*codec_context).sample_aspect_ratio.den = 0;
//		(*codec_context).gop_size = fps * 2;
//		(*codec_context).max_b_frames = 0;
//		(*codec_context).pix_fmt = AVPixelFormat::AV_PIX_FMT_CUDA;
//		(*codec_context).color_range = AVColorRange::AVCOL_RANGE_JPEG;

//		match video_quality {
//			VideoQuality::Low => {
//				(*codec_context).bit_rate = 10000000 + (((*codec_context).width * (*codec_context).height) / 2) as i64;
//				if use_hevc {
//					(*codec_context).qmin = 20;
//					(*codec_context).qmax = 35;
//				} else {
//					(*codec_context).qmin = 5;
//					(*codec_context).qmax = 20;
//				 }
//				 //av_opt_set((*codec_context).priv_data, "preset", "slow", 0);
//				 //av_opt_set((*codec_context).priv_data, "profile", "high", 0);
//				 //(*codec_context).profile = FF_PROFILE_H264_HIGH;
//				 //av_opt_set((*codec_context).priv_data, "preset", "p4", 0);
//			},
//			VideoQuality::Medium => {
//				if use_hevc {
//					(*codec_context).qmin = 17;
//					(*codec_context).qmax = 30;
//				} else {
//					(*codec_context).qmin = 5;
//					(*codec_context).qmax = 15;
//				}
//				//av_opt_set((*codec_context).priv_data, "preset", "slow", 0);
//				//av_opt_set((*codec_context).priv_data, "profile", "high", 0);
//				//(*codec_context).profile = FF_PROFILE_H264_HIGH;
//				//av_opt_set((*codec_context).priv_data, "preset", "p5", 0);
//			},
//			VideoQuality::High => {
//				(*codec_context).bit_rate = 15000000 + (((*codec_context).width * (*codec_context).height) / 2) as i64;
//				if use_hevc {
//					(*codec_context).qmin = 16;
//					(*codec_context).qmax = 25;
//				} else {
//					(*codec_context).qmin = 3;
//					(*codec_context).qmax = 13;
//				}
//				//av_opt_set((*codec_context).priv_data, "preset", "veryslow", 0);
//				//av_opt_set((*codec_context).priv_data, "profile", "high", 0);
//				//(*codec_context).profile = FF_PROFILE_H264_HIGH;
//				//av_opt_set((*codec_context).priv_data, "preset", "p7", 0);
//			}
//		};
//		if (*codec_context).codec_id == AVCodecID::AV_CODEC_ID_MPEG1VIDEO {
//			(*codec_context).mb_decision = 2;
//		}

//		// Some formats want stream headers to be seperate
//		if ((*(*av_format_context).oformat).flags & AVFMT_GLOBALHEADER) != 0 {
//			(*av_format_context).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
//		}
//	};

//	Ok(())
//}

// static void open_video(AVCodecContext *codec_context,
//                        WindowPixmap &window_pixmap, AVBufferRef **device_ctx,
//                        CUgraphicsResource *cuda_graphics_resource, CUcontext cuda_context) {
// fn open_video(
// 	codec_context: *const AVCodecContext,
// 	device_ctx: *mut *mut AVBufferRef,
// 	// CUgraphicsResource *cuda_graphics_resource, CUcontext cuda_context
// ) -> Result<(), String> {
// 	unsafe {
// 		*device_ctx = av_hwdevice_ctx_alloc(AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA);
// 		if (*device_ctx).is_null() {
// 			return Err("Failed to create hardware device context.".into());
// 		}

// 		let hw_device_context: *mut AVHWDeviceContext = (*(*device_ctx)).data as *mut AVHWDeviceContext;
// 		let cuda_device_context: *mut AVCUDADeviceContext = (*hw_device_context).hwctx as *mut AVCUDADeviceContext;
// 		cuda_device_context->cuda_ctx = cuda_context;
// 		if(av_hwdevice_ctx_init(*device_ctx) < 0) {
// 			fprintf(stderr, "Error: Failed to create hardware device context\n");
// 			exit(1);
// 		}

// 		AVBufferRef *frame_context = av_hwframe_ctx_alloc(*device_ctx);
// 		if (!frame_context) {
// 			fprintf(stderr, "Error: Failed to create hwframe context\n");
// 			exit(1);
// 		}

// 		AVHWFramesContext *hw_frame_context =
// 			(AVHWFramesContext *)frame_context->data;
// 		hw_frame_context->width = codec_context->width;
// 		hw_frame_context->height = codec_context->height;
// 		hw_frame_context->sw_format = AV_PIX_FMT_0RGB32;
// 		hw_frame_context->format = codec_context->pix_fmt;
// 		hw_frame_context->device_ref = *device_ctx;
// 		hw_frame_context->device_ctx = (AVHWDeviceContext *)(*device_ctx)->data;

// 		if (av_hwframe_ctx_init(frame_context) < 0) {
// 			fprintf(stderr, "Error: Failed to initialize hardware frame context "
// 				"(note: ffmpeg version needs to be > 4.0\n");
// 			exit(1);
// 		}

// 		codec_context->hw_device_ctx = *device_ctx;
// 		codec_context->hw_frames_ctx = frame_context;

// 		ret = avcodec_open2(codec_context, codec_context->codec, nullptr);
// 		if (ret < 0) {
// 			fprintf(stderr, "Error: Could not open video codec: %s\n",
// 				"blabla"); // av_err2str(ret));
// 			exit(1);
// 		}

// 		if(window_pixmap.target_texture_id != 0) {
// 			CUresult res;
// 			CUcontext old_ctx;
// 			res = cuCtxPopCurrent(&old_ctx);
// 			res = cuCtxPushCurrent(cuda_context);
// 			res = cuGraphicsGLRegisterImage(
// 				cuda_graphics_resource, window_pixmap.target_texture_id, GL_TEXTURE_2D,
// 				CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY);
// 			// cuGraphicsUnregisterResource(*cuda_graphics_resource);
// 			if (res != CUDA_SUCCESS) {
// 				const char *err_str;
// 				cuGetErrorString(res, &err_str);
// 				fprintf(stderr,
// 					"Error: cuGraphicsGLRegisterImage failed, error %s, texture "
// 					"id: %u\n",
// 					err_str, window_pixmap.target_texture_id);
// 				exit(1);
// 			}
// 			res = cuCtxPopCurrent(&old_ctx);
// 		}
// 	};

// 	Ok(())
// }

/// Open a given output file.
// fn open_output(path: &str, elementary_streams: &[CodecParameters]) -> Result<Muxer<File>, ac_ffmpeg::Error> {
// 	let output_format = OutputFormat::guess_from_file_name(path)
// 		.ok_or_else(|| ac_ffmpeg::Error::new(format!("unable to guess output format for file: {}", path)))?;

// 	let output = File::create(path)
// 		.map_err(|err| ac_ffmpeg::Error::new(format!("unable to create output file {}: {}", path, err)))?;

// 	let io = IO::from_seekable_write_stream(output);

// 	let mut muxer_builder = Muxer::builder();

// 	for codec_parameters in elementary_streams {
// 		muxer_builder.add_stream(codec_parameters)?;
// 	}

// 	muxer_builder.build(io, output_format)
// }

fn main() -> Result<(), Box<dyn Error>> {
	// Initialize the CUDA API`
	rustacuda::init(CudaFlags::empty())?;

	// Get the first device
	let device = Device::get_device(0)?;

	// Create a context associated to this device
	let mut context = Context::create_and_push(
		ContextFlags::MAP_HOST | ContextFlags::SCHED_AUTO, device)?;

	let mut capturer = CudaCapturer::new()?;

	let status = capturer.status()?;
	println!("{:#?}", capturer.status()?);
	if !status.can_create_now {
		panic!("Can't create a system capture session.");
	}

	let width = status.screen_size.w;
	let height = status.screen_size.h;
	let fps = 30;
	let use_hevc = true;

	capturer.start(BufferFormat::Rgb)?;

	let frame_info = capturer.next_frame()?;
	println!("{:#?}", frame_info);


	let start_time = std::time::Instant::now();
	let time_step = 1.0 / 30.0;

	// let frame = VideoFrame::new(
	// 	width,
	// 	height
	// )?;

	unsafe {
		let mut av_format_context: *mut AVFormatContext = null_mut();
		let res = moonshine_ffmpeg::avformat_alloc_output_context2(
			&mut av_format_context,
			null(), null(),
			CStr::from_bytes_with_nul(b"test.mp4\0").map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()
		);
		if res < 0 {
			panic!("Failed to create output format context: {}", res);
		}

		let video_codec_context = moonshine_ffmpeg::create_video_codec_context(
			av_format_context,
			VideoQuality_HIGH,
			width,
			height,
			fps,
			use_hevc
		);

		let mut device_ctx: *mut AVBufferRef = null_mut();
		let mut cuda_graphics_resource: *mut CUgraphicsResource = null_mut();
		moonshine_ffmpeg::open_video(
			video_codec_context,
			&mut device_ctx,
			cuda_graphics_resource,
			&mut context as *mut _ as *mut CUctx_st
		);
	};





	// let width = 3440;
	// let height = 1440;
	// let duration = std::time::Duration::from_secs(5);

	// // note: it is 1/fps
	// let time_base = TimeBase::new(1, 25);

	// let pixel_format = PixelFormat::from_str("rgb24")?;

	// // create a black video frame with a given resolution
	// let frame = VideoFrameMut::black(pixel_format, width as _, height as _)
	// 	.with_time_base(time_base);

	// let mut encoder = VideoEncoder::builder("libx264")?
	// 	.pixel_format(pixel_format)
	// 	.width(width as _)
	// 	.height(height as _)
	// 	.time_base(time_base)
	// 	.build()?;

	// let codec_parameters = encoder.codec_parameters().into();

	// let mut muxer = open_output("test.mp4", &[codec_parameters])?;

	// let mut frame_idx = 0;
	// let mut frame_timestamp = Timestamp::new(frame_idx, time_base);
	// let max_timestamp = Timestamp::from_millis(0) + duration;

	// while frame_timestamp < max_timestamp {
	// 	let cloned_frame = frame.clone().with_pts(frame_timestamp);

	// 	encoder.push(cloned_frame)?;

	// 	while let Some(packet) = encoder.take()? {
	// 		muxer.push(packet.with_stream_index(0))?;
	// 	}

	// 	frame_idx += 1;
	// 	frame_timestamp = Timestamp::new(frame_idx, time_base);
	// }

	// encoder.flush()?;

	// while let Some(packet) = encoder.take()? {
	// 	muxer.push(packet.with_stream_index(0))?;
	// }

	// muxer.flush()?;
	Ok(())


	// // Initialize the CUDA API`
	// rustacuda::init(CudaFlags::empty())?;

	// // Get the first device
	// let device = Device::get_device(0)?;

	// // Create a context associated to this device
	// let _context = Context::create_and_push(
	// 	ContextFlags::MAP_HOST | ContextFlags::SCHED_AUTO, device)?;

	// // Create a capturer that captures to CUDA context.
	// let mut capturer = CudaCapturer::new()?;

	// let status = capturer.status()?;
	// println!("get_status: {:#?}", status);
	// if !status.can_create_now {
	// 	panic!("Can't create a CUDA capture session.");
	// }

	// capturer.start(BufferFormat::Rgb)?;

	// let frame_info = capturer.next_frame()?;
	// println!("{:#?}", frame_info);

	// // Wrap the buffer in GPU memory.
	// let device_buffer = ManuallyDrop::new(unsafe { DeviceBuffer::from_raw_parts(
	// 	DevicePointer::wrap(frame_info.device_buffer as *mut u8),
	// 	frame_info.device_buffer_len as usize,
	// ) });

	// // Create a page locked buffer to avoid unnecessary copying.
	// // See https://docs.rs/rustacuda/latest/rustacuda/memory/index.html#page-locked-host-memory for more information.
	// let mut data: LockedBuffer<u8> = unsafe { LockedBuffer::uninitialized(frame_info.device_buffer_len as usize) }?;

	// // Copy device memory to host memory and wrap it as an image.
	// device_buffer.copy_to(&mut data)?;
	// let slice = data.as_slice();
	// let frame = ImageBuffer::<Rgb<u8>, &[u8]>::from_raw(frame_info.width, frame_info.height, slice).unwrap();
	// frame.save("frame.png")?;

	// capturer.stop()?;

	// Ok(())
}
