#![feature(cstr_from_bytes_until_nul)]

use nvfbc::{BufferFormat, CudaCapturer};
use nvfbc::cuda::CaptureMethod;

use crate::encoder::{NvencEncoder, CodecType, VideoQuality};

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
	let fps = 60;

	capturer.start(BufferFormat::Bgra, fps)?;

	let mut encoder = NvencEncoder::new(
		width,
		height,
		CodecType::H264,
		VideoQuality::Slowest,
		cuda_context,
	)?;

	let start_time = std::time::Instant::now();
	while start_time.elapsed().as_secs() < 2 {
		let start = std::time::Instant::now();
		let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)?;
		encoder.encode(frame_info.device_buffer, start_time.elapsed())?;
		println!("Capture: {}msec", start.elapsed().as_millis());
	}

	encoder.stop()?;

	Ok(())
}
