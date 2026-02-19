//! Compositor state and Smithay protocol handler implementations.
//!
//! `MoonshineCompositor` is the central state struct for the headless compositor.
//! All Smithay `delegate_*!` macros target this struct.

use std::os::unix::io::OwnedFd;
use std::sync::mpsc;

use smithay::backend::allocator::dmabuf::AsDmabuf;
use smithay::backend::allocator::gbm::GbmAllocator;
use smithay::backend::allocator::{Allocator, Buffer, Fourcc, Modifier};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Element, Id, RenderElement};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::{Bind, ImportDma};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::wayland::compositor;
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::Space;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Clock, IsAlive, Logical, Monotonic, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Wm;

use super::cursor::{self, PointerElement, PointerRenderElement};
use super::frame::{ExportedFrame, ExportedPlane};

// Combined render element type for compositing space + cursor elements.
// We use GlesRenderer concretely (no generics) to avoid complex trait bound issues.
pub enum OutputRenderElements {
	Space(SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>),
	Pointer(PointerRenderElement<GlesRenderer>),
}

impl Element for OutputRenderElements {
	fn id(&self) -> &Id {
		match self {
			Self::Space(e) => e.id(),
			Self::Pointer(e) => e.id(),
		}
	}

	fn current_commit(&self) -> CommitCounter {
		match self {
			Self::Space(e) => e.current_commit(),
			Self::Pointer(e) => e.current_commit(),
		}
	}

	fn geometry(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Rectangle<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.geometry(scale),
			Self::Pointer(e) => e.geometry(scale),
		}
	}

	fn src(&self) -> smithay::utils::Rectangle<f64, smithay::utils::Buffer> {
		match self {
			Self::Space(e) => e.src(),
			Self::Pointer(e) => e.src(),
		}
	}

	fn location(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Point<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.location(scale),
			Self::Pointer(e) => e.location(scale),
		}
	}

	fn transform(&self) -> smithay::utils::Transform {
		match self {
			Self::Space(e) => e.transform(),
			Self::Pointer(e) => e.transform(),
		}
	}

	fn damage_since(&self, scale: smithay::utils::Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.damage_since(scale, commit),
			Self::Pointer(e) => e.damage_since(scale, commit),
		}
	}

	fn opaque_regions(&self, scale: smithay::utils::Scale<f64>) -> OpaqueRegions<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.opaque_regions(scale),
			Self::Pointer(e) => e.opaque_regions(scale),
		}
	}

	fn alpha(&self) -> f32 {
		match self {
			Self::Space(e) => e.alpha(),
			Self::Pointer(e) => e.alpha(),
		}
	}

	fn kind(&self) -> smithay::backend::renderer::element::Kind {
		match self {
			Self::Space(e) => e.kind(),
			Self::Pointer(e) => e.kind(),
		}
	}
}

impl RenderElement<GlesRenderer> for OutputRenderElements {
	fn draw(
		&self,
		frame: &mut GlesFrame<'_, '_>,
		src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
		dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
		damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
		opaque_regions: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
	) -> Result<(), GlesError> {
		match self {
			Self::Space(e) => e.draw(frame, src, dst, damage, opaque_regions),
			Self::Pointer(e) => e.draw(frame, src, dst, damage, opaque_regions),
		}
	}

	fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
		match self {
			Self::Space(e) => e.underlying_storage(renderer),
			Self::Pointer(e) => e.underlying_storage(renderer),
		}
	}
}

impl From<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> for OutputRenderElements {
	fn from(e: SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>) -> Self {
		Self::Space(e)
	}
}

impl From<PointerRenderElement<GlesRenderer>> for OutputRenderElements {
	fn from(e: PointerRenderElement<GlesRenderer>) -> Self {
		Self::Pointer(e)
	}
}

/// Central compositor state for Moonshine's headless compositor.
///
/// Runs on a dedicated calloop thread. All Smithay delegate_*! macros
/// target this struct. No physical display — frames are rendered to
/// GBM buffers and exported to the video encoder.
#[allow(dead_code)]
pub struct MoonshineCompositor {
	// -- Wayland plumbing --
	pub display_handle: DisplayHandle,
	pub compositor_state: CompositorState,
	pub shm_state: ShmState,
	pub xdg_shell_state: XdgShellState,
	pub seat_state: SeatState<Self>,
	pub output_manager_state: OutputManagerState,
	pub data_device_state: DataDeviceState,

	// -- Rendering --
	pub output: Output,
	pub damage_tracker: OutputDamageTracker,
	pub allocator: GbmAllocator<std::fs::File>,
	pub renderer: GlesRenderer,

	// -- DMA-BUF --
	pub dmabuf_state: DmabufState,
	pub dmabuf_global: DmabufGlobal,

	// -- Frame relay to encoder --
	pub frame_tx: mpsc::SyncSender<ExportedFrame>,

	// -- Input --
	pub seat: Seat<Self>,

	// -- Cursor --
	pub cursor_position: Point<f64, Logical>,
	pub cursor_status: CursorImageStatus,
	pub pointer_element: PointerElement,

	// -- Desktop --
	pub space: Space<smithay::desktop::Window>,
	pub clock: Clock<Monotonic>,

	// -- Lifecycle --
	pub handle: LoopHandle<'static, Self>,

	// -- Frame dimensions --
	pub width: u32,
	pub height: u32,

	// -- Render format --
	pub render_fourcc: Fourcc,
	pub render_modifiers: Vec<Modifier>,

	// -- XWayland --
	pub xwayland_shell_state: XWaylandShellState,
	pub xwm: Option<X11Wm>,
	pub xdisplay: Option<u32>,
	/// Channel to notify the session thread of the XWayland display number
	/// once it becomes ready.
	pub xdisplay_tx: Option<mpsc::SyncSender<u32>>,
}

/// Client state required by Smithay's compositor.
pub struct ClientState {
	pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
	fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}

	fn disconnected(
		&self,
		_client_id: smithay::reexports::wayland_server::backend::ClientId,
		_reason: smithay::reexports::wayland_server::backend::DisconnectReason,
	) {
	}
}

impl MoonshineCompositor {
	/// Create a new compositor state.
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		display: Display<Self>,
		display_handle: DisplayHandle,
		handle: LoopHandle<'static, Self>,
		output: Output,
		damage_tracker: OutputDamageTracker,
		allocator: GbmAllocator<std::fs::File>,
		renderer: GlesRenderer,
		frame_tx: mpsc::SyncSender<ExportedFrame>,
		width: u32,
		height: u32,
		render_fourcc: Fourcc,
		render_modifiers: Vec<Modifier>,
		xdisplay_tx: mpsc::SyncSender<u32>,
		render_node: &std::path::Path,
	) -> (Self, Display<Self>) {
		let compositor_state = CompositorState::new::<Self>(&display_handle);
		let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
		let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
		let mut seat_state = SeatState::new();
		let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
		let data_device_state = DataDeviceState::new::<Self>(&display_handle);
		let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
		RelativePointerManagerState::new::<Self>(&display_handle);
		PointerConstraintsState::new::<Self>(&display_handle);
		let clock = Clock::new();

		let mut space = Space::default();

		// Create seat with keyboard and pointer.
		let mut seat = seat_state.new_wl_seat(&display_handle, "moonshine");
		seat.add_keyboard(Default::default(), 200, 25)
			.expect("Failed to add keyboard to seat");
		seat.add_pointer();

		// Create the Wayland socket for clients to connect.
		let socket_source = ListeningSocketSource::new_auto()
			.expect("Failed to create Wayland listening socket");
		let socket_name = socket_source.socket_name().to_os_string();
		tracing::info!("Wayland socket: {:?}", socket_name);

		// Set WAYLAND_DISPLAY for child processes.
		std::env::set_var("WAYLAND_DISPLAY", &socket_name);

		// Register the socket source with the event loop.
		let mut display_handle_clone = display_handle.clone();
		handle
			.insert_source(socket_source, move |client_stream, _, _state| {
				tracing::info!("New Wayland client connected");
				if let Err(e) = display_handle_clone.insert_client(
					client_stream,
					std::sync::Arc::new(ClientState {
						compositor_state: CompositorClientState::default(),
					}),
				) {
					tracing::error!("Failed to insert client: {e}");
				}
			})
			.expect("Failed to register socket source");

		// Create the output global so clients can see it.
		let _output_global = output.create_global::<Self>(&display_handle);

		// Map the output in the space so that space_render_elements()
		// knows the output geometry and can associate mapped windows
		// with it. Without this, no render elements are produced.
		space.map_output(&output, (0, 0));

		// Advertise wp_linux_dmabuf_v1 (version 5 with device feedback) so
		// Vulkan WSI and other GPU clients can create DMA-BUF-backed
		// wl_buffer objects. NVIDIA's Vulkan WSI requires the feedback
		// protocol to know which device to allocate on.
		let dmabuf_formats = renderer.dmabuf_formats();
		let render_node_dev = std::fs::metadata(render_node)
			.map(|m| {
				use std::os::unix::fs::MetadataExt;
				m.rdev()
			})
			.expect("Failed to get render node device id");
		let default_feedback = DmabufFeedbackBuilder::new(render_node_dev, dmabuf_formats.clone())
			.build()
			.expect("Failed to build DmabufFeedback");
		let mut dmabuf_state = DmabufState::new();
		let dmabuf_global = dmabuf_state.create_global_with_default_feedback::<Self>(&display_handle, &default_feedback);

		// Load the default xcursor and build a PointerElement with it.
		let cursor_buffer = cursor::load_default_cursor();
		let mut pointer_element = PointerElement::default();
		pointer_element.set_buffer(cursor_buffer);

		(Self {
			display_handle,
			compositor_state,
			shm_state,
			xdg_shell_state,
			seat_state,
			output_manager_state,
			data_device_state,
			output,
			damage_tracker,
			allocator,
			renderer,
			dmabuf_state,
			dmabuf_global,
			frame_tx,
			seat,
			cursor_position: Point::from((width as f64 / 2.0, height as f64 / 2.0)),
			cursor_status: CursorImageStatus::default_named(),
			pointer_element,
			space,
			clock,
			handle,
			width,
			height,
			render_fourcc,
			render_modifiers,
			xwayland_shell_state,
			xwm: None,
			xdisplay: None,
			xdisplay_tx: Some(xdisplay_tx),
		}, display)
	}

	/// Render the current scene and export the frame to the encoder.
	pub fn render_and_export(
		&mut self,
	) {
		// Allocate a GBM buffer for this frame.
		let buffer = match self.allocator.create_buffer(
			self.width,
			self.height,
			self.render_fourcc,
			&self.render_modifiers,
		) {
			Ok(buffer) => buffer,
			Err(e) => {
				tracing::error!("Failed to allocate GBM buffer: {e}");
				return;
			},
		};

		// Export the buffer as a Dmabuf so we can bind it for rendering.
		let mut dmabuf = match buffer.export() {
			Ok(dmabuf) => dmabuf,
			Err(e) => {
				tracing::error!("Failed to export GBM buffer as Dmabuf: {e}");
				return;
			},
		};

		// Bind the Dmabuf as a render target.
		let mut framebuffer = match self.renderer.bind(&mut dmabuf) {
			Ok(fb) => fb,
			Err(e) => {
				tracing::error!("Failed to bind Dmabuf for rendering: {e}");
				return;
			},
		};

		// Collect render elements from the space.
		let num_space_elements = self.space.elements().count();
		let space_elements: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> =
			match smithay::desktop::space::space_render_elements(&mut self.renderer, [&self.space], &self.output, 1.0) {
				Ok(elements) => elements,
				Err(e) => {
					tracing::error!("Failed to collect render elements: {e}");
					return;
				},
			};

		// Build cursor render elements.
		// Reset to the default named cursor if the client cursor surface is dead.
		let mut reset = false;
		if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
			reset = !surface.alive();
		}
		if reset {
			self.cursor_status = CursorImageStatus::default_named();
		}

		self.pointer_element.set_status(self.cursor_status.clone());

		let cursor_hotspot = if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
			compositor::with_states(surface, |states| {
				states
					.data_map
					.get::<std::sync::Mutex<CursorImageAttributes>>()
					.unwrap()
					.lock()
					.unwrap()
					.hotspot
			})
		} else {
			(0, 0).into()
		};

		let scale = smithay::utils::Scale::from(1.0);
		let cursor_pos = self.cursor_position;
		let cursor_elements: Vec<OutputRenderElements> = self.pointer_element.render_elements(
			&mut self.renderer,
			(cursor_pos - cursor_hotspot.to_f64())
				.to_physical(scale)
				.to_i32_round(),
			scale,
			1.0,
		);

		// Combine cursor elements (rendered on top) with space elements.
		let mut elements: Vec<OutputRenderElements> = Vec::with_capacity(
			cursor_elements.len() + space_elements.len(),
		);
		elements.extend(cursor_elements);
		elements.extend(space_elements.into_iter().map(OutputRenderElements::Space));

		tracing::trace!(
			num_space_elements,
			num_render_elements = elements.len(),
			"Rendering frame"
		);

		// Render using the damage tracker (full redraw for now, age=0).
		let render_result = self.damage_tracker.render_output(
			&mut self.renderer,
			&mut framebuffer,
			0, // buffer age - 0 forces full redraw
			&elements,
			[0.0, 0.0, 0.0, 1.0], // black clear color
		);

		match render_result {
			Ok(_) | Err(smithay::backend::renderer::damage::Error::OutputNoMode(_)) => {},
			Err(e) => {
				tracing::error!("Failed to render output: {e}");
				return;
			},
		}

		// Drop framebuffer before exporting, to release the mutable borrow on dmabuf.
		drop(framebuffer);

		// Build ExportedFrame from the Dmabuf.
		let exported = export_dmabuf(&dmabuf);
		match exported {
			Ok(frame) => {
				// Send to the encoder — drop the frame if encoder is behind.
				if let Err(mpsc::TrySendError::Disconnected(_)) = self.frame_tx.try_send(frame) {
					tracing::debug!("Frame channel disconnected, compositor stopping.");
				}
			},
			Err(e) => {
				tracing::error!("Failed to export frame: {e}");
			},
		}

		// Send frame callbacks to clients so they know to submit the
		// next buffer.
		self.space.elements().for_each(|window| {
			window.send_frame(
				&self.output,
				self.clock.now(),
				Some(std::time::Duration::ZERO),
				|_, _| Some(self.output.clone()),
			);
		});

		// Flush the frame callbacks (and any other pending events) to
		// clients immediately. Without this, the wl_callback.done events
		// sit in the outgoing buffer until the next Wayland socket
		// activity (e.g. mouse movement), starving the client's present
		// loop.
		if let Err(e) = self.display_handle.flush_clients() {
			tracing::error!("Failed to flush clients after render: {e}");
		}
	}

	/// Start the XWayland server so X11 applications can connect.
	///
	/// Spawns the XWayland process and registers it as a calloop
	/// event source. When XWayland signals readiness, the X11 window
	/// manager is started and DISPLAY is set for child processes.
	pub fn start_xwayland(&mut self) {
		use smithay::wayland::compositor::CompositorHandler;
		use smithay::xwayland::{XWayland, XWaylandEvent};

		// Log XWayland stderr to a file for debugging.
		let log_dir = std::env::temp_dir().join("moonshine");
		let _ = std::fs::create_dir_all(&log_dir);
		let xwayland_log_stderr = std::fs::File::create(log_dir.join("xwayland.log"))
			.map(std::process::Stdio::from)
			.unwrap_or_else(|_| std::process::Stdio::null());
		let xwayland_log_stdout = std::fs::File::create(log_dir.join("xwayland_stdout.log"))
			.map(std::process::Stdio::from)
			.unwrap_or_else(|_| std::process::Stdio::null());

		// Log key environment state before spawning.
		tracing::info!(
			wayland_display = ?std::env::var("WAYLAND_DISPLAY"),
			xdg_runtime_dir = ?std::env::var("XDG_RUNTIME_DIR"),
			"Spawning XWayland"
		);

		let (xwayland, client) = match XWayland::spawn(
			&self.display_handle,
			None,
			[("WAYLAND_DEBUG", "1")],
			true,
			xwayland_log_stdout,
			xwayland_log_stderr,
			|_| (),
		) {
			Ok(result) => result,
			Err(e) => {
				tracing::error!("Failed to spawn XWayland: {e}");
				return;
			},
		};
		tracing::info!(
			display_number = xwayland.display_number(),
			"XWayland process spawned, waiting for readiness"
		);

		let ret = self.handle.insert_source(xwayland, move |event, _, data: &mut MoonshineCompositor| match event {
			XWaylandEvent::Ready {
				x11_socket,
				display_number,
			} => {
				// Set the client compositor scale to 1.0 (no HiDPI scaling for XWayland).
				data.client_compositor_state(&client).set_client_scale(1.0);

				let wm = smithay::xwayland::X11Wm::start_wm(data.handle.clone(), x11_socket, client.clone())
					.expect("Failed to start X11 window manager");

				tracing::info!(display_number, "XWayland ready");
				std::env::set_var("DISPLAY", format!(":{display_number}"));

				data.xwm = Some(wm);
				data.xdisplay = Some(display_number);

				// Notify the session thread that XWayland is ready.
				if let Some(tx) = data.xdisplay_tx.take() {
					let _ = tx.send(display_number);
				}
			},
			XWaylandEvent::Error => {
				tracing::error!("XWayland crashed on startup");
			},
		});

		if let Err(e) = ret {
			tracing::error!("Failed to insert XWayland source into event loop: {e}");
		}
	}
}

/// Convert a Smithay Dmabuf into our pipeline's ExportedFrame.
///
/// We duplicate each plane's fd because Smithay retains ownership of the
/// original fds inside the Dmabuf, and they will be closed when the
/// buffer is recycled. The encoder needs the fds to remain valid
/// until Vulkan's vkAllocateMemory consumes them.
fn export_dmabuf(dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf) -> Result<ExportedFrame, String> {
	let planes: Vec<ExportedPlane> = dmabuf
		.handles()
		.zip(dmabuf.offsets())
		.zip(dmabuf.strides())
		.map(|((handle, offset), stride)| {
			let dup_fd: OwnedFd = handle
				.try_clone_to_owned()
				.map_err(|e| format!("Failed to duplicate DMA-BUF fd: {e}"))?;
			Ok(ExportedPlane {
				fd: dup_fd,
				offset,
				stride,
			})
		})
		.collect::<Result<Vec<_>, String>>()?;

	Ok(ExportedFrame {
		planes,
		format: dmabuf.format().code as u32,
		modifier: Into::<u64>::into(dmabuf.format().modifier),
		width: dmabuf.width() as u32,
		height: dmabuf.height() as u32,
	})
}
