//! Smithay protocol handler implementations for MoonshineCompositor.
//!
//! These are the minimum required delegate implementations for a working
//! Wayland compositor with XWayland support.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Buffer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::backend::renderer::ImportDma;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_dmabuf;
use smithay::delegate_output;
use smithay::delegate_pointer_constraints;
use smithay::delegate_relative_pointer;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::delegate_xwayland_shell;
use smithay::desktop::Window;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler};
use smithay::wayland::selection::data_device::{
	ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};

use smithay::xwayland::XWaylandClientData;

use super::focus::KeyboardFocusTarget;
use super::state::{ClientState, MoonshineCompositor};

// -- Buffer Handler --

impl BufferHandler for MoonshineCompositor {
	fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

// -- SHM Handler --

impl ShmHandler for MoonshineCompositor {
	fn shm_state(&self) -> &ShmState {
		&self.shm_state
	}
}

// -- Compositor Handler --

impl CompositorHandler for MoonshineCompositor {
	fn compositor_state(&mut self) -> &mut CompositorState {
		&mut self.compositor_state
	}

	fn client_compositor_state<'a>(
		&self,
		client: &'a smithay::reexports::wayland_server::Client,
	) -> &'a CompositorClientState {
		// XWayland clients use XWaylandClientData; regular Wayland
		// clients use our ClientState. Try both.
		if let Some(state) = client.get_data::<ClientState>() {
			return &state.compositor_state;
		}
		if let Some(state) = client.get_data::<XWaylandClientData>() {
			return &state.compositor_state;
		}
		panic!("Client has neither ClientState nor XWaylandClientData");
	}

	fn commit(&mut self, surface: &WlSurface) {
		tracing::trace!("Surface commit");
		// Mark the screen as dirty so the next timer tick renders and sends a frame.
		self.screen_dirty = true;

		// Initialize RendererSurfaceState for this surface so that
		// space_render_elements can produce render elements from the
		// attached buffer. Without this call surfaces appear bufferless
		// and the rendered frame is always blank.
		on_commit_buffer_handler::<Self>(surface);

		// Ensure the surface is not a pending sync subsurface.
		if is_sync_subsurface(surface) {
			return;
		}

		// If the surface is a toplevel, refresh the space.
		if let Some(window) = self
			.space
			.elements()
			.find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
			.cloned()
		{
			window.on_commit();
		}

		// Handle popup commits.
		self.popups_commit(surface);
	}
}

impl MoonshineCompositor {
	fn popups_commit(&mut self, _surface: &WlSurface) {
		// Popup handling can be added later if needed.
	}

	/// Set keyboard focus to the given window.
	///
	/// For a game streaming compositor there is typically a single fullscreen
	/// application, so we always grant focus to the most recently mapped window.
	///
	/// Uses `KeyboardFocusTarget::Window` so that X11 windows receive
	/// `XSetInputFocus` via the `X11Surface` `KeyboardTarget` impl.
	pub(super) fn set_keyboard_focus_to_window(&mut self, window: &Window) {
		tracing::debug!("Setting keyboard focus to window.");
		let serial = smithay::utils::SERIAL_COUNTER.next_serial();
		if let Some(keyboard) = self.seat.get_keyboard() {
			keyboard.set_focus(self, Some(KeyboardFocusTarget::Window(window.clone())), serial);
		}
	}
}

// -- XDG Shell Handler --

impl XdgShellHandler for MoonshineCompositor {
	fn xdg_shell_state(&mut self) -> &mut XdgShellState {
		&mut self.xdg_shell_state
	}

	fn new_toplevel(&mut self, surface: ToplevelSurface) {
		tracing::debug!("New XDG toplevel mapped in space.");

		// Tell the client the desired surface size so Vulkan WSI can
		// create a swapchain. Without an initial configure the client
		// blocks indefinitely waiting for the compositor to propose a
		// size.
		surface.with_pending_state(|state| {
			state.size = Some((self.width as i32, self.height as i32).into());
		});
		surface.send_configure();

		let window = Window::new_wayland_window(surface);
		self.space.map_element(window.clone(), (0, 0), false);

		// Give the new toplevel keyboard focus so it receives key events.
		self.set_keyboard_focus_to_window(&window);
	}

	fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
		// Popup handling can be added later.
	}

	fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
		// Popup grabs can be added later.
	}

	fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {
		// Repositioning can be added later.
	}
}

// -- Seat Handler --

impl SeatHandler for MoonshineCompositor {
	type KeyboardFocus = KeyboardFocusTarget;
	type PointerFocus = WlSurface;
	type TouchFocus = WlSurface;

	fn seat_state(&mut self) -> &mut SeatState<Self> {
		&mut self.seat_state
	}

	fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
		tracing::trace!(?image, "Cursor image changed");
		self.cursor_status = image;
	}

	fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&KeyboardFocusTarget>) {
		tracing::debug!(?focused, "Keyboard focus changed");
	}

	fn led_state_changed(&mut self, _seat: &Seat<Self>, _led_state: smithay::input::keyboard::LedState) {}
}

// -- Selection / Data Device Handlers --

impl SelectionHandler for MoonshineCompositor {
	type SelectionUserData = ();
}

impl DataDeviceHandler for MoonshineCompositor {
	fn data_device_state(&self) -> &DataDeviceState {
		&self.data_device_state
	}
}

impl ClientDndGrabHandler for MoonshineCompositor {}
impl ServerDndGrabHandler for MoonshineCompositor {}

// -- Output Handler --

impl OutputHandler for MoonshineCompositor {
	fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

// -- Pointer Constraints Handler --

impl PointerConstraintsHandler for MoonshineCompositor {
	fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
		// Auto-activate constraints when the surface already has pointer focus.
		if let Some(current_focus) = pointer.current_focus() {
			if &current_focus == surface {
				with_pointer_constraint(surface, pointer, |constraint| {
					constraint.unwrap().activate();
				});
			}
		}
	}

	fn cursor_position_hint(
		&mut self,
		surface: &WlSurface,
		pointer: &PointerHandle<Self>,
		location: Point<f64, Logical>,
	) {
		if with_pointer_constraint(surface, pointer, |constraint| constraint.is_some_and(|c| c.is_active())) {
			let origin = self
				.space
				.elements()
				.find_map(|window| {
					use smithay::wayland::seat::WaylandFocus;
					(window.wl_surface().as_deref() == Some(surface)).then(|| window.geometry())
				})
				.unwrap_or_default()
				.loc
				.to_f64();

			pointer.set_location(origin + location);
		}
	}
}

// -- Delegate macros --

delegate_compositor!(MoonshineCompositor);
delegate_dmabuf!(MoonshineCompositor);
delegate_shm!(MoonshineCompositor);
delegate_xdg_shell!(MoonshineCompositor);
delegate_seat!(MoonshineCompositor);
delegate_data_device!(MoonshineCompositor);
delegate_output!(MoonshineCompositor);
delegate_relative_pointer!(MoonshineCompositor);
delegate_pointer_constraints!(MoonshineCompositor);
delegate_xwayland_shell!(MoonshineCompositor);

// -- DMA-BUF Handler --

impl DmabufHandler for MoonshineCompositor {
	fn dmabuf_state(&mut self) -> &mut DmabufState {
		&mut self.dmabuf_state
	}

	fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
		tracing::debug!(format = ?dmabuf.format(), "DMA-BUF import requested");
		if self.renderer.import_dmabuf(&dmabuf, None).is_ok() {
			tracing::debug!("DMA-BUF import successful");
			let _ = notifier.successful::<MoonshineCompositor>();
		} else {
			tracing::warn!("DMA-BUF import failed");
			notifier.failed();
		}
	}
}

// -- XWayland Shell Handler --

impl XWaylandShellHandler for MoonshineCompositor {
	fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
		&mut self.xwayland_shell_state
	}
}

// -- XWM Handler (X11 Window Manager) --

impl XwmHandler for MoonshineCompositor {
	fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
		self.xwm.as_mut().expect("XWayland WM not initialized")
	}

	fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

	fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

	fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			title = ?window.title(),
			class = ?window.class(),
			override_redirect = window.is_override_redirect(),
			wl_surface = ?window.wl_surface(),
			"X11 window map request"
		);

		// Configure the X11 window to fill the output.
		let geo = Rectangle::new((0, 0).into(), (self.width as i32, self.height as i32).into());
		if let Err(e) = window.configure(geo) {
			tracing::warn!("Failed to configure X11 window geometry: {e}");
		}

		// Mark the window as fullscreen so the client doesn't draw
		// resize borders or window decorations.
		if let Err(e) = window.set_fullscreen(true) {
			tracing::warn!("Failed to set X11 window fullscreen: {e}");
		}

		// Grant the map request.
		if let Err(e) = window.set_mapped(true) {
			tracing::error!("Failed to set X11 window mapped: {e}");
			return;
		}
		let win = Window::new_x11_window(window);
		self.space.map_element(win.clone(), (0, 0), true);

		// Give the new X11 window keyboard focus.
		self.set_keyboard_focus_to_window(&win);

		// Log all space elements after mapping for debugging.
		for (i, elem) in self.space.elements().enumerate() {
			let x11_info = elem
				.x11_surface()
				.map(|x| (x.title(), x.class(), x.is_override_redirect(), x.wl_surface()));
			tracing::debug!(i, ?x11_info, loc = ?self.space.element_location(elem), "Space element after map");
		}
	}

	fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::info!(
			title = ?window.title(),
			class = ?window.class(),
			geometry = ?window.geometry(),
			"X11 override-redirect window mapped"
		);
		let location = window.geometry().loc;
		let win = Window::new_x11_window(window);
		self.space.map_element(win.clone(), location, true);

		// Don't steal keyboard focus for override-redirect windows.
		// These are typically tooltips, menus, or overlay windows (like
		// the Steam overlay) and should not take focus away from the
		// game window.
	}

	fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
		let maybe = self
			.space
			.elements()
			.find(|e| e.x11_surface().map(|x| x == &window).unwrap_or(false))
			.cloned();
		if let Some(elem) = maybe {
			self.space.unmap_elem(&elem);
		}
		if !window.is_override_redirect() {
			let _ = window.set_mapped(false);
		}
	}

	fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

	fn configure_request(
		&mut self,
		_xwm: XwmId,
		window: X11Surface,
		_x: Option<i32>,
		_y: Option<i32>,
		w: Option<u32>,
		h: Option<u32>,
		_reorder: Option<Reorder>,
	) {
		// Grant geometry changes but ignore position (we control placement).
		let mut geo = window.geometry();
		if let Some(w) = w {
			geo.size.w = w as i32;
		}
		if let Some(h) = h {
			geo.size.h = h as i32;
		}
		let _ = window.configure(geo);
	}

	fn configure_notify(
		&mut self,
		_xwm: XwmId,
		window: X11Surface,
		geometry: Rectangle<i32, Logical>,
		_above: Option<u32>,
	) {
		// Update position in space if the window moved.
		let Some(elem) = self
			.space
			.elements()
			.find(|e| e.x11_surface().map(|x| x == &window).unwrap_or(false))
			.cloned()
		else {
			return;
		};
		self.space.map_element(elem, geometry.loc, false);
	}

	fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {
		// Interactive resize not needed for headless compositor.
	}

	fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
		// Interactive move not needed for headless compositor.
	}
}
