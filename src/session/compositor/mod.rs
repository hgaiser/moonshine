//! Embedded headless Smithay compositor for Moonshine.
//!
//! This module replaces the external Gamescope compositor and PipeWire capture
//! with an in-process Smithay compositor. Frames are rendered to GBM-backed
//! DMA-BUFs and exported directly to the video encoder.

mod cursor;
mod focus;
pub mod frame;
mod handlers;
pub mod input;
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

type CompositorChannels = (
	mpsc::Receiver<ExportedFrame>,
	calloop::channel::Sender<CompositorInputEvent>,
	mpsc::Receiver<u32>,
);

/// Configuration for the compositor.
#[derive(Clone, Debug)]
pub struct CompositorConfig {
	/// Width of the virtual output in pixels.
	pub width: u32,
	/// Height of the virtual output in pixels.
	pub height: u32,
	/// Refresh rate in Hz.
	pub refresh_rate: u32,
}

/// Start the headless compositor on a dedicated thread.
///
/// Returns a receiver for exported frames, a sender for input events,
/// and a receiver for the XWayland display number.
/// The compositor runs on its own `calloop::EventLoop` thread.
pub fn start_compositor(
	config: CompositorConfig,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<CompositorChannels, String> {
	let (frame_tx, frame_rx) = mpsc::sync_channel::<ExportedFrame>(2);
	let (input_tx, input_rx) = calloop::channel::channel::<CompositorInputEvent>();
	let (xdisplay_tx, xdisplay_rx) = mpsc::sync_channel::<u32>(1);

	std::thread::Builder::new()
		.name("compositor".to_string())
		.spawn(move || {
			if let Err(e) = run_compositor(config, frame_tx, input_rx, xdisplay_tx, stop) {
				tracing::error!("Compositor failed: {e}");
			}
		})
		.map_err(|e| format!("Failed to spawn compositor thread: {e}"))?;

	Ok((frame_rx, input_tx, xdisplay_rx))
}

/// Main compositor loop running on a dedicated thread.
fn run_compositor(
	config: CompositorConfig,
	frame_tx: mpsc::SyncSender<ExportedFrame>,
	input_rx: calloop::channel::Channel<CompositorInputEvent>,
	xdisplay_tx: mpsc::SyncSender<u32>,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	// Trigger session shutdown if the compositor exits unexpectedly.
	let _session_stop_token = stop.trigger_shutdown_token(SessionShutdownReason::CompositorStopped);
	let _delay_stop = stop.delay_shutdown_token();

	// Open a render node (no DRM master required for headless operation).
	let render_node = find_render_node()?;
	tracing::debug!("Using render node: {}", render_node.display());

	// Open the render node twice — GbmDevice<File> doesn't impl Clone
	// because File doesn't impl Clone, so we need separate file handles
	// for the allocator's GBM device and the EGL display's GBM device.
	let render_fd_alloc = std::fs::File::open(&render_node)
		.map_err(|e| format!("Failed to open render node {}: {e}", render_node.display()))?;
	let render_fd_egl = std::fs::File::open(&render_node)
		.map_err(|e| format!("Failed to open render node {}: {e}", render_node.display()))?;

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

	// Prefer Argb8888, then Xrgb8888, then fall back to first available format.
	let preferred_fourccs = [Fourcc::Argb8888, Fourcc::Xrgb8888];
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

	tracing::debug!(
		"Selected render format: {:?} with {} modifier(s)",
		render_fourcc,
		render_modifiers.len()
	);

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
		xdisplay_tx,
		&render_node,
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

	tracing::info!("Compositor stopped.");
	Ok(())
}

/// Find the first available DRM render node.
fn find_render_node() -> Result<std::path::PathBuf, String> {
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

	entries
		.into_iter()
		.next()
		.map(|e| e.path())
		.ok_or_else(|| "No render node found in /dev/dri".to_string())
}
