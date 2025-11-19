use std::sync::{atomic::{AtomicU32, Ordering}, Arc, Condvar, Mutex};

use async_shutdown::ShutdownManager;
use cudarc::driver::CudaContext;
use ffmpeg::Frame;
use gst::prelude::*;

use crate::session::manager::SessionShutdownReason;

pub struct VideoFrameCapturer { }

impl VideoFrameCapturer {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub fn new(
		node_id: u32,
		capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		cuda_context: Arc<CudaContext>,
		framerate: u32,
		frame_number: Arc<AtomicU32>,
		frame_notifier: Arc<Condvar>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing frame capture.");

		let inner = FrameCaptureInner { node_id };
		std::thread::Builder::new().name("video-capture".to_string()).spawn(
			move || {
				let _ = cuda_context.bind_to_thread()
					.map_err(|e| tracing::error!("Failed to bind CUDA device to thread: {e}"));
				inner.run(
					framerate,
					capture_buffer,
					intermediate_buffer,
					frame_number,
					frame_notifier,
					stop_session_manager,
				);
			}
		)
			.map_err(|e| tracing::error!("Failed to start frame capture thread: {e}"))?;

		Ok(Self { })
	}
}

struct FrameCaptureInner {
	node_id: u32,
}

impl FrameCaptureInner {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub fn run(
		self,
		framerate: u32,
		mut capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		frame_number: Arc<std::sync::atomic::AtomicU32>,
		frame_notifier: Arc<Condvar>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		tracing::debug!("Starting frame capture.");

		// Trigger session shutdown if we exit unexpectedly.
		let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoFrameCaptureStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		if let Err(e) = gst::init() {
			tracing::error!("Failed to initialize GStreamer: {e}");
			return;
		}

		let width = unsafe { (*capture_buffer.as_ptr()).width };
		let height = unsafe { (*capture_buffer.as_ptr()).height };

		let pipeline_str = format!(
			"pipewiresrc path={} ! queue max-size-buffers=1 leaky=downstream ! videoscale ! videoconvert ! videorate drop-only=true ! video/x-raw,format=BGRA,width={},height={},framerate={}/1 ! appsink name=sink sync=false",
			self.node_id, width, height, framerate
		);

		let pipeline = match gst::parse::launch(&pipeline_str) {
			Ok(pipeline) => pipeline,
			Err(e) => {
				tracing::error!("Failed to parse GStreamer pipeline: {e}");
				return;
			}
		};

		let pipeline = match pipeline.dynamic_cast::<gst::Pipeline>() {
			Ok(pipeline) => pipeline,
			Err(_) => {
				tracing::error!("Failed to cast to GStreamer pipeline.");
				return;
			}
		};

		let sink = match pipeline.by_name("sink") {
			Some(sink) => sink,
			None => {
				tracing::error!("Failed to find sink element in pipeline.");
				return;
			}
		};

		let sink = match sink.dynamic_cast::<gst_app::AppSink>() {
			Ok(sink) => sink,
			Err(_) => {
				tracing::error!("Failed to cast sink to AppSink.");
				return;
			}
		};

		// Configure appsink
		sink.set_caps(Some(&gst::Caps::builder("video/x-raw")
			.field("format", "BGRA")
			.build()));
		sink.set_max_buffers(1);
		sink.set_drop(true);

		if let Err(e) = pipeline.set_state(gst::State::Playing) {
			tracing::error!("Failed to set pipeline state to Playing: {e}");
			return;
		}
		tracing::info!("GStreamer pipeline started playing.");

		let mut current_frame = 0;

		while !stop_session_manager.is_shutdown_triggered() {
			let sample = match sink.try_pull_sample(gst::ClockTime::from_mseconds(1000)) {
				Some(sample) => sample,
				None => {
					if sink.is_eos() {
						tracing::info!("GStreamer pipeline EOS.");
						break;
					}
					// Timeout, check shutdown and continue
					continue;
				}
			};

			let buffer = match sample.buffer() {
				Some(buffer) => buffer,
				None => {
					tracing::warn!("Received sample without buffer.");
					continue;
				}
			};

			let map = match buffer.map_readable() {
				Ok(map) => map,
				Err(e) => {
					tracing::error!("Failed to map buffer readable: {e}");
					continue;
				}
			};

			let data = map.as_slice();

			unsafe {
				// TODO: Check frame size?
				// We assume the pipeline gives us the correct size or we should check caps.
				// For now, just copy.

				if let Err(e) = cudarc::driver::result::memcpy_htod_sync(
					(*capture_buffer.as_mut_ptr()).data[0] as cudarc::driver::sys::CUdeviceptr,
					data
				) {
					tracing::error!("Failed to copy memory to CUDA: {e}");
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

			current_frame += 1;
			tracing::trace!("Current frame: {}", current_frame);
			frame_number.store(current_frame, Ordering::Relaxed);
			frame_notifier.notify_all();
		}

		let _ = pipeline.set_state(gst::State::Null);
		tracing::debug!("Frame capturer stopped.");
	}
}
