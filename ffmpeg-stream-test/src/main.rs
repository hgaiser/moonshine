mod cuda;
mod error;

use std::ffi::{CStr, c_char};
use std::{ptr::null_mut, ffi::CString, mem::MaybeUninit};
use std::io::Write;

use nvfbc::{BufferFormat, CudaCapturer};
use nvfbc::cuda::CaptureMethod;

use error::FfmpegError;

fn to_c_str(data: &str) -> Result<CString, ()> {
	CString::new(data)
		.map_err(|e| println!("Failed to create CString: {e}"))
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

fn main() -> Result<(), ()> {
	let cuda_context = cuda::init_cuda(0)
		.map_err(|e| println!("Failed to initialize CUDA: {e}"))?;

	// Create a capturer that captures to CUDA context.
	let mut capturer = CudaCapturer::new()
		.map_err(|e| println!("Failed to create CUDA capture device: {e}"))?;

	let status = capturer.status()
		.map_err(|e| println!("Failed to get capturer status: {e}"))?;
	println!("{status:#?}");
	if !status.can_create_now {
		panic!("Can't create a CUDA capture session.");
	}

	let width = status.screen_size.w;
	let height = status.screen_size.h;
	let fps = 60;

	capturer.start(BufferFormat::Bgra, fps)
		.map_err(|e| println!("Failed to start frame capturer: {e}"))?;

	unsafe {
		// Init the codec used to encode our given image
		// let codec_id = ffmpeg_sys::AVCodecID_AV_CODEC_ID_MPEG4;

		let codec = ffmpeg_sys::avcodec_find_encoder_by_name(to_c_str("h264_nvenc")?.as_ptr());
		let codec_context = ffmpeg_sys::avcodec_alloc_context3(codec);

		(*codec_context).bit_rate      = 400000;
		(*codec_context).width         = width as i32;
		(*codec_context).height        = height as i32;

		(*codec_context).time_base.num = 1;
		(*codec_context).time_base.den = fps as i32;
		(*codec_context).gop_size      = fps as i32;
		(*codec_context).max_b_frames  = 1;
		// (*codec_context).pix_fmt       = ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P;
		(*codec_context).pix_fmt       = ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA;
		(*codec_context).codec_type    = ffmpeg_sys::AVMediaType_AVMEDIA_TYPE_VIDEO;

		// if codec_id == ffmpeg_sys::AVCodecID_AV_CODEC_ID_H264 {
			// ffmpeg_sys::av_opt_set((*codec_context).priv_data, to_c_str("preset").as_ptr(), to_c_str("ultrafast").as_ptr(), 0);
			// ffmpeg_sys::av_opt_set((*codec_context).priv_data, to_c_str("tune").as_ptr(), to_c_str("zerolatency").as_ptr(), 0);
		// }

		let device_ctx = ffmpeg_sys::av_hwdevice_ctx_alloc(ffmpeg_sys::AVHWDeviceType_AV_HWDEVICE_TYPE_CUDA);
		if device_ctx.is_null() {
			println!("Failed to create hardware device context.");
			return Err(());
		}

		let hw_device_context = (*device_ctx).data as *mut ffmpeg_sys::AVHWDeviceContext;
		let cuda_device_context = (*hw_device_context).hwctx as *mut ffmpeg_sys::AVCUDADeviceContext;
		(*cuda_device_context).cuda_ctx = cuda_context;
		check_ret(ffmpeg_sys::av_hwdevice_ctx_init(device_ctx))
			.map_err(|e| println!("Failed to initialize hardware device: {e}"))?;

		let frame_context = ffmpeg_sys::av_hwframe_ctx_alloc(device_ctx) as *mut ffmpeg_sys::AVBufferRef;
		if frame_context.is_null() {
			println!("Failed to create hwframe context.");
			return Err(());
		}

		let hw_frame_context = &mut *((*frame_context).data as *mut ffmpeg_sys::AVHWFramesContext);
		hw_frame_context.width = (*codec_context).width;
		hw_frame_context.height = (*codec_context).height;
		hw_frame_context.sw_format = ffmpeg_sys::AV_PIX_FMT_0RGB32;
		hw_frame_context.format = (*codec_context).pix_fmt;

		check_ret(ffmpeg_sys::av_hwframe_ctx_init(frame_context))
			.map_err(|e| println!("Failed to initialize hardware frame context: {e}"))?;

		(*codec_context).hw_frames_ctx = frame_context;

		ffmpeg_sys::avcodec_open2(codec_context, codec, null_mut());

		//Init the Frame containing our raw data
		let frame = ffmpeg_sys::av_frame_alloc();
		(*frame).format = (*codec_context).pix_fmt;
		(*frame).width  = (*codec_context).width;
		(*frame).height = (*codec_context).height;
		(*frame).hw_frames_ctx = (*codec_context).hw_frames_ctx;

		// TODO: Remove this, this shouldn't be necessary!
		// This allocates a HW frame, but we should manually create our own frame (through nvfbc).
		check_ret(ffmpeg_sys::av_hwframe_get_buffer((*frame).hw_frames_ctx, frame, 0))
			.map_err(|e| println!("Failed to allocate hardware frame: {e}"))?;
		(*frame).linesize[0] = (*frame).width * 4;
		// ffmpeg_sys::av_image_alloc((*frame).data.as_mut_ptr(), (*frame).linesize.as_mut_ptr(), (*frame).width, (*frame).height, (*codec_context).pix_fmt, 32);

		//Init the format context
		let mut format_context = ffmpeg_sys::avformat_alloc_context();
		let format = ffmpeg_sys::av_guess_format(to_c_str("rtp")?.as_ptr(), null_mut(), null_mut());
		ffmpeg_sys::avformat_alloc_output_context2(&mut format_context, format, (*format).name, to_c_str("rtp://127.0.0.1:49990")?.as_ptr());

		ffmpeg_sys::avio_open(&mut (*format_context).pb, to_c_str("rtp://127.0.0.1:49990")?.as_ptr(), ffmpeg_sys::AVIO_FLAG_WRITE as i32);

		//Configure the AVStream for the output format context
		let stream = ffmpeg_sys::avformat_new_stream(format_context, codec);

		ffmpeg_sys::avcodec_parameters_from_context((*stream).codecpar, codec_context);
		(*stream).time_base.num = 1;
		(*stream).time_base.den = fps as i32;

		std::thread::sleep(std::time::Duration::from_secs(5));

		// Rewrite the header.
		ffmpeg_sys::avformat_write_header(format_context, null_mut());

		// Write a file for VLC.
		let mut buf = [0u8; 1024];
		ffmpeg_sys::av_sdp_create(&mut format_context, 1, buf.as_mut_ptr() as *mut i8, buf.len() as i32);
		let mut w = std::fs::File::create("video.sdp").unwrap();
		w.write_all(&buf).unwrap();

		let mut packet: ffmpeg_sys::AVPacket = MaybeUninit::zeroed().assume_init();
		let mut j = 0;
		for i in 0..10000 {
			let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
				.map_err(|e| println!("Failed to capture frame: {e}"))?;
			(*frame).data[0] = frame_info.device_buffer as *mut u8;

			ffmpeg_sys::fflush(ffmpeg_sys::stdout);
			ffmpeg_sys::av_init_packet(&mut packet);
			packet.data = null_mut();    // packet data will be allocated by the encoder
			packet.size = 0;

			// let rgb = (
			// 	(i % 255) as f64,
			// 	(i % 255) as f64,
			// 	(i % 255) as f64,
			// );

			// let yuv = (
			// 	( 0.257 * rgb.0 + 0.504 * rgb.1 + 0.098 * rgb.2 +  16.0) as i32,
			// 	(-0.148 * rgb.0 - 0.291 * rgb.1 + 0.439 * rgb.2 + 128.0) as i32,
			// 	( 0.439 * rgb.0 - 0.368 * rgb.1 - 0.071 * rgb.2 + 128.0) as i32,
			// );

			// /* prepare a dummy image */
			// /* Y */
			// for y in 0..(*codec_context).height {
			// 	for x in 0..(*codec_context).width {
			// 		*((*frame).data[0].offset((y * (*codec_context).width + x) as isize) as *mut i32) = yuv.0;
			// 	}
			// }

			// for y in 0..(*codec_context).height/2 {
			// 	for x in 0..(*codec_context).width / 2 {
			// 		*((*frame).data[1].offset((y * (*frame).linesize[1] + x) as isize) as *mut i32) = yuv.1;
			// 		*((*frame).data[2].offset((y * (*frame).linesize[2] + x) as isize) as *mut i32) = yuv.2;
			// 	}
			// }

			/* Which frame is it ? */
			(*frame).pts = i;

			/* Send the frame to the codec */
			ffmpeg_sys::avcodec_send_frame(codec_context, frame);

			/* Use the data in the codec to the AVPacket */
			let ret = ffmpeg_sys::avcodec_receive_packet(codec_context, &mut packet);
			if ret == ffmpeg_sys::AVERROR_EOF {
				println!("Stream EOF");
			} else if ret == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
				println!("Stream EAGAIN");
			} else {
				println!("Write frame {} (size={})", j, packet.size);
				j += 1;

				/* Write the data on the packet to the output format  */
				ffmpeg_sys::av_packet_rescale_ts(&mut packet, (*codec_context).time_base, (*stream).time_base);
				ffmpeg_sys::av_interleaved_write_frame(format_context, &mut packet);

				/* Reset the packet */
				ffmpeg_sys::av_packet_unref(&mut packet);
			}

			std::thread::sleep(std::time::Duration::from_micros(1_000_000 / fps as u64));
		}

		// end
		ffmpeg_sys::avcodec_send_frame(codec_context, null_mut());

		//Free everything
		ffmpeg_sys::av_free(codec_context as *mut _);
		ffmpeg_sys::av_free(format_context as *mut _);
	}

	Ok(())
}
