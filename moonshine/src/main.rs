use std::error::Error;

use nvfbc::{BufferFormat, CudaCapturer};
use rustacuda::{CudaFlags, device::Device, context::{Context, ContextFlags}, prelude::{DeviceBuffer, CopyDestination}};
use rustacuda_core::DevicePointer;

fn main() -> Result<(), Box<dyn Error>> {
	let mut capturer = CudaCapturer::new()?;
	let status = capturer.status()?;
	println!("get_status: {:#?}", status);

	if !status.can_create_now {
		panic!("Can't create a capture session.");
	}

	// Initialize the CUDA API
	rustacuda::init(CudaFlags::empty())?;

	// Get the first device
	let device = Device::get_device(0)?;

	// Create a context associated to this device
	let _context = Context::create_and_push(
		ContextFlags::MAP_HOST | ContextFlags::SCHED_AUTO, device)?;

	capturer.start(BufferFormat::Rgb)?;

	let frame_info = capturer.next_frame()?;
	println!("frame_info: {:#?}", frame_info);

	// Wrap the GPU memory.
	let device_buffer = unsafe { DeviceBuffer::from_raw_parts(
		DevicePointer::wrap(frame_info.device_buffer as *mut u8),
		frame_info.byte_size as usize,
	) };

	// Create system memory buffer.
	let mut frame = image::RgbImage::new(frame_info.width, frame_info.height);
	device_buffer.copy_to(frame.as_mut())?;
	frame.save("/home/hgaiser/frame.png")?;

	std::mem::forget(device_buffer);

	capturer.stop()?;

	println!("Done!");

	Ok(())
}
