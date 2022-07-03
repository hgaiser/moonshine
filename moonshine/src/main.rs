#![feature(cstr_from_bytes_until_nul)]

use std::{ffi::CStr, ptr::{null, null_mut}};

use ffmpeg_sys::{AVFormatContext, VideoQuality_HIGH, AVBufferRef, CUgraphicsResource, av_log_set_level, AV_LOG_QUIET};
use nvfbc::{BufferFormat, CudaCapturer};
use nvfbc::cuda::CaptureMethod;

mod cuda;
mod error;

fn get_ffmpeg_error(error_code: i32) -> Result<String, String> {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let cuda_context = cuda::init_cuda(0)?;

	// Create a capturer that captures to CUDA context.
	let mut capturer = CudaCapturer::new()?;

	let status = capturer.status()?;
	println!("{:#?}", status);
	if !status.can_create_now {
		panic!("Can't create a CUDA capture session.");
	}

	let width = status.screen_size.w;
	let height = status.screen_size.h;
	let fps = 30;
	let use_hevc = false;
	let filename = "test.mp4";
	let video_stream_index = 0;

	capturer.start(BufferFormat::Bgra, fps)?;

	unsafe {
		av_log_set_level(AV_LOG_QUIET);
		let mut av_format_context: *mut AVFormatContext = null_mut();
		let res = ffmpeg_sys::avformat_alloc_output_context2(
			&mut av_format_context,
			null(), null(),
			CStr::from_bytes_with_nul(b"test.mp4\0").map_err(|e| format!("failed to create output filename cstr: {}", e))?.as_ptr()
		);
		if res < 0 {
			panic!("Failed to create output format context: {}", res);
		}

		let video_codec_context = ffmpeg_sys::create_video_codec_context(
			av_format_context,
			VideoQuality_HIGH,
			width,
			height,
			fps,
			use_hevc
		);

		let video_stream = ffmpeg_sys::create_stream(av_format_context, video_codec_context);

		let mut device_ctx: *mut AVBufferRef = null_mut();
		let mut cuda_graphics_resource: CUgraphicsResource = null_mut();
		ffmpeg_sys::open_video(
			video_codec_context,
			&mut device_ctx,
			&mut cuda_graphics_resource,
			cuda_context,
		);

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
			panic!("Could not open '{}': {}", filename, get_ffmpeg_error(res)?);
		}

		let res = ffmpeg_sys::avformat_write_header(av_format_context, null_mut());
		if res < 0 {
			panic!("Error occurred when writing header to output file: {}", get_ffmpeg_error(res)?);
		}

		let frame: *mut ffmpeg_sys::AVFrame = ffmpeg_sys::av_frame_alloc();
		if frame.is_null() {
			panic!("Failed to allocate frame");
		}
		(*frame).format = (*video_codec_context).pix_fmt;
		(*frame).width = (*video_codec_context).width;
		(*frame).height = (*video_codec_context).height;

		if ffmpeg_sys::av_hwframe_get_buffer((*video_codec_context).hw_frames_ctx, frame, 0) < 0 {
			panic!("Failed to allocate hardware buffer");
		}

		(*frame).width = (width & !1) as i32;
		(*frame).height = (height & !1) as i32;

		let start_time = std::time::Instant::now();
		let time_step = 1.0 / fps as f64;

		while start_time.elapsed().as_secs() < 2 {
			let start = std::time::Instant::now();
			let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)?;
			(*frame).data[0] = frame_info.device_buffer as *mut u8;
			(*frame).pts = (start_time.elapsed().as_secs_f64() * ffmpeg_sys::AV_TIME_BASE as f64) as i64;
			// next_recording_time = std::time::Instant::now() + std::time::Duration::from_secs_f64(time_step);

			let res = ffmpeg_sys::avcodec_send_frame(video_codec_context, frame);
			if res >= 0 {
				ffmpeg_sys::receive_frames(
					video_codec_context,
					video_stream_index,
					video_stream,
					frame,
					av_format_context,
				// 	write_output_mutex
				);
			} else {
				eprintln!("Error: avcodec_send_frame failed: {}", get_ffmpeg_error(res)?);
			}

			println!("Capture: {}msec", start.elapsed().as_millis());
		}

		if ffmpeg_sys::av_write_trailer(av_format_context) != 0 {
			panic!("Failed to write trailer");
		}
		if ffmpeg_sys::avio_close((*av_format_context).pb) != 0 {
			panic!("Failed to close file");
		}
	};

	Ok(())
}
