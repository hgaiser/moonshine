use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_shutdown::ShutdownManager;
use ffmpeg::Frame;
use nvfbc::{cuda::CaptureMethod, BufferFormat, CudaCapturer};

pub struct FrameCapturer {
	capturer: CudaCapturer,
}

impl FrameCapturer {
	pub fn new() -> Result<Self> {
		let capturer = CudaCapturer::new().context("Failed to create CUDA capture device")?;
		capturer
			.release_context()
			.context("Failed to release frame capturer CUDA context")?;

		Ok(Self { capturer })
	}

	pub fn status(&self) -> Result<nvfbc::Status> {
		self.capturer.status().context("Failed to get NvFBC status")
	}

	pub fn run(
		mut self,
		framerate: u32,
		mut capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		notifier: Arc<std::sync::Condvar>,
		stop_signal: ShutdownManager<()>,
	) -> Result<()> {
		self.capturer
			.bind_context()
			.context("Failed to bind frame capturer CUDA context")?;
		self.capturer
			.start(BufferFormat::Bgra, framerate)
			.context("Failed to start CUDA capture device")?;
		tracing::info!("Started frame capture.");

		while !stop_signal.is_shutdown_triggered() {
			let frame_info = self
				.capturer
				.next_frame(CaptureMethod::NoWaitIfNewFrame)
				.context("Failed to wait for new CUDA frame")?;
			tracing::trace!("Frame info: {:#?}", frame_info);

			// capture_buffer.as_raw_mut().data[0] = frame_info.device_buffer as *mut u8;
			unsafe {
				if let Err(e) = cudarc::driver::result::memcpy_dtod_sync(
					(*capture_buffer.as_mut_ptr()).data[0] as cudarc::driver::sys::CUdeviceptr,
					frame_info.device_buffer as cudarc::driver::sys::CUdeviceptr,
					frame_info.device_buffer_len as usize,
				) {
					tracing::error!("Failed to copy CUDA memory: {e}");
					continue;
				}
			}

			// Swap the intermediate buffer with the output buffer and signal that we have a new frame.
			// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
			{
				let mut lock = intermediate_buffer.lock().expect("Failed to lock intermediate buffer");
				std::mem::swap(&mut *lock, &mut capture_buffer);
			}
			notifier.notify_one();
		}

		tracing::debug!("Received stop signal.");

		Ok(())
	}
}
