//! PipeWire video capture module.
//!
//! This module provides video frame capture from PipeWire nodes.
//! Frames are captured as DMA-BUF references for zero-copy GPU processing.

use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use async_shutdown::ShutdownManager;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::video::VideoFormat as SpaVideoFormat;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::Pod;

use crate::session::manager::SessionShutdownReason;

/// Pixel format of the captured frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum CapturePixelFormat {
	/// BGRx (32-bit, blue first)
	BGRx,
	/// RGBx (32-bit, red first)
	RGBx,
	/// BGRA (32-bit with alpha)
	BGRA,
	/// RGBA (32-bit with alpha)
	RGBA,
	/// NV12 (YUV420 semi-planar)
	NV12,
	/// I420 (YUV420 planar)
	I420,
}

impl CapturePixelFormat {
	/// Convert from SPA video format.
	pub fn from_spa_format(format: SpaVideoFormat) -> Option<Self> {
		match format {
			SpaVideoFormat::BGRx => Some(Self::BGRx),
			SpaVideoFormat::RGBx => Some(Self::RGBx),
			SpaVideoFormat::BGRA => Some(Self::BGRA),
			SpaVideoFormat::RGBA => Some(Self::RGBA),
			SpaVideoFormat::NV12 => Some(Self::NV12),
			SpaVideoFormat::I420 => Some(Self::I420),
			_ => None,
		}
	}
}

/// Configuration for video capture.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
	/// PipeWire node ID to capture from.
	pub node_id: u32,
	/// Expected frame width.
	pub width: u32,
	/// Expected frame height.
	pub height: u32,
}

/// DMA-BUF information for zero-copy frame capture.
#[derive(Debug)]
pub struct DmaBufInfo {
	/// File descriptor for the DMA-BUF (duplicated, safe to use across threads)
	pub fd: std::os::unix::io::RawFd,
	/// Byte offset into the DMA-BUF.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
	/// DRM format modifier.
	pub modifier: u64,
	/// Width of the frame.
	pub width: u32,
	/// Height of the frame.
	pub height: u32,
}

impl Drop for DmaBufInfo {
	fn drop(&mut self) {
		if self.fd >= 0 {
			// SAFETY: We own the FD and it was duplicated for us.
			unsafe {
				let _ = OwnedFd::from_raw_fd(self.fd);
			}
		}
	}
}

/// A captured video frame (DMA-BUF only, zero-copy path)
pub struct CapturedFrame {
	/// DMA-BUF info for zero-copy.
	pub dmabuf: DmaBufInfo,
	/// Pixel format of the data.
	pub format: CapturePixelFormat,
}

/// Handle to a running capture session.
pub struct CaptureHandle {
	/// Thread handle for the capture loop.
	thread: Option<JoinHandle<()>>,
	/// Receiver for captured frames.
	frame_rx: Receiver<CapturedFrame>,
}

impl CaptureHandle {
	/// Try to receive a frame without blocking.
	#[allow(dead_code)]
	pub fn try_recv(&self) -> Option<CapturedFrame> {
		self.frame_rx.try_recv().ok()
	}

	/// Receive a frame with a timeout.
	pub fn recv_timeout(&self, timeout: std::time::Duration) -> Result<CapturedFrame, mpsc::RecvTimeoutError> {
		self.frame_rx.recv_timeout(timeout)
	}

	/// Wait for the capture thread to finish.
	#[allow(dead_code)]
	pub fn join(mut self) -> Result<(), Box<dyn std::any::Any + Send>> {
		if let Some(thread) = self.thread.take() {
			thread.join()
		} else {
			Ok(())
		}
	}
}

impl Drop for CaptureHandle {
	fn drop(&mut self) {
		// Thread will be stopped when shutdown manager triggers.
		// Just wait for it to finish if it hasn't already.
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// Start a video capture session.
///
/// This spawns a dedicated thread that runs the PipeWire event loop.
/// Captured frames are converted to YUV and sent to the returned handle.
///
/// # Arguments
/// * `config` - Capture configuration
/// * `stop` - Shutdown manager to signal when to stop capturing
///
/// # Returns
/// A `CaptureHandle` that can be used to receive captured frames.
pub fn start_capture(
	config: CaptureConfig,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<CaptureHandle, String> {
	let (frame_tx, frame_rx) = mpsc::channel();

	let thread = thread::Builder::new()
		.name("pipewire-capture".to_string())
		.spawn(move || {
			if let Err(e) = run_capture_loop(config, frame_tx, stop) {
				tracing::error!("PipeWire capture failed: {}", e);
			}
		})
		.map_err(|e| format!("Failed to spawn capture thread: {}", e))?;

	Ok(CaptureHandle {
		thread: Some(thread),
		frame_rx,
	})
}

/// Internal state for negotiated video format.
#[derive(Clone, Copy, Debug, Default)]
struct NegotiatedFormat {
	format: Option<SpaVideoFormat>,
	width: u32,
	height: u32,
}

/// Main capture loop running in dedicated thread.
fn run_capture_loop(
	config: CaptureConfig,
	frame_tx: Sender<CapturedFrame>,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	// Initialize pipewire in this thread.
	pipewire::init();

	let mainloop =
		pipewire::main_loop::MainLoopBox::new(None).map_err(|e| format!("Failed to create PipeWire main loop: {e}"))?;
	let context = pipewire::context::ContextBox::new(mainloop.loop_(), None)
		.map_err(|e| format!("Failed to create PipeWire context: {e}"))?;
	let core = context
		.connect(None)
		.map_err(|e| format!("Failed to connect to PipeWire: {e}"))?;

	// Create a stream to capture from the node.
	let stream = pipewire::stream::StreamBox::new(
		&core,
		"moonshine-video-capture",
		pipewire::properties::properties! {
			*pipewire::keys::MEDIA_TYPE => "Video",
			*pipewire::keys::MEDIA_CATEGORY => "Capture",
			*pipewire::keys::MEDIA_ROLE => "Screen",
		},
	)
	.map_err(|e| format!("Failed to create PipeWire stream: {e}"))?;

	// Shared state for the negotiated format.
	let negotiated_format = Arc::new(Mutex::new(NegotiatedFormat::default()));
	let negotiated_format_param = negotiated_format.clone();
	let negotiated_format_process = negotiated_format.clone();

	// Set up stream listener with format negotiation.
	let _listener = stream
		.add_local_listener_with_user_data(())
		.state_changed(|_, _, old, new| {
			tracing::debug!("PipeWire stream state changed: {:?} -> {:?}", old, new);
		})
		.param_changed(move |_stream, _, id, pod| {
			// Only handle Format parameter.
			if id != ParamType::Format.as_raw() {
				return;
			}

			if let Some(pod) = pod {
				if let Some(format_info) = parse_video_format_from_pod(pod) {
					tracing::info!(
						"Negotiated video format: {:?} {}x{}",
						format_info.format,
						format_info.width,
						format_info.height
					);
					*negotiated_format_param.lock().unwrap() = format_info;

					// Don't set buffer params from consumer side - let producer decide.
					// The producer (gamescope) will use DmaBuf because our format params.
					// include the modifier property. The producer allocates buffers.
					tracing::debug!("Format negotiated with modifier, producer will allocate DMA-BUF buffers");
				}
			}
		})
		.process(move |stream, _| {
			if let Some(mut buffer) = stream.dequeue_buffer() {
				let datas = buffer.datas_mut();
				if datas.is_empty() {
					return;
				}

				let data = &mut datas[0];

				// Get the negotiated format.
				let format = negotiated_format_process.lock().unwrap();
				let spa_format = format.format.unwrap_or(SpaVideoFormat::BGRx);
				let frame_width = format.width;
				let frame_height = format.height;
				drop(format);

				// Convert SPA format to our format.
				let pixel_format = CapturePixelFormat::from_spa_format(spa_format).unwrap_or(CapturePixelFormat::BGRx);

				// Log the data type for debugging.
				let data_type = data.type_();
				tracing::trace!("Received buffer with data type: {:?}", data_type);

				// Check if this is a DMA-BUF (required for zero-copy)
				if data_type == DataType::DmaBuf {
					// DMA-BUF path: send fd info for zero-copy import.
					let fd = data.fd();

					// Duplicate the fd so it survives after buffer is returned.
					// SAFETY: data.fd() returns a valid fd owned by PipeWire
					let owned: OwnedFd = {
						let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
						match borrowed.try_clone_to_owned() {
							Ok(owned) => owned,
							Err(e) => {
								tracing::warn!("Failed to duplicate DMA-BUF fd: {e}");
								return;
							},
						}
					};

					// Get the raw fd and forget the OwnedFd so it doesn't close.
					// The pipeline is now responsible for closing this fd.
					let dup_fd = owned.as_raw_fd();
					std::mem::forget(owned);

					// Get chunk info for offset/size.
					let chunk = data.chunk();
					let offset = chunk.offset();
					let stride = chunk.stride();

					// Send frame.
					let _ = frame_tx.send(CapturedFrame {
						dmabuf: DmaBufInfo {
							fd: dup_fd,
							offset,
							stride: stride as u32,
							modifier: 0, // Linear layout (TODO: get from buffer metadata)
							width: frame_width,
							height: frame_height,
						},
						format: pixel_format,
					});
				} else {
					// Memory-mapped path - not supported, we only work with DMA-BUF.
					tracing::error!(
						"Received non-DMA-BUF frame (type: {:?}), only DMA-BUF is supported for zero-copy encoding",
						data_type
					);
				}
			}
		})
		.register()
		.map_err(|e| format!("Failed to register stream listener: {e}"))?;

	// Connect to the pipewire node.
	tracing::debug!(
		"Connecting to PipeWire node {} for {}x{} capture",
		config.node_id,
		config.width,
		config.height
	);

	// Build format params with modifier to force DMA-BUF negotiation.
	// The modifier property with MANDATORY flag tells gamescope to use DmaBuf.
	let format_params_buffer = build_format_params().ok_or("Failed to build format params")?;

	// Cast the buffer to a Pod reference.
	// SAFETY: build_format_params returns a valid serialized pod
	let format_pod = unsafe { &*(format_params_buffer.as_ptr() as *const Pod) };
	let mut params: Vec<&Pod> = vec![format_pod];

	stream
		.connect(
			pipewire::spa::utils::Direction::Input,
			Some(config.node_id),
			pipewire::stream::StreamFlags::AUTOCONNECT,
			&mut params[..],
		)
		.map_err(|e| format!("Failed to connect PipeWire stream: {e}"))?;

	// Run the main loop with periodic checks for shutdown.
	let pw_loop = mainloop.loop_();
	let iterate_timeout = std::time::Duration::from_millis(100);

	tracing::debug!("Starting PipeWire capture loop");

	while !stop.is_shutdown_triggered() {
		pw_loop.iterate(iterate_timeout);
	}

	tracing::debug!("PipeWire capture loop stopped");
	Ok(())
}

/// Parse video format information from a negotiated format pod.
fn parse_video_format_from_pod(pod: &Pod) -> Option<NegotiatedFormat> {
	use pipewire::spa::sys;

	let pod_ptr = pod.as_raw_ptr();
	let mut video_info: sys::spa_video_info_raw = unsafe { std::mem::zeroed() };

	let result = unsafe { sys::spa_format_video_raw_parse(pod_ptr, &mut video_info) };

	if result < 0 {
		tracing::warn!("Failed to parse video format from pod: {}", result);
		return None;
	}

	let format = NegotiatedFormat {
		format: Some(SpaVideoFormat::from_raw(video_info.format)),
		width: video_info.size.width,
		height: video_info.size.height,
	};

	tracing::info!(
		"Parsed video format: {:?} {}x{}",
		format.format,
		format.width,
		format.height
	);

	Some(format)
}

/// Build format parameters for stream connection.
///
/// This creates a SPA pod that advertises what video formats we can accept.
/// The modifier property with MANDATORY flag forces DMA-BUF negotiation.
fn build_format_params() -> Option<Vec<u8>> {
	use pipewire::spa::sys;
	use std::mem::MaybeUninit;

	// Buffer to hold the serialized pod.
	let mut buffer = vec![0u8; 1024];

	// Video format constants from spa/param/video/raw.h enum.
	// SPA_VIDEO_FORMAT_BGRx = 8 (verified from the enum in raw.h)
	const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
	// DRM_FORMAT_MOD_LINEAR = 0 (linear memory layout)
	const DRM_FORMAT_MOD_LINEAR: i64 = 0;

	unsafe {
		let builder = sys::spa_pod_builder {
			data: buffer.as_mut_ptr() as *mut std::ffi::c_void,
			size: buffer.len() as u32,
			_padding: 0,
			state: sys::spa_pod_builder_state {
				offset: 0,
				flags: 0,
				frame: std::ptr::null_mut(),
			},
			callbacks: sys::spa_callbacks {
				funcs: std::ptr::null(),
				data: std::ptr::null_mut(),
			},
		};

		let mut builder = builder;
		let mut frame: MaybeUninit<sys::spa_pod_frame> = MaybeUninit::uninit();
		let mut choice_frame: MaybeUninit<sys::spa_pod_frame> = MaybeUninit::uninit();

		// Build format object (SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat)
		sys::spa_pod_builder_push_object(
			&mut builder,
			frame.as_mut_ptr(),
			sys::SPA_TYPE_OBJECT_Format,
			sys::SPA_PARAM_EnumFormat,
		);

		// SPA_FORMAT_mediaType = Video.
		sys::spa_pod_builder_prop(&mut builder, sys::SPA_FORMAT_mediaType, 0);
		sys::spa_pod_builder_id(&mut builder, sys::SPA_MEDIA_TYPE_video);

		// SPA_FORMAT_mediaSubtype = raw.
		sys::spa_pod_builder_prop(&mut builder, sys::SPA_FORMAT_mediaSubtype, 0);
		sys::spa_pod_builder_id(&mut builder, sys::SPA_MEDIA_SUBTYPE_raw);

		// SPA_FORMAT_VIDEO_format = BGRx.
		sys::spa_pod_builder_prop(&mut builder, sys::SPA_FORMAT_VIDEO_format, 0);
		sys::spa_pod_builder_id(&mut builder, SPA_VIDEO_FORMAT_BGRX);

		// SPA_FORMAT_VIDEO_size (with range to accept any size from producer)
		sys::spa_pod_builder_prop(&mut builder, sys::SPA_FORMAT_VIDEO_size, 0);
		sys::spa_pod_builder_push_choice(&mut builder, choice_frame.as_mut_ptr(), sys::SPA_CHOICE_Range, 0);
		// Default, min, max sizes - we accept any size from gamescope.
		sys::spa_pod_builder_rectangle(&mut builder, 1920, 1080); // default
		sys::spa_pod_builder_rectangle(&mut builder, 1, 1); // min
		sys::spa_pod_builder_rectangle(&mut builder, 65535, 65535); // max
		sys::spa_pod_builder_pop(&mut builder, choice_frame.as_mut_ptr());

		// SPA_FORMAT_VIDEO_framerate (with range)
		sys::spa_pod_builder_prop(&mut builder, sys::SPA_FORMAT_VIDEO_framerate, 0);
		sys::spa_pod_builder_push_choice(&mut builder, choice_frame.as_mut_ptr(), sys::SPA_CHOICE_Range, 0);
		// Default framerate (30/1), min (0/1), max (60/1)
		sys::spa_pod_builder_fraction(&mut builder, 30, 1);
		sys::spa_pod_builder_fraction(&mut builder, 0, 1);
		sys::spa_pod_builder_fraction(&mut builder, 60, 1);
		sys::spa_pod_builder_pop(&mut builder, choice_frame.as_mut_ptr());

		// SPA_FORMAT_VIDEO_modifier with MANDATORY flag - forces DMA-BUF negotiation.
		// The producer (gamescope) checks for this property to decide DmaBuf vs MemFd.
		// Use Enum choice with LINEAR modifier (same as gamescope producer offers)
		sys::spa_pod_builder_prop(
			&mut builder,
			sys::SPA_FORMAT_VIDEO_modifier,
			sys::SPA_POD_PROP_FLAG_MANDATORY,
		);
		sys::spa_pod_builder_push_choice(&mut builder, choice_frame.as_mut_ptr(), sys::SPA_CHOICE_Enum, 0);
		// First value is default, second is the enum value.
		sys::spa_pod_builder_long(&mut builder, DRM_FORMAT_MOD_LINEAR);
		sys::spa_pod_builder_long(&mut builder, DRM_FORMAT_MOD_LINEAR);
		sys::spa_pod_builder_pop(&mut builder, choice_frame.as_mut_ptr());

		// Pop the format object.
		let pod = sys::spa_pod_builder_pop(&mut builder, frame.as_mut_ptr());

		if pod.is_null() {
			tracing::warn!("Failed to build format params pod");
			return None;
		}

		let pod_typed = pod as *mut sys::spa_pod;
		let pod_size = (*pod_typed).size as usize + std::mem::size_of::<sys::spa_pod>();
		buffer.truncate(pod_size);

		tracing::debug!("Built format params pod with modifier: {} bytes", pod_size);
		Some(buffer)
	}
}
