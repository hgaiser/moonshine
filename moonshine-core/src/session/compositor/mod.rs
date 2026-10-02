//! Embedded headless Smithay compositor for Moonshine.
//!
//! This module replaces the external Gamescope compositor and PipeWire capture
//! with an in-process Smithay compositor. Frames are rendered to GBM-backed
//! DMA-BUFs and exported directly to the video encoder.

pub(crate) mod admission;
mod capture;
mod color_management;
mod cursor;
mod focus;
pub(crate) mod frame;
mod gamescope_swapchain;
mod gpu_timing;
mod handlers;
pub(crate) mod input;
mod protocols;
mod scaling;
mod state;
mod x11_focus;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{Capability, GlesRenderer};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::Transform;

use crate::session::SessionContext;
use crate::session::manager::SessionShutdownReason;

use self::admission::{CaptureReceiver, CaptureSender, capture_channel};
use self::input::CompositorInputEvent;
use self::state::MoonshineCompositor;

/// Keyboard configuration for the compositor's XKB state.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct KeyboardConfig {
	pub layout: String,
	pub variant: String,
	pub model: String,
	pub options: Option<String>,
}

impl Default for KeyboardConfig {
	fn default() -> Self {
		Self {
			layout: "us".to_string(),
			variant: String::new(),
			model: String::new(),
			options: None,
		}
	}
}

/// How focus is split between windows. Mirrors gamescope's
/// `VirtualConnectorStrategy` (`backend_virtual_connector_strategy`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualConnectorStrategy {
	/// One focus across the whole output; the highest-priority window wins.
	#[default]
	SingleApplication,
	/// Steam names the focus window/app-id list.
	SteamControlled,
	/// One focus per app id.
	PerAppId,
	/// One focus per window.
	PerWindow,
}

impl VirtualConnectorStrategy {
	/// Strategies that only ever drive a single output/connector.
	pub fn is_single_output(self) -> bool {
		matches!(self, Self::SingleApplication | Self::SteamControlled)
	}
}

/// Select complete-scene capture; auto preserves the zero-copy fast path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
	#[default]
	Auto,
	Composited,
}

/// Configuration for the embedded headless compositor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CompositorConfig {
	/// Optional GPU device identifier for compositor rendering.
	pub gpu: Option<String>,
	/// Automatic direct export, or forced composition for compatibility diagnosis.
	pub capture_mode: CaptureMode,

	/// Whether to enable HDR mode in the compositor if the client supports it.
	pub hdr: bool,

	/// Steam integration mode, equivalent to gamescope's `-e`. Promotes the
	/// connector strategy to [`VirtualConnectorStrategy::SteamControlled`] and
	/// enables Steam's window filtering.
	pub steam_mode: bool,

	/// Focus split strategy, equivalent to gamescope's
	/// `backend_virtual_connector_strategy`. Only meaningful without
	/// `steam_mode`/multiple outputs.
	pub virtual_connector_strategy: VirtualConnectorStrategy,

	/// Keyboard configuration for the compositor's XKB state.
	pub keyboard: KeyboardConfig,
}

impl Default for CompositorConfig {
	fn default() -> Self {
		Self {
			gpu: None,
			capture_mode: CaptureMode::Auto,
			hdr: true,
			steam_mode: true,
			virtual_connector_strategy: VirtualConnectorStrategy::SingleApplication,
			keyboard: KeyboardConfig::default(),
		}
	}
}

/// Runtime context derived from the client's session request.
pub(crate) struct CompositorContext {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
	pub output_scale: f64,
	pub log_stats: bool,
}

/// Information sent from the compositor thread once XWayland is ready.
pub(crate) struct CompositorReady {
	/// X11 display number (e.g. `:1` → `1`).
	pub xdisplay: u32,
	/// Wayland socket name for the session compositor.
	pub wayland_display: String,
	/// Whether this compositor can switch HDR on later without restarting.
	pub hdr_capable: bool,
}

struct CompositorReconfigure {
	width: u32,
	height: u32,
	refresh_rate: u32,
	hdr: bool,
	applied: tokio::sync::oneshot::Sender<Result<bool, String>>,
}

/// Handles returned by `Compositor::new()` for wiring into streams.
pub(crate) struct CompositorHandles {
	pub frame_rx: CaptureReceiver,
	pub input_tx: calloop::channel::Sender<CompositorInputEvent>,
}

/// Unlaunched compositor — holds channel endpoints, can only be launched.
pub(crate) struct Compositor {
	config: CompositorConfig,
	context: CompositorContext,
	stop: ShutdownManager<SessionShutdownReason>,
	frame_tx: CaptureSender,
	input_rx: calloop::channel::Channel<CompositorInputEvent>,
	ready_tx: std::sync::mpsc::SyncSender<CompositorReady>,
	ready_rx: std::sync::mpsc::Receiver<CompositorReady>,
	reconfigure_tx: calloop::channel::Sender<CompositorReconfigure>,
	reconfigure_rx: calloop::channel::Channel<CompositorReconfigure>,
}

/// Launched compositor — can be queried, cannot be launched again.
pub(crate) struct LaunchedCompositor {
	ready: CompositorReady,
	reconfigure_tx: calloop::channel::Sender<CompositorReconfigure>,
}

impl CompositorContext {
	pub fn from_session(ctx: &SessionContext, log_stats: bool) -> Self {
		let output_scale = sanitize_output_scale(ctx.application.output_scale);
		if ctx.application.output_scale.is_some_and(|scale| scale != output_scale) {
			tracing::warn!(
				scale = ctx.application.output_scale,
				"Invalid application output scale; using 1.0"
			);
		}
		Self {
			width: ctx.resolution.0,
			height: ctx.resolution.1,
			refresh_rate: ctx.refresh_rate,
			hdr: ctx.hdr,
			output_scale,
			log_stats,
		}
	}
}

fn sanitize_output_scale(scale: Option<f64>) -> f64 {
	match scale {
		Some(scale) if scale.is_finite() && (0.25..=8.0).contains(&scale) => scale,
		_ => 1.0,
	}
}

impl Compositor {
	pub fn new(
		config: CompositorConfig,
		context: CompositorContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> (Self, CompositorHandles) {
		let (frame_tx, frame_rx) = capture_channel();
		let (input_tx, input_rx) = calloop::channel::channel();
		let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
		let (reconfigure_tx, reconfigure_rx) = calloop::channel::channel();

		(
			Self {
				config,
				context,
				stop,
				frame_tx,
				input_rx,
				ready_tx,
				ready_rx,
				reconfigure_tx,
				reconfigure_rx,
			},
			CompositorHandles { frame_rx, input_tx },
		)
	}

	pub fn launch(self) -> Result<LaunchedCompositor, ()> {
		let Self {
			config,
			context,
			stop,
			frame_tx,
			input_rx,
			ready_tx,
			ready_rx,
			reconfigure_tx,
			reconfigure_rx,
		} = self;

		// Registered before the thread exists: session completion then implies
		// the compositor state (buffer pools, client buffers, Xwayland) is gone.
		let worker = crate::session::lifecycle::WorkerGuard::register(&stop, SessionShutdownReason::CompositorStopped)?;
		std::thread::Builder::new()
			.name("compositor".to_string())
			.spawn(move || {
				let _worker = worker;
				if let Err(e) = run_compositor(config, context, frame_tx, input_rx, reconfigure_rx, ready_tx, stop) {
					tracing::error!("Compositor failed: {e}");
				}
			})
			.map_err(|e| {
				tracing::error!("Failed to spawn compositor thread: {e}");
			})?;

		let ready = ready_rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| {
			tracing::warn!("Timed out waiting for compositor ready: {e}");
		})?;

		Ok(LaunchedCompositor { ready, reconfigure_tx })
	}
}

impl LaunchedCompositor {
	pub fn ready(&self) -> &CompositorReady {
		&self.ready
	}
	pub async fn reconfigure(&self, width: u32, height: u32, refresh_rate: u32, hdr: bool) -> Result<bool, ()> {
		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.reconfigure_tx
			.send(CompositorReconfigure {
				width,
				height,
				refresh_rate,
				hdr,
				applied,
			})
			.map_err(|_| ())?;
		waiting
			.await
			.map_err(|_| ())?
			.map_err(|error| tracing::warn!(%error, "Compositor reconfiguration failed"))
	}
}

/// Main compositor loop running on a dedicated thread.
fn run_compositor(
	config: CompositorConfig,
	context: CompositorContext,
	mut frame_tx: CaptureSender,
	input_rx: calloop::channel::Channel<CompositorInputEvent>,
	reconfigure_rx: calloop::channel::Channel<CompositorReconfigure>,
	ready_tx: mpsc::SyncSender<CompositorReady>,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	let capture_demand = frame_tx.take_demand_source();

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

	// Resolve against the actual opened device before publishing readiness or
	// launching an application. Clones retain this GPU across stream epochs.
	frame_tx.set_context(crate::gpu::capture_context(&render_fd_alloc)?);

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
	// HDR: prefer FP16 > 10-bit > 8-bit ABGR. FP16 is required for scRGB
	// (EXTENDED_SRGB_LINEAR) content whose HDR highlights carry values > 1.0 that a
	// 10-bit UNORM render buffer would clamp at composite time; it also holds
	// BT.2020+PQ content (values in [0,1]) losslessly for the passthrough path.
	// SDR: prefer 8-bit ABGR/XBGR to match Vulkan WSI and avoid GL R↔B channel swaps.
	// Vulkan WSI on Wayland defaults to XBGR/ABGR formats, so using ARGB causes
	// GL to incorrectly swap red/blue channels during blit operations.
	let select_format = |preferred_fourccs: &[Fourcc]| {
		preferred_fourccs.iter().find_map(|&fourcc| {
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
	};
	let sdr_render_format = select_format(&[Fourcc::Abgr8888, Fourcc::Xbgr8888, Fourcc::Argb8888, Fourcc::Xrgb8888])
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
	let hdr_render_format = config
		.hdr
		.then(|| select_format(&[Fourcc::Abgr16161616f, Fourcc::Abgr2101010]))
		.flatten();
	let hdr_capable = hdr_render_format.is_some();
	let hdr = context.hdr && hdr_capable;
	let (render_fourcc, render_modifiers) = if hdr {
		hdr_render_format.clone().expect("HDR capability checked")
	} else {
		sdr_render_format.clone()
	};

	tracing::debug!(
		"Selected render format: {:?} with {} modifier(s)",
		render_fourcc,
		render_modifiers.len()
	);

	if config.hdr && context.hdr && !hdr {
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
		size: (context.width as i32, context.height as i32).into(),
		refresh: (context.refresh_rate * 1000) as i32,
	};

	// Synthesize plausible physical dimensions so games that check the
	// display's physical size (e.g. Ghost of Tsushima) see a valid monitor
	// instead of a 0x0mm virtual output. Approximate a 27" display at
	// the configured resolution's aspect ratio, but fall back to 16:9 if
	// the configured dimensions are invalid.
	let diag_mm = 686.0_f64; // 27 inches in mm
	let aspect = if context.width == 0 || context.height == 0 {
		16.0_f64 / 9.0_f64
	} else {
		context.width as f64 / context.height as f64
	};
	let h_mm = ((diag_mm / (1.0 + aspect * aspect).sqrt()) as i32).max(1);
	let w_mm = ((h_mm as f64 * aspect) as i32).max(1);

	let output = Output::new(
		"moonshine-virtual".to_string(),
		PhysicalProperties {
			size: (w_mm, h_mm).into(),
			subpixel: Subpixel::Unknown,
			make: "Moonshine".into(),
			model: "Virtual Output".into(),
			serial_number: "".into(),
		},
	);
	output.change_current_state(
		Some(mode),
		Some(Transform::Normal),
		Some(Scale::Fractional(context.output_scale)),
		Some((0, 0).into()),
	);
	output.set_preferred(mode);

	// Create the damage tracker for this output.
	let damage_tracker = OutputDamageTracker::from_output(&output);

	// Build the compositor state.
	let (mut state, display) = MoonshineCompositor::new(
		display,
		display_handle.clone(),
		event_loop.handle(),
		output,
		damage_tracker,
		gbm_allocator,
		renderer,
		frame_tx,
		context.width,
		context.height,
		render_fourcc,
		render_modifiers,
		sdr_render_format,
		hdr_render_format,
		ready_tx,
		&render_node,
		hdr,
		hdr_capable,
		config.steam_mode,
		config.virtual_connector_strategy,
		config.keyboard.clone(),
		config.capture_mode,
		context.log_stats,
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

	let refresh_rate = Arc::new(AtomicU32::new(context.refresh_rate.max(1)));
	let reconfigured_refresh_rate = refresh_rate.clone();
	event_loop
		.handle()
		.insert_source(reconfigure_rx, move |event, _, state: &mut MoonshineCompositor| {
			if let calloop::channel::Event::Msg(request) = event {
				let result = state.reconfigure_output(request.width, request.height, request.refresh_rate, request.hdr);
				if result.is_ok() {
					reconfigured_refresh_rate.store(request.refresh_rate.max(1), Ordering::Release);
				}
				let effective_hdr = result.map(|()| state.hdr);
				let _ = request.applied.send(effective_hdr);
			}
		})
		.map_err(|e| format!("Failed to insert compositor reconfiguration channel: {e}"))?;

	// Set up the frame timer.
	// Use Instant-based absolute scheduling so that render time inside
	// the callback doesn't drift the cadence. `ToDuration` would add the
	// interval *after* the callback returns, progressively skewing the
	// actual period and producing ~58 Hz instead of 60 Hz.
	let frame_nanos: u64 = 1_000_000_000u64 / u64::from(context.refresh_rate.max(1));
	let frame_interval = std::time::Duration::from_nanos(frame_nanos);
	// Both timers use the same grid. Demand and a temporary paced-send stall
	// must not create a persistent phase offset from application callbacks.
	state.next_refresh_at = std::time::Instant::now() + frame_interval;
	state.next_capture_at = state.next_refresh_at;
	let timer = smithay::reexports::calloop::timer::Timer::from_deadline(state.next_refresh_at);
	event_loop
		.handle()
		.insert_source(timer, move |_event, _metadata, state: &mut MoonshineCompositor| {
			// Type a bounded batch of any clipboard text queued since the last tick.
			input::drain_pending_text(state);
			state.render_and_export(true);
			// Schedule the next frame relative to the ideal wall-clock
			// target, not relative to "now". This absorbs render-time
			// jitter and keeps a steady cadence.
			let interval = std::time::Duration::from_nanos(
				1_000_000_000u64 / u64::from(refresh_rate.load(Ordering::Acquire).max(1)),
			);
			state.next_refresh_at =
				capture::next_capture_deadline(state.next_refresh_at, std::time::Instant::now(), interval);
			smithay::reexports::calloop::timer::TimeoutAction::ToInstant(state.next_refresh_at)
		})
		.map_err(|e| format!("Failed to insert frame timer: {e}"))?;

	if let Some(demand) = capture_demand {
		// One preallocated timer, rearmed by coalesced consumer demand. Keep the
		// lifecycle timer above so blocked capture never blocks Wayland clients.
		let capture_timer = calloop::Dispatcher::new(
			calloop::timer::Timer::from_duration(std::time::Duration::from_secs(86400)),
			|_, _, state: &mut MoonshineCompositor| {
				state.render_and_export(false);
				calloop::timer::TimeoutAction::ToDuration(std::time::Duration::from_secs(86400))
			},
		);
		let handle = event_loop.handle();
		let token = handle
			.register_dispatcher(capture_timer.clone())
			.map_err(|e| format!("Failed to register capture demand timer: {e}"))?;
		event_loop
			.handle()
			.insert_source(demand, move |_, _, state: &mut MoonshineCompositor| {
				if state.frame_tx.requested() {
					capture_timer
						.as_source_mut()
						.set_deadline(state.next_capture_at.max(std::time::Instant::now()));
					if let Err(error) = handle.update(&token) {
						tracing::warn!(%error, "Failed to rearm capture demand timer");
					}
				}
			})
			.map_err(|e| format!("Failed to register capture demand wakeup: {e}"))?;
	}

	tracing::info!(
		"Compositor started: {}x{} @ {}Hz, output scale {}",
		context.width,
		context.height,
		context.refresh_rate,
		context.output_scale
	);

	// Run the event loop.
	// Use `None` as timeout so dispatch blocks until the next calloop
	// source fires (frame timer, input channel, or Wayland client event).
	// A hard timeout like 16ms would compete with the frame timer cadence.
	state.start_xwayland();

	tracing::debug!(
		shutdown_triggered = stop.is_shutdown_triggered(),
		"Entering compositor event loop"
	);

	while !stop.is_shutdown_triggered() {
		event_loop
			.dispatch(None, &mut state)
			.map_err(|e| format!("Event loop dispatch error: {e}"))?;
		if state.capture_failed {
			let _ = stop.trigger_shutdown(SessionShutdownReason::CompositorStopped);
		}
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
///
/// Delegates to the shared implementation in the healthcheck module.
fn find_render_node(gpu_config: &Option<String>) -> Result<std::path::PathBuf, String> {
	crate::healthcheck::find_render_node(gpu_config)
}

#[cfg(test)]
mod tests {
	use super::sanitize_output_scale;

	#[test]
	fn output_scale_accepts_fractional_values() {
		assert_eq!(sanitize_output_scale(Some(1.5)), 1.5);
		assert_eq!(sanitize_output_scale(Some(2.0)), 2.0);
	}

	#[test]
	fn output_scale_defaults_or_rejects_invalid_values() {
		assert_eq!(sanitize_output_scale(None), 1.0);
		assert_eq!(sanitize_output_scale(Some(0.0)), 1.0);
		assert_eq!(sanitize_output_scale(Some(f64::NAN)), 1.0);
		assert_eq!(sanitize_output_scale(Some(9.0)), 1.0);
	}
}
