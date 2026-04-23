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
use smithay::delegate_presentation;
use smithay::delegate_relative_pointer;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_viewporter;
use smithay::delegate_xdg_shell;
use smithay::delegate_xwayland_shell;
use smithay::desktop::{Window, WindowSurface};
use smithay::desktop::WindowSurfaceType;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::IsAlive;
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
use smithay::xwayland::xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};

use smithay::xwayland::XWaylandClientData;

use super::focus::KeyboardFocusTarget;
use super::state::{ClientState, MoonshineCompositor};

use smithay::reexports::x11rb;
use smithay::reexports::x11rb::connection::Connection as _;
use smithay::reexports::x11rb::protocol::xproto::{ConnectionExt as _, InputFocus, PropMode};

// -- Buffer Handler --

impl BufferHandler for MoonshineCompositor {
	fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

/// Read the Steam game app ID from `/proc/{pid}/environ`.
///
/// Proton games launched via Steam inherit `SteamGameId` (or the older
/// `SteamAppId`) in their process environment.  This provides the numeric
/// app ID for games whose X11 window class is the executable name (e.g.
/// `"bg3"`) rather than the conventional `steam_app_<id>` form.
fn steam_app_id_from_pid(pid: u32) -> Option<u32> {
	let data = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
	for var in data.split(|&b| b == 0) {
		for prefix in [b"SteamGameId=".as_slice(), b"SteamAppId=".as_slice()] {
			if let Some(val) = var.strip_prefix(prefix) {
				if let Ok(id) = std::str::from_utf8(val).unwrap_or("").parse::<u32>() {
					return Some(id);
				}
			}
		}
	}
	None
}

/// Return the `starttime` field (field 22) from `/proc/{pid}/stat`.
///
/// Used as the second half of the `(pid, starttime)` key in
/// `pid_app_id_cache` to detect PID reuse: a recycled PID will have a
/// different start time than the original process.
fn pid_start_time(pid: u32) -> Option<u64> {
	let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
	// Field 2 is the process name in parentheses, which may itself contain
	// spaces and parentheses.  Find the last ')' to skip it reliably.
	let after_comm = contents.rsplit_once(')')?.1;
	// Remaining fields begin at field 3 (state).  Field 22 (starttime) is
	// index 19 from there (22 - 3 = 19, 0-indexed).
	after_comm.split_whitespace().nth(19).and_then(|s| s.parse().ok())
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
		// Mark the screen as dirty so the next timer tick renders and sends a frame.
		self.screen_dirty = true;

		// Apply pending color management state.
		if let Some(cm) = &mut self.color_management {
			cm.commit(surface);
		}

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

	fn destroyed(&mut self, surface: &WlSurface) {
		tracing::debug!(surface_id = ?surface.id(), "surface destroyed");
		if let Some(cm) = &mut self.color_management {
			cm.surface_destroyed(surface);
		}

		// Evict any dead windows from the space. Wayland toplevel windows
		// have no explicit unmap callback, so they must be cleaned up here
		// when their underlying surface is destroyed.
		let dead: Vec<_> = self.space.elements().filter(|w| !w.alive()).cloned().collect();
		for w in &dead {
			self.space.unmap_elem(w);
		}
		if !dead.is_empty() {
			self.determine_and_apply_focus();
		}
	}
}

const STEAM_BPM_APP_ID: u32 = 769;

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
	///
	/// When an X11 window lacks a `wl_surface` (e.g. before XWayland's serial
	/// matching completes), keyboard events are proxied through another
	/// XWayland surface temporarily. Once `surface_associated` fires and the
	/// wl_surface becomes available, this function is called again to switch
	/// to the direct `Window` target.
	///
	/// After setting Smithay keyboard focus, this immediately synchronizes
	/// X11 input focus via `SetInputFocus` and updates `_NET_ACTIVE_WINDOW`
	/// on the root window. This is necessary because Wine under the
	/// `steamcompmgr` WM identity processes `FocusIn` events directly
	/// (bypassing `WM_TAKE_FOCUS`) and expects the WM to call
	/// `XSetInputFocus` explicitly.
	pub(super) fn set_keyboard_focus_to_window(&mut self, window: &Window) {
		let serial = smithay::utils::SERIAL_COUNTER.next_serial();

		// Update X11 focus state to match the new target window type.
		if let Some(x11) = window.x11_surface() {
			self.focused_x11_window = Some(x11.window_id());
		} else {
			// Native Wayland window — clear X11 state so sync_x11_focus() doesn't
			// call SetInputFocus on BPM's X11 window or leave GAMESCOPE_FOCUSED_APP
			// set to 769, which would cause Steam to keep claiming controller input.
			self.focused_x11_window = None;
			if self.focused_app_id == STEAM_BPM_APP_ID {
				self.focused_app_id = 0;
			}
		}

		// If the X11 window has no wl_surface yet (serial matching still
		// in progress), proxy keyboard events through another XWayland surface.
		if let Some(x11) = window.x11_surface() {
			if x11.wl_surface().is_none() {
				if let Some(proxy_surface) = self.find_xwayland_proxy_surface() {
					tracing::debug!(
						window_id = x11.window_id(),
						"X11 window has no wl_surface yet, using proxy for keyboard delivery"
					);
					let target = KeyboardFocusTarget::ProxiedX11 {
						window: window.clone(),
						proxy_surface,
					};
					if let Some(keyboard) = self.seat.get_keyboard() {
						keyboard.set_focus(self, Some(target), serial);
					}
					// Set X11 focus immediately even in proxy mode.
					// SetInputFocus works on X11 window IDs and doesn't
					// need a wl_surface. This eliminates the gap where
					// the old window lost focus but the new window hadn't
					// gained X11 focus yet.
					self.sync_x11_focus();
					// Still schedule a reset so surface_associated can
					// upgrade from ProxiedX11 to direct Window focus.
					self.x11_focus_needs_reset = true;
					return;
				}
				tracing::warn!("No XWayland proxy surface found for keyboard delivery");
			} else {
				tracing::debug!(
					window_id = x11.window_id(),
					wl_surface = ?x11.wl_surface().map(|s| s.id()),
					"Setting keyboard focus to X11 window with wl_surface"
				);
			}
		}

		if let Some(keyboard) = self.seat.get_keyboard() {
			keyboard.set_focus(self, Some(KeyboardFocusTarget::Window(window.clone())), serial);
		}

		// Synchronize X11 focus immediately.
		self.sync_x11_focus();
	}

	/// Synchronize X11 input focus with the current `focused_x11_window`.
	///
	/// Sets both `SetInputFocus` and `_NET_ACTIVE_WINDOW` so Wine's
	/// `NtUserSetForegroundWindow` gets called via the `FocusIn` event.
	pub(super) fn sync_x11_focus(&mut self) {
		let Some((conn, root, atoms)) = &self.x11_input_conn else {
			tracing::warn!("sync_x11_focus: no x11_input_conn");
			return;
		};
		// X11-specific focus operations only apply when an X11 window is focused.
		// When a native Wayland window has focus, skip SetInputFocus and
		// _NET_ACTIVE_WINDOW but still update GAMESCOPE_FOCUSED_APP below so
		// Steam yields controller focus to the game.
		if let Some(win_id) = self.focused_x11_window {
			tracing::debug!(window = win_id, root = root, "sync_x11_focus: setting X11 focus and atoms");

			match conn.set_input_focus(InputFocus::PARENT, win_id, x11rb::CURRENT_TIME) {
				Ok(cookie) => {
					if let Err(e) = cookie.check() {
						tracing::warn!(window = win_id, error = %e, "sync_x11_focus: SetInputFocus X11 error");
					}
				},
				Err(e) => tracing::warn!(window = win_id, error = %e, "sync_x11_focus: SetInputFocus connection error"),
			}

			let _ = conn.change_property(
				PropMode::REPLACE,
				*root,
				atoms.net_active_window,
				atoms.xa_window,
				32,
				1,
				&win_id.to_ne_bytes(),
			);
		} else {
			tracing::debug!(root = root, "sync_x11_focus: no X11 window focused, updating gamescope atoms only");
		}

		let app_id = self.focused_app_id;

		// Set GAMESCOPE_FOCUSABLE_APPS and GAMESCOPE_FOCUSABLE_WINDOWS BEFORE
		// the focused app atom. Steam monitors these to determine which apps
		// are running and focusable. Setting them first ensures the focused
		// app ID is already in the focusable list when Steam processes the
		// focused app change via PropertyNotify.
		let mut focusable_appids: Vec<u32> = Vec::new();
		let mut focusable_windows: Vec<u32> = Vec::new();

		for win in self.space.elements() {
			let Some(x11) = win.x11_surface() else {
				continue;
			};
			if x11.is_override_redirect() {
				continue;
			}

			let class = x11.class();
			let window_app_id: u32 = class
				.strip_prefix("steam_app_")
				.and_then(|s| s.parse().ok())
				.unwrap_or_else(|| {
					if class.eq_ignore_ascii_case("steam") {
						STEAM_BPM_APP_ID
					} else {
						x11.pid().unwrap_or(0)
					}
				});
			// Look up via cache if we only have a PID (no steam_app_ class prefix).
			let window_app_id = if window_app_id == 0 {
				x11.pid()
					.map(|pid| {
						let key = (pid, pid_start_time(pid).unwrap_or(0));
						*self
							.pid_app_id_cache
							.entry(key)
							.or_insert_with(|| steam_app_id_from_pid(pid).unwrap_or(0))
					})
					.unwrap_or(0)
			} else {
				window_app_id
			};

			if window_app_id != 0 && !focusable_appids.contains(&window_app_id) {
				focusable_appids.push(window_app_id);
			}

			// [window_id, appid, pid] triplet — matches gamescope format.
			focusable_windows.push(x11.window_id());
			focusable_windows.push(window_app_id);
			focusable_windows.push(x11.pid().unwrap_or(0));
		}

		{
			let data: Vec<u8> = focusable_appids.iter().flat_map(|id| id.to_ne_bytes()).collect();
			let _ = conn.change_property(
				PropMode::REPLACE,
				*root,
				atoms.gamescope_focusable_apps,
				atoms.xa_cardinal,
				32,
				focusable_appids.len() as u32,
				&data,
			);
		}
		{
			let data: Vec<u8> = focusable_windows.iter().flat_map(|id| id.to_ne_bytes()).collect();
			let _ = conn.change_property(
				PropMode::REPLACE,
				*root,
				atoms.gamescope_focusable_windows,
				atoms.xa_cardinal,
				32,
				focusable_windows.len() as u32,
				&data,
			);
		}

		{
			let data: &[u8] = if app_id != 0 { &app_id.to_ne_bytes() } else { &[] };
			let len = if app_id != 0 { 1u32 } else { 0u32 };
			let _ = conn.change_property(
				PropMode::REPLACE,
				*root,
				atoms.gamescope_focused_app,
				atoms.xa_cardinal,
				32,
				len,
				data,
			);
		}
		let _ = conn.flush();
		self.x11_focus_needs_reset = false;
	}

	/// Determine the best focus candidate and apply focus atomically.
	///
	/// This is modeled on gamescope's `DetermineAndApplyFocus`. It iterates
	/// all windows, classifies them, selects the best focus candidate using
	/// a priority system, and applies both Wayland and X11 focus in one step.
	///
	/// Priority: game window (steam_app_*) > other non-override windows > nothing.
	pub(super) fn determine_and_apply_focus(&mut self) {
		// Three priority levels for focus candidates:
		//   1. steam_app_XXXXX  — the launched game itself (highest priority)
		//   2. any other non-Steam window (e.g. class="bg3") — may be a game
		//      that creates its own X11 class name rather than using Steam's
		//   3. Steam Big Picture Mode (class="steam") — fallback only
		let mut game_window: Option<Window> = None;
		let mut non_steam_window: Option<Window> = None;
		let mut fallback_window: Option<Window> = None;

		for win in self.space.elements() {
			if !win.alive() {
				continue;
			}
			match win.underlying_surface() {
				WindowSurface::Wayland(_) => {
					// Native Wayland toplevels are never Steam BPM (which is always
					// X11). Treat them as non-Steam candidates so they beat BPM.
					if non_steam_window.is_none() {
						non_steam_window = Some(win.clone());
					}
				},
				WindowSurface::X11(x11) => {
					if x11.is_override_redirect() {
						continue;
					}

					let class = x11.class();
					if class.starts_with("steam_app_") {
						game_window = Some(win.clone());
					} else if class != "steam" {
						// Any non-Steam window (e.g. a Proton game using its own class
						// name) should take priority over Steam Big Picture Mode.
						non_steam_window = Some(win.clone());
					} else {
						fallback_window = Some(win.clone());
					}
				},
			}
		}

		let focus_target = if let Some(game) = game_window {
			game
		} else if let Some(non_steam) = non_steam_window {
			non_steam
		} else if let Some(fallback) = fallback_window {
			// Steam BPM is the only candidate.  If a game override surface is
			// active and still alive, the game window is likely just temporarily
			// unmapped during startup; don't hand focus back to Steam BPM in
			// that case.
			let override_alive = self.override_surface.as_ref().is_some_and(|(s, _)| s.alive());
			if override_alive {
				tracing::debug!(
					"Focus determination: game override active, not switching to Steam BPM while game window is absent"
				);
				return;
			}
			fallback
		} else {
			tracing::debug!("Focus determination: no suitable focus candidate");
			return;
		};

		let new_id = focus_target.x11_surface().map(|x| x.window_id());

		// Check if focus is actually changing to a different window.
		// But always allow re-applying focus to the same window if X11
		// focus needs a reset (e.g. ProxiedX11→Window upgrade when
		// wl_surface becomes available).
		if new_id == self.focused_x11_window && !self.x11_focus_needs_reset {
			return;
		}

		tracing::debug!(
			window_id = ?new_id,
			title = ?focus_target.x11_surface().map(|x| x.title()),
			needs_reset = self.x11_focus_needs_reset,
			"Focus determination: applying focus"
		);

		// Extract app ID from window class for GAMESCOPE_FOCUSED_APP.
		// Game windows use class "steam_app_XXXXX" with the Steam app ID.
		// Steam BPM uses class "steam" — gamescope assigns it app ID 769
		// (via the STEAM_BIGPICTURE property). We match that here so Steam
		// knows to activate controller configs for BPM after a game exits.
		if let Some(x11) = focus_target.x11_surface() {
			let class = x11.class();
			if let Some(id_str) = class.strip_prefix("steam_app_") {
				self.focused_app_id = id_str.parse().unwrap_or(0);
				tracing::debug!(app_id = self.focused_app_id, class = %class, "Extracted app ID from window class");
			} else if class.eq_ignore_ascii_case("steam") {
				self.focused_app_id = STEAM_BPM_APP_ID;
				tracing::debug!(app_id = self.focused_app_id, class = %class, "Steam BPM window, using app ID 769");
			} else {
				// The game uses its own window class (e.g. "bg3").  Try to
				// recover the Steam app ID from the process environment so that
				// GAMESCOPE_FOCUSED_APP is set correctly and Steam Big Picture
				// Mode yields controller focus to the game.  Use the cache to
				// avoid re-reading /proc/{pid}/environ on every focus event.
				self.focused_app_id = x11
					.pid()
					.map(|pid| {
						let key = (pid, pid_start_time(pid).unwrap_or(0));
						*self
							.pid_app_id_cache
							.entry(key)
							.or_insert_with(|| steam_app_id_from_pid(pid).unwrap_or(0))
					})
					.unwrap_or(0);
				if self.focused_app_id != 0 {
					tracing::debug!(
						app_id = self.focused_app_id,
						class = %class,
						pid = ?x11.pid(),
						"Looked up Steam app ID from process environment"
					);
				}
			}
		}

		self.set_keyboard_focus_to_window(&focus_target);

		// Set pointer focus to the target window's surface for wl_pointer delivery.
		let window_loc = self.space.element_geometry(&focus_target).map(|g| g.loc);
		if let Some(window_loc) = window_loc {
			let pos_within_window = self.cursor_position - window_loc.to_f64();
			if let Some((surface, surface_offset)) =
				focus_target.surface_under(pos_within_window, WindowSurfaceType::ALL)
			{
				let surface_loc = surface_offset.to_f64() + window_loc.to_f64();
				let pointer = self.seat.get_pointer().unwrap();
				let serial = smithay::utils::SERIAL_COUNTER.next_serial();
				tracing::debug!(
					surface_id = ?surface.id(),
					cursor = ?self.cursor_position,
					?surface_loc,
					"determine_and_apply_focus: setting initial pointer focus via motion()"
				);
				pointer.motion(
					self,
					Some((surface, surface_loc)),
					&smithay::input::pointer::MotionEvent {
						location: self.cursor_position,
						serial,
						time: self.clock.now().as_millis(),
					},
				);
				pointer.frame(self);
			}
		} else {
			tracing::warn!("determine_and_apply_focus: focus target has no geometry in space");
		}
	}

	/// Find any X11 window in the space that has a `wl_surface`.
	///
	/// Since all XWayland surfaces share the same Wayland client, any surface
	/// can be used to route keyboard events to XWayland.
	fn find_xwayland_proxy_surface(&self) -> Option<WlSurface> {
		self.space
			.elements()
			.filter_map(|w| w.x11_surface())
			.find_map(|x11| x11.wl_surface())
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
					if let Some(c) = constraint {
						c.activate();
					}
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

			let new_pos = origin + location;
			pointer.set_location(new_pos);
			// Keep Moonshine's cursor position in sync so that future
			// pointer.motion() calls use the updated position (e.g. after
			// the game calls SetCursorPos to reset the cursor to center).
			self.cursor_position = new_pos;
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
delegate_viewporter!(MoonshineCompositor);
delegate_presentation!(MoonshineCompositor);

// -- DMA-BUF Handler --

impl DmabufHandler for MoonshineCompositor {
	fn dmabuf_state(&mut self) -> &mut DmabufState {
		&mut self.dmabuf_state
	}

	fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
		let format = dmabuf.format();
		tracing::debug!(
			client_fourcc = format!("0x{:08X} ({:?})", format.code as u32, format.code),
			num_planes = dmabuf.num_planes(),
			render_fourcc = format!("0x{:08X} ({:?})", self.render_fourcc as u32, self.render_fourcc),
			"Client DMA-BUF import"
		);
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

	fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
		tracing::debug!(
			window_id = surface.window_id(),
			wl_surface = ?wl_surface.id(),
			title = ?surface.title(),
			class = ?surface.class(),
			"X11 surface associated with wl_surface"
		);

		// Re-run focus determination. A newly associated wl_surface may
		// make a higher-priority window (e.g. the game) eligible for
		// direct Wayland focus that wasn't possible before.
		self.determine_and_apply_focus();
	}
}

// -- XWM Handler (X11 Window Manager) --

impl XwmHandler for MoonshineCompositor {
	fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
		self.xwm.as_mut().expect("XWayland WM not initialized")
	}

	fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			window_id = window.window_id(),
			title = ?window.title(),
			class = ?window.class(),
			"New X11 window"
		);
	}

	fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			window_id = window.window_id(),
			title = ?window.title(),
			class = ?window.class(),
			"New X11 override-redirect window"
		);
	}

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

		// Skip setting _NET_WM_STATE_FULLSCREEN — under "steamcompmgr" WM
		// name, Wine strips fullscreen from _NET_WM_STATE immediately,
		// creating a pending state change that blocks WM_TAKE_FOCUS
		// processing. Gamescope never sets fullscreen on X11 windows
		// either; it handles scaling externally. We just configure the
		// window geometry to fill the output instead.

		// Grant the map request.
		if let Err(e) = window.set_mapped(true) {
			tracing::error!("Failed to set X11 window mapped: {e}");
			return;
		}
		let win = Window::new_x11_window(window.clone());
		self.space.map_element(win.clone(), (0, 0), true);

		// Use deterministic focus determination instead of unconditionally
		// giving focus to the newly mapped window.
		self.determine_and_apply_focus();

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
		// Map with activate=false so the game window keeps
		// _NET_WM_STATE_FOCUSED.  Wine uses that property to determine
		// the foreground window and only delivers input to it.
		self.space.map_element(win.clone(), location, false);
	}

	fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
		let was_focused = Some(window.window_id()) == self.focused_x11_window;
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
		// If the focused window was unmapped, re-determine focus.
		if was_focused {
			self.focused_x11_window = None;
			self.determine_and_apply_focus();
		}
	}

	fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
		// Evict the cache entry for this process so that a later process that
		// reuses the same PID is not incorrectly tagged with the old app ID
		// (the (pid, starttime) key already prevents most false hits, but
		// proactively pruning on window destruction keeps the map bounded).
		if let Some(pid) = window.pid() {
			self.pid_app_id_cache.retain(|(p, _), _| *p != pid);
		}
	}

	fn property_notify(&mut self, _xwm: XwmId, _window: X11Surface, _property: WmWindowProperty) {
		// Re-evaluate focus on any property change.
		self.determine_and_apply_focus();
	}

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
