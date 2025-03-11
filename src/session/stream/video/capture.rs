use std::{sync::{atomic::{AtomicBool, AtomicU32, Ordering}, Arc, Condvar, Mutex}, thread::JoinHandle};

use async_shutdown::TriggerShutdownToken;
use cudarc::driver::CudaDevice;
use ffmpeg::Frame;
use nvfbc::{CudaCapturer, BufferFormat, cuda::CaptureMethod};

pub struct VideoFrameCapturer {
	stop_flag: Arc<AtomicBool>,
	inner_handle: JoinHandle<()>,
}

impl VideoFrameCapturer {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub fn new(
		capturer: CudaCapturer,
		capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		cuda_device: Arc<CudaDevice>,
		framerate: u32,
		frame_number: Arc<AtomicU32>,
		frame_notifier: Arc<Condvar>,
		session_stop_token: TriggerShutdownToken<()>,
	) -> Result<Self, ()> {
		tracing::info!("Starting frame capture.");

		let inner = FrameCaptureInner { capturer };
		let stop_flag = Arc::new(AtomicBool::new(false));
		let inner_handle = std::thread::Builder::new().name("video-capture".to_string()).spawn({
			let stop_flag = stop_flag.clone();
			move || {
				let _ = cuda_device.bind_to_thread()
					.map_err(|e| tracing::error!("Failed to bind CUDA device to thread: {e}"));
				inner.run(
					framerate,
					capture_buffer,
					intermediate_buffer,
					frame_number,
					frame_notifier,
					stop_flag,
					session_stop_token,
				);
			}
		})
			.map_err(|e| tracing::error!("Failed to start video capture thread: {e}"))?;

		Ok(Self { stop_flag, inner_handle })
	}

	pub async fn stop(self) -> Result<(), ()> {
		tracing::info!("Requesting frame capture to stop.");
		self.stop_flag.store(true, Ordering::Relaxed);
		self.inner_handle.join()
			.map_err(|_| tracing::error!("Failed to join audio capture thread."))?;
		Ok(())
	}
}

struct FrameCaptureInner {
	capturer: CudaCapturer,
}

impl FrameCaptureInner {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub fn run(
		mut self,
		framerate: u32,
		mut capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		frame_number: Arc<std::sync::atomic::AtomicU32>,
		frame_notifier: Arc<Condvar>,
		stop_flag: Arc<AtomicBool>,
		session_stop_token: TriggerShutdownToken<()>,
	) {
		if let Err(e) = self.capturer.bind_context() {
			tracing::error!("Failed to bind frame capturer CUDA context: {e}");
			return;
		}
		if let Err(e) = self.capturer.start(BufferFormat::Bgra, framerate) {
			tracing::error!("Failed to start CUDA capture device: {e}");
			return;
		}
		tracing::info!("Started frame capture.");

		while !stop_flag.load(Ordering::Relaxed) {
			let frame_info = match self.capturer.next_frame(CaptureMethod::NoWaitIfNewFrame) {
				Ok(frame_info) => frame_info,
				Err(e) => {
					tracing::warn!("Failed to wait for new CUDA frame: {e}");
					continue;
				},
			};
			tracing::trace!("Frame info: {:#?}", frame_info);

			unsafe {
				if (*capture_buffer.as_ptr()).width != frame_info.width as i32 || (*capture_buffer.as_ptr()).height != frame_info.height as i32 {
					// TODO: Implement scaling?
					tracing::warn!("Frame size mismatch, expected ({}, {}), got ({}, {}).", (*capture_buffer.as_ptr()).width, (*capture_buffer.as_ptr()).height, frame_info.width, frame_info.height);
					continue;
				}

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
				let mut lock = match intermediate_buffer.lock() {
					Ok(lock) => lock,
					Err(e) => {
						tracing::error!("Failed to lock intermediate buffer: {e}");
						continue;
					},
				};
				std::mem::swap(&mut *lock, &mut capture_buffer);
			}

			tracing::trace!("Current frame: {}", frame_info.current_frame);
			frame_number.store(frame_info.current_frame, Ordering::Relaxed);
			frame_notifier.notify_all();
		}

		// If we were asked to stop, ignore the stop token, no need to panic.
		if stop_flag.load(Ordering::Relaxed) {
			session_stop_token.forget();
		}

		tracing::info!("Video capturer stopped.");
	}
}
