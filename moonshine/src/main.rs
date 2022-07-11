#![feature(cstr_from_bytes_until_nul)]

use std::{ffi::CStr, ptr::{null, null_mut}};

use ffmpeg_sys::{AVFormatContext, VideoQuality_HIGH, AVBufferRef, CUgraphicsResource, av_log_set_level, AV_LOG_QUIET};
use nvfbc::{BufferFormat, CudaCapturer};
use nvfbc::cuda::CaptureMethod;

use crate::encoder::{NvencEncoder, Codec};

mod cuda;
mod encoder;
mod error;

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
	let video_stream_index = 0;

	capturer.start(BufferFormat::Bgra, fps)?;

	let encoder = NvencEncoder::new(cuda_context, Codec::H264, width, height, fps)?;

	let start_time = std::time::Instant::now();
	let time_step = 1.0 / fps as f64;

	while start_time.elapsed().as_secs() < 2 {
		let start = std::time::Instant::now();
		let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)?;

		encoder.encode(frame_info.device_buffer, start_time.elapsed())?;

		println!("Capture: {}msec", start.elapsed().as_millis());
	}

	encoder.stop();

	Ok(())
}
