use std::sync::{atomic::Ordering, Arc, Mutex};

use async_shutdown::ShutdownManager;
use ffmpeg::Frame;
use nvfbc::{CudaCapturer, BufferFormat, cuda::CaptureMethod};


pub struct FrameCapturer {
	capturer: CudaCapturer,
}

impl FrameCapturer {
	pub fn new() -> Result<Self, ()> {
		let capturer = CudaCapturer::new()
			.map_err(|e| tracing::error!("Failed to create CUDA capture device: {e}"))?;
		capturer.release_context()
			.map_err(|e| tracing::error!("Failed to release frame capturer CUDA context: {e}"))?;

		Ok(Self { capturer })
	}

	pub fn status(&self) -> Result<nvfbc::Status, ()>{
		self.capturer.status()
			.map_err(|e| tracing::error!("Failed to get NvFBC status: {e}"))
	}

	pub fn run(
		mut self,
		framerate: u32,
		mut capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		frame_number: Arc<std::sync::atomic::AtomicU32>,
		frame_notifier: Arc<std::sync::Condvar>,
		stop_signal: ShutdownManager<()>,
	) -> Result<(), ()> {
		self.capturer.bind_context()
			.map_err(|e| tracing::error!("Failed to bind frame capturer CUDA context: {e}"))?;
		self.capturer.start(BufferFormat::Bgra, framerate)
			.map_err(|e| tracing::error!("Failed to start CUDA capture device: {e}"))?;
		tracing::info!("Started frame capture.");

		while !stop_signal.is_shutdown_triggered() {
			let frame_info = self.capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
				.map_err(|e| tracing::error!("Failed to wait for new CUDA frame: {e}"))?;
			tracing::trace!("Frame info: {:#?}", frame_info);

			unsafe {
				if let Err(e) = cudarc::driver::result::memcpy_dtod_sync(
					(*capture_buffer.as_mut_ptr()).data[0] as cudarc::driver::sys::CUdeviceptr,
					frame_info.device_buffer as cudarc::driver::sys::CUdeviceptr,
					frame_info.device_buffer_len as usize
				) {
					tracing::error!("Failed to copy CUDA memory: {e}");
					continue;
				}
			}

			// Swap the intermediate buffer with the output buffer and signal that we have a new frame.
			// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
			{
				let mut lock = intermediate_buffer.lock()
					.map_err(|e| tracing::error!("Failed to lock intermediate buffer: {e}"))?;
				std::mem::swap(&mut *lock, &mut capture_buffer);
			}

			tracing::trace!("Current frame: {}", frame_info.current_frame);
			frame_number.store(frame_info.current_frame, Ordering::Relaxed);
			frame_notifier.notify_all();
		}

		tracing::debug!("Received stop signal.");

		Ok(())
	}
}
