//! Embedded headless Smithay compositor for Moonshine.
//!
//! This module replaces the external Gamescope compositor and PipeWire capture
//! with an in-process Smithay compositor. Frames are rendered to GBM-backed
//! DMA-BUFs and exported directly to the video encoder.

mod color_management;
mod cursor;
mod focus;
pub mod frame;
mod gamescope_swapchain;
mod handlers;
pub mod input;
mod protocols;
mod state;

use std::sync::mpsc;

use async_shutdown::ShutdownManager;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{Capability, GlesRenderer};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::Transform;

use crate::session::manager::SessionShutdownReason;

use self::frame::ExportedFrame;
use self::input::CompositorInputEvent;
use self::state::MoonshineCompositor;

/// Configuration for the compositor.
#[derive(Clone, Debug)]
pub struct CompositorConfig {
	/// Width of the virtual output in pixels.
	pub width: u32,
	/// Height of the virtual output in pixels.
	pub height: u32,
	/// Refresh rate in Hz.
	pub refresh_rate: u32,
	/// Optional GPU configuration (path, PCI ID, vendor:device, or vendor name).
	pub gpu: Option<String>,
	/// Whether HDR mode is active. When true, prefers 10-bit/FP16 GBM formats.
	pub hdr: bool,
}

/// Information sent from the compositor thread once XWayland is ready.
pub struct CompositorReady {
	/// X11 display number (e.g. `:1` → `1`).
	pub xdisplay: u32,
	/// Wayland socket name for the gamescope WSI layer (only set when HDR is active).
	pub gamescope_wayland_display: Option<String>,
}

/// Result type for `start_compositor`.
type CompositorHandles = (
	mpsc::Receiver<ExportedFrame>,
	calloop::channel::Sender<CompositorInputEvent>,
	mpsc::Receiver<CompositorReady>,
);

/// Start the headless compositor on a dedicated thread.
///
/// Returns a receiver for exported frames, a sender for input events,
/// and a receiver for the XWayland display number.
/// The compositor runs on its own `calloop::EventLoop` thread.
pub fn start_compositor(
	config: CompositorConfig,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<CompositorHandles, String> {
	let (frame_tx, frame_rx) = mpsc::sync_channel::<ExportedFrame>(2);
	let (input_tx, input_rx) = calloop::channel::channel::<CompositorInputEvent>();
	let (ready_tx, ready_rx) = mpsc::sync_channel::<CompositorReady>(1);

	std::thread::Builder::new()
		.name("compositor".to_string())
		.spawn(move || {
			if let Err(e) = run_compositor(config, frame_tx, input_rx, ready_tx, stop) {
				tracing::error!("Compositor failed: {e}");
			}
		})
		.map_err(|e| format!("Failed to spawn compositor thread: {e}"))?;

	Ok((frame_rx, input_tx, ready_rx))
}

/// Main compositor loop running on a dedicated thread.
fn run_compositor(
	config: CompositorConfig,
	frame_tx: mpsc::SyncSender<ExportedFrame>,
	input_rx: calloop::channel::Channel<CompositorInputEvent>,
	ready_tx: mpsc::SyncSender<CompositorReady>,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	// Trigger session shutdown if the compositor exits unexpectedly.
	let _session_stop_token = stop.trigger_shutdown_token(SessionShutdownReason::CompositorStopped);
	let _delay_stop = stop.delay_shutdown_token();

	// Open a render node (no DRM master required for headless operation).
	let render_node = find_render_node(&config.gpu)?;
	tracing::debug!("Using render node: {}", render_node.display());

	// Open the render node.
	// Must use read-write access: DRM render nodes require O_RDWR for
	// GPU buffer mapping (amdgpu_bo_cpu_map fails with EACCES otherwise).
	let render_fd_alloc = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(&render_node)
		.map_err(|e| format!("Failed to open render node {}: {e}", render_node.display()))?;

	// Clone the file handle for the EGL display's GBM device.
	// GbmDevice takes ownership of the file, so we need a separate handle.
	let render_fd_egl = render_fd_alloc
		.try_clone()
		.map_err(|e| format!("Failed to clone render node handle: {e}"))?;

	// Initialize GBM.
	let gbm_device_alloc =
		GbmDevice::new(render_fd_alloc).map_err(|e| format!("Failed to create GBM device for allocator: {e}"))?;
	let gbm_allocator = GbmAllocator::new(gbm_device_alloc, GbmBufferFlags::RENDERING);

	// Initialize EGL + GLES renderer.
	let gbm_device_egl =
		GbmDevice::new(render_fd_egl).map_err(|e| format!("Failed to create GBM device for EGL: {e}"))?;
	let egl_display =
		unsafe { EGLDisplay::new(gbm_device_egl) }.map_err(|e| format!("Failed to create EGL display: {e}"))?;
	let egl_context = EGLContext::new(&egl_display).map_err(|e| format!("Failed to create EGL context: {e}"))?;

	// Use all supported capabilities except per-texture Fencing.
	// Moonshine renders with a single non-shared EGL context and the
	// frame-level EGLFence (ExportFence) already ensures all GPU work is
	// complete before the DMA-BUF is handed to Vulkan.  Per-texture read
	// fences are redundant here and DMA-BUF implicit sync protects client
	// buffer reuse.  Removing them saves ~3% compositor-thread CPU
	// (TextureSync::update_read overhead).
	let capabilities = unsafe { GlesRenderer::supported_capabilities(&egl_context) }
		.map_err(|e| format!("Failed to query renderer capabilities: {e}"))?;
	let capabilities = capabilities.into_iter().filter(|c| *c != Capability::Fencing);
	let renderer = unsafe { GlesRenderer::with_capabilities(egl_context, capabilities) }
		.map_err(|e| format!("Failed to create GLES renderer: {e}"))?;

	// Query the EGL display for formats that can be used as render targets.
	let render_formats = renderer.egl_context().dmabuf_render_formats();
	tracing::debug!("Supported DMA-BUF render formats: {}", render_formats.iter().count());

	// Select preferred render format based on HDR mode.
	// HDR: prefer 10-bit > FP16 > 8-bit ABGR (to match common Vulkan WSI format).
	// SDR: prefer 8-bit ABGR/XBGR to match Vulkan WSI and avoid GL R↔B channel swaps.
	// Vulkan WSI on Wayland defaults to XBGR/ABGR formats, so using ARGB causes
	// GL to incorrectly swap red/blue channels during blit operations.
	let preferred_fourccs: Vec<Fourcc> = if config.hdr {
		vec![
			Fourcc::Abgr2101010,
			Fourcc::Abgr16161616f,
			Fourcc::Abgr8888,
			Fourcc::Xbgr8888,
		]
	} else {
		vec![Fourcc::Abgr8888, Fourcc::Xbgr8888, Fourcc::Argb8888, Fourcc::Xrgb8888]
	};
	let (render_fourcc, render_modifiers) = preferred_fourccs
		.iter()
		.find_map(|&fourcc| {
			let modifiers: Vec<Modifier> = render_formats
				.iter()
				.filter(|f| f.code == fourcc)
				.map(|f| f.modifier)
				.collect();
			if modifiers.is_empty() {
				None
			} else {
				Some((fourcc, modifiers))
			}
		})
		.or_else(|| {
			// Fall back to first available format, collecting all its modifiers.
			let first = render_formats.iter().next()?;
			let fourcc = first.code;
			let modifiers: Vec<Modifier> = render_formats
				.iter()
				.filter(|f| f.code == fourcc)
				.map(|f| f.modifier)
				.collect();
			Some((fourcc, modifiers))
		})
		.ok_or_else(|| "No supported DMA-BUF render formats found".to_string())?;

	tracing::info!(
		"Selected render format: {:?} with {} modifier(s)",
		render_fourcc,
		render_modifiers.len()
	);

	// Derive effective HDR: only if an HDR-capable format was actually selected.
	let hdr = config.hdr && matches!(render_fourcc, Fourcc::Abgr16161616f | Fourcc::Abgr2101010);
	if config.hdr && !hdr {
		tracing::warn!(
			"HDR requested but no HDR-capable format available (using {:?}), falling back to SDR",
			render_fourcc
		);
	}

	// Create the calloop event loop.
	let mut event_loop: EventLoop<MoonshineCompositor> =
		EventLoop::try_new().map_err(|e| format!("Failed to create event loop: {e}"))?;

	// Create the Wayland display.
	let display = smithay::reexports::wayland_server::Display::<MoonshineCompositor>::new()
		.map_err(|e| format!("Failed to create Wayland display: {e}"))?;
	let display_handle = display.handle();

	// Create a virtual output.
	let mode = Mode {
		size: (config.width as i32, config.height as i32).into(),
		refresh: (config.refresh_rate * 1000) as i32,
	};

	let output = Output::new(
		"moonshine-virtual".to_string(),
		PhysicalProperties {
			size: (0, 0).into(),
			subpixel: Subpixel::Unknown,
			make: "Moonshine".into(),
			model: "Virtual Output".into(),
		},
	);
	output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
	output.set_preferred(mode);

	// Create the damage tracker for this output.
	let damage_tracker = OutputDamageTracker::from_output(&output);

	// Build the compositor state.
	let (state, display) = MoonshineCompositor::new(
		display,
		display_handle.clone(),
		event_loop.handle(),
		output,
		damage_tracker,
		gbm_allocator,
		renderer,
		frame_tx,
		config.width,
		config.height,
		render_fourcc,
		render_modifiers,
		ready_tx,
		&render_node,
		hdr,
	);

	// Insert the Wayland display as a calloop event source so client
	// messages (including XWayland's protocol handshake) are dispatched
	// whenever data arrives on the Wayland socket, not only after the
	// frame timer fires.
	event_loop
		.handle()
		.insert_source(
			calloop::generic::Generic::new(display, calloop::Interest::READ, calloop::Mode::Level),
			|_, display, state: &mut MoonshineCompositor| {
				// Safety: we never drop the display while the event loop runs.
				unsafe {
					let display = display.get_mut();
					if let Err(e) = display.dispatch_clients(state) {
						tracing::error!("Failed to dispatch Wayland clients: {e}");
					}

					// Send deferred wp_image_description_info_v1 destructor events.
					for info in state.deferred_info_done.drain(..) {
						info.done();
					}

					// Flush pending events back to clients. Without this,
					// responses (e.g. wl_registry.global, wl_callback.done)
					// remain buffered and are never sent, causing XWayland's
					// initial roundtrip to block indefinitely.
					if let Err(e) = display.flush_clients() {
						tracing::error!("Failed to flush Wayland clients: {e}");
					}
				}
				Ok(calloop::PostAction::Continue)
			},
		)
		.map_err(|e| format!("Failed to insert Wayland display source: {e}"))?;

	// Register the input channel from the control stream.
	event_loop
		.handle()
		.insert_source(input_rx, |event, _, state: &mut MoonshineCompositor| {
			if let calloop::channel::Event::Msg(input_event) = event {
				input::process_input(input_event, state);
				// Flush queued Wayland events (pointer enter/motion/button, keyboard
				// key, etc.) to the client immediately. Without this the events sit
				// in the outgoing buffer until the next Display dispatch cycle.
				let _ = state.display_handle.flush_clients();
			}
		})
		.map_err(|e| format!("Failed to insert input channel: {e}"))?;

	// Set up the frame timer.
	// Use Instant-based absolute scheduling so that render time inside
	// the callback doesn't drift the cadence. `ToDuration` would add the
	// interval *after* the callback returns, progressively skewing the
	// actual period and producing ~58 Hz instead of 60 Hz.
	let frame_nanos: u64 = 1_000_000_000u64 / config.refresh_rate as u64;
	let frame_interval = std::time::Duration::from_nanos(frame_nanos);
	let mut next_frame = std::time::Instant::now() + frame_interval;
	let timer = smithay::reexports::calloop::timer::Timer::from_duration(frame_interval);
	event_loop
		.handle()
		.insert_source(timer, move |_event, _metadata, state: &mut MoonshineCompositor| {
			state.render_and_export();
			// Schedule the next frame relative to the ideal wall-clock
			// target, not relative to "now". This absorbs render-time
			// jitter and keeps a steady cadence.
			next_frame += frame_interval;
			let now = std::time::Instant::now();
			if next_frame <= now {
				// We fell behind — snap forward instead of bursting.
				next_frame = now + frame_interval;
			}
			smithay::reexports::calloop::timer::TimeoutAction::ToInstant(next_frame)
		})
		.map_err(|e| format!("Failed to insert frame timer: {e}"))?;

	tracing::info!(
		"Compositor started: {}x{} @ {}Hz",
		config.width,
		config.height,
		config.refresh_rate
	);

	// Run the event loop.
	// Use `None` as timeout so dispatch blocks until the next calloop
	// source fires (frame timer, input channel, or Wayland client event).
	// A hard timeout like 16ms would compete with the frame timer cadence.
	let mut state = state;
	state.start_xwayland();

	tracing::debug!(
		shutdown_triggered = stop.is_shutdown_triggered(),
		"Entering compositor event loop"
	);

	while !stop.is_shutdown_triggered() {
		event_loop
			.dispatch(None, &mut state)
			.map_err(|e| format!("Event loop dispatch error: {e}"))?;
	}

	// Stop the application first so X11 clients disconnect from
	// Xwayland, then tear down the X11 window manager. When the event
	// loop is dropped afterwards, Smithay's XWayland::Drop disconnects
	// the Wayland client, and the `-terminate` flag causes Xwayland to
	// exit.
	state.shutdown_session_processes();

	tracing::info!("Compositor stopped.");
	Ok(())
}

/// Find the appropriate DRM render node.
fn find_render_node(gpu_config: &Option<String>) -> Result<std::path::PathBuf, String> {
	// Check environment variable override first.
	if let Ok(node) = std::env::var("MOONSHINE_RENDER_NODE") {
		return Ok(std::path::PathBuf::from(node));
	}

	// Scan /dev/dri/ for render nodes.
	let dri_path = std::path::Path::new("/dev/dri");
	if !dri_path.exists() {
		return Err("No /dev/dri directory found".to_string());
	}

	let mut entries: Vec<_> = std::fs::read_dir(dri_path)
		.map_err(|e| format!("Failed to read /dev/dri: {e}"))?
		.filter_map(|entry| entry.ok())
		.filter(|entry| {
			entry
				.file_name()
				.to_str()
				.map(|name| name.starts_with("renderD"))
				.unwrap_or(false)
		})
		.collect();

	entries.sort_by_key(|e| e.file_name());

	// If no render nodes found, return error.
	if entries.is_empty() {
		return Err("No render node found in /dev/dri".to_string());
	}

	// Helper to get uevent info
	let get_device_info = |entry: &std::fs::DirEntry| -> Option<(String, String)> {
		let file_name = entry.file_name();
		let name = file_name.to_str()?;
		let device_path = std::path::Path::new("/sys/class/drm").join(name).join("device/uevent");
		let content = std::fs::read_to_string(device_path).ok()?;

		// Parse uevent for convenience
		// We care about PCI_SLOT_NAME (e.g. 0000:01:00.0), PCI_ID (e.g. 10DE:2C02), DRIVER (e.g. nvidia)
		Some((name.to_string(), content))
	};

	if let Some(config_str) = gpu_config {
		// If config is an absolute path, use it directly.
		let path = std::path::Path::new(config_str);
		if path.is_absolute() {
			if path.exists() {
				return Ok(path.to_path_buf());
			} else {
				return Err(format!("Configured GPU path does not exist: {}", config_str));
			}
		}

		// Otherwise, search for a match in available nodes.
		for entry in &entries {
			if let Some((name, uevent)) = get_device_info(entry) {
				// Check filename match (e.g. "renderD128")
				if name == *config_str {
					return Ok(entry.path());
				}

				// Check uevent content matches
				// We do a case-insensitive substring match for flexibility.
				if uevent.to_lowercase().contains(&config_str.to_lowercase()) {
					return Ok(entry.path());
				}
			}
		}

		return Err(format!("No GPU found matching configuration: {}", config_str));
	}

	// Heuristics: prefer discrete GPU.
	// We prioritize NVIDIA > AMD > Intel for discrete GPUs.
	// But distinguishing AMD iGPU vs dGPU is hard without more info.
	// However, usually:
	// NVIDIA (10DE) -> Discrete
	// AMD (1002) -> Discrete or Integrated
	// Intel (8086) -> Integrated (mostly)

	// Let's iterate and score them.
	let mut best_node = None;
	let mut best_score = -1;

	for entry in &entries {
		let score = if let Some((_, uevent)) = get_device_info(entry) {
			if uevent.contains("PCI_ID=10DE") {
				// NVIDIA
				100
			} else if uevent.contains("PCI_ID=1002") {
				// AMD
				// If we could distinguish iGPU/dGPU here it would be better, but for now give it a high score.
				// Assuming if NVIDIA is present it wins (score 100), otherwise AMD (score 50).
				50
			} else {
				// Intel or others
				0
			}
		} else {
			0
		};

		if score > best_score {
			best_score = score;
			best_node = Some(entry.path());
		}
	}

	// If found a "better" one, use it. Otherwise fall back to the first one (original behavior).
	Ok(best_node.unwrap_or_else(|| entries[0].path()))
}

/// Probe the GPU for HDR-capable render formats.
///
/// Opens the render node, creates a temporary EGL context to query
/// DMA-BUF render formats, and returns `true` if any 10-bit or FP16
/// format suitable for HDR is available (Abgr2101010, Abgr16161616f).
pub fn probe_hdr_support(gpu_config: &Option<String>) -> bool {
	let render_node = match find_render_node(gpu_config) {
		Ok(node) => node,
		Err(e) => {
			tracing::warn!("HDR probe: failed to find render node: {e}");
			return false;
		},
	};

	let render_fd = match std::fs::OpenOptions::new().read(true).write(true).open(&render_node) {
		Ok(fd) => fd,
		Err(e) => {
			tracing::warn!("HDR probe: failed to open render node {}: {e}", render_node.display());
			return false;
		},
	};

	let gbm_device = match GbmDevice::new(render_fd) {
		Ok(dev) => dev,
		Err(e) => {
			tracing::warn!("HDR probe: failed to create GBM device: {e}");
			return false;
		},
	};

	let egl_display = match unsafe { EGLDisplay::new(gbm_device) } {
		Ok(d) => d,
		Err(e) => {
			tracing::warn!("HDR probe: failed to create EGL display: {e}");
			return false;
		},
	};

	let egl_context = match EGLContext::new(&egl_display) {
		Ok(c) => c,
		Err(e) => {
			tracing::warn!("HDR probe: failed to create EGL context: {e}");
			return false;
		},
	};

	let render_formats = egl_context.dmabuf_render_formats();
	let hdr_capable = render_formats
		.iter()
		.any(|f| matches!(f.code, Fourcc::Abgr2101010 | Fourcc::Abgr16161616f));

	if hdr_capable {
		tracing::info!("HDR probe: GPU supports HDR-capable render formats.");
	} else {
		tracing::info!("HDR probe: no HDR-capable render formats found, HDR will not be advertised.");
	}

	hdr_capable
}
