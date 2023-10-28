use std::sync::{Arc, Mutex};

use async_shutdown::ShutdownManager;
use ffmpeg::Frame;
use nvfbc::{CudaCapturer, BufferFormat, cuda::CaptureMethod};

use crate::cuda::check_ret;

pub struct FrameCapturer {
	capturer: CudaCapturer,
}

impl FrameCapturer {
	pub fn new() -> Result<Self, ()> {
		let capturer = CudaCapturer::new()
			.map_err(|e| log::error!("Failed to create CUDA capture device: {e}"))?;
		capturer.release_context()
			.map_err(|e| log::error!("Failed to release frame capturer CUDA context: {e}"))?;

		Ok(Self { capturer })
	}

	pub fn run(
		mut self,
		framerate: u32,
		mut capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		notifier: Arc<std::sync::Condvar>,
		stop_signal: ShutdownManager<()>,
	) -> Result<(), ()> {
		self.capturer.bind_context()
			.map_err(|e| log::error!("Failed to bind frame capturer CUDA context: {e}"))?;
		self.capturer.start(BufferFormat::Bgra, framerate)
			.map_err(|e| log::error!("Failed to start CUDA capture device: {e}"))?;
		log::info!("Started frame capture.");

		while !stop_signal.is_shutdown_triggered() {
			let frame_info = self.capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
				.map_err(|e| log::error!("Failed to wait for new CUDA frame: {e}"))?;
			log::trace!("Frame info: {:#?}", frame_info);

			// capture_buffer.as_raw_mut().data[0] = frame_info.device_buffer as *mut u8;
			unsafe {
				check_ret(ffmpeg_sys::cuMemcpy(
					capture_buffer.as_raw_mut().data[0] as u64,
					frame_info.device_buffer as u64,
					frame_info.device_buffer_len as usize,
				))
					.map_err(|e| println!("Failed to copy CUDA memory: {e}")).unwrap();
			}

			// Swap the intermediate buffer with the output buffer and signal that we have a new frame.
			// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
			{
				let mut lock = intermediate_buffer.lock()
					.map_err(|e| log::error!("Failed to lock intermediate buffer: {e}"))?;
				std::mem::swap(&mut *lock, &mut capture_buffer);
			}
			notifier.notify_one();
		}

		log::debug!("Received stop signal.");

		Ok(())
	}
}
