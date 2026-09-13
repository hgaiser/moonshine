//! Smithay protocol handler implementations for MoonshineCompositor.
//!
//! These are the minimum required delegate implementations for a working
//! Wayland compositor with XWayland support.

use smithay::backend::allocator::Buffer;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::input::{InputTime, TabletToolDescriptor};
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_dispatch2;
use smithay::desktop::Window;
use smithay::input::dnd::DndGrabHandler;
use smithay::input::pointer::{CursorImageStatus, MotionEvent, PointerHandle};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState, is_sync_subsurface};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{ConstraintRemove, PointerConstraintsHandler, with_pointer_constraint};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler};
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
	XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};

use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::XWaylandClientData;

use crate::session::compositor::focus::{KeyboardFocusTarget, WindowFlags, WindowMetadata, get_window_priority_key};
use crate::session::compositor::state::{ClientState, MoonshineCompositor};

// ---------------------------------------------------------------------------
// Process-tree app_id detection (mirrors gamescope's get_appid_from_pid)
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Instant;

type AppIdCacheKey = (u32, u64);
type AppIdCacheValue = (u32, Instant);
type AppIdCache = RwLock<HashMap<AppIdCacheKey, AppIdCacheValue>>;

/// PID → app_id cache keyed on `(pid, starttime)` to avoid stale hits after
/// PID reuse. Processes in a Steam game tree (Steam → reaper → proton → game)
/// are stable, so caching with a short TTL is safe.
static APPID_CACHE: OnceLock<AppIdCache> = OnceLock::new();

/// TTL for cached app_id results — 5 seconds. Processes die and respawn,
/// but within a single gaming session the PIDs are stable.
const APPID_CACHE_TTL_SECS: u64 = 5;

/// Read `/proc/<pid>/stat` to extract the starttime field (field 22).
/// Returns the raw clock ticks since boot — not wall-clock time.
fn get_pid_starttime(pid: u32) -> Option<u64> {
	let stat_path = format!("/proc/{}/stat", pid);
	let proc_stat = std::fs::read_to_string(&stat_path).ok()?;

	// Parse the process name (between first '(' and last ')') and the
	// fields after the closing paren.
	let proc_name_end = proc_stat.rfind(')')?;
	let fields_after = proc_stat[proc_name_end + 2..].split_whitespace().collect::<Vec<&str>>();

	// fields_after[0] = state, [1] = ppid, ..., [19] = starttime (0-indexed: 19)
	// starttime is the 22nd field overall (index 21 in full /proc/pid/stat)
	// but after stripping "name (state ppid ...)", starttime is at index 19.
	fields_after.get(19).and_then(|s| s.parse::<u64>().ok())
}

/// Look up a cached app_id for a PID, respecting (pid, starttime) key and TTL.
/// Removes expired entries on read to prevent unbounded cache growth.
fn cache_get(pid: u32) -> Option<u32> {
	let starttime = get_pid_starttime(pid)?;
	let cache = APPID_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
	let entry = cache.read().unwrap().get(&(pid, starttime)).cloned();
	match entry {
		Some((app_id, time)) if time.elapsed().as_secs() < APPID_CACHE_TTL_SECS => Some(app_id),
		Some(_) => {
			// Entry expired — remove it.
			cache.write().unwrap().remove(&(pid, starttime));
			None
		},
		None => None,
	}
}

/// Store an app_id result in the cache keyed on (pid, starttime).
fn cache_set(pid: u32, app_id: u32) {
	let starttime = get_pid_starttime(pid);
	let mut cache = APPID_CACHE.get_or_init(|| RwLock::new(HashMap::new())).write().unwrap();
	if let Some(start) = starttime {
		cache.insert((pid, start), (app_id, Instant::now()));
	}
}

/// Walk the process tree starting from `pid`, looking for Steam app ID.
/// Detects `SteamLaunch AppId=<N>` and `steamwebhelper --appids=<N>` in
/// `/proc/<pid>/cmdline`. Returns the first matching app ID found, or 0.
///
/// Uses a simple TTL cache to avoid re-reading `/proc/<pid>/cmdline` for
/// the same PID across multiple focus evaluations.
///
/// Gamescope: `get_appid_from_pid()` — walks `/proc/<pid>/stat` to find the
/// parent PID, then reads `/proc/<pid>/cmdline` looking for "SteamLaunch AppId=N".
/// The walk continues through parent processes until the app ID is found or
/// the process tree is exhausted.
fn get_appid_from_pid(pid: u32) -> u32 {
	let mut next_pid = pid;

	tracing::trace!(target: "focus", pid, "get_appid_from_pid: starting process tree walk");

	loop {
		// Read /proc/<pid>/stat to get the parent PID.
		let stat_path = format!("/proc/{}/stat", next_pid);
		let proc_stat = match std::fs::read_to_string(&stat_path) {
			Ok(s) => s,
			Err(e) => {
				tracing::trace!(target: "focus", pid = next_pid, err = %e, "get_appid_from_pid: cannot read /proc/<pid>/stat, stopping walk");
				break;
			},
		};

		// Parse the process name (between first '(' and last ')') and the
		// state + parent_pid fields after the closing paren.
		let proc_name_start = match proc_stat.find('(') {
			Some(i) => i + 1,
			None => {
				tracing::warn!(target: "focus", pid = next_pid, "get_appid_from_pid: cannot parse proc name from stat");
				break;
			},
		};
		let proc_name_end = match proc_stat.rfind(')') {
			Some(i) => i,
			None => {
				tracing::warn!(
					target: "focus",
					pid = next_pid,
					"get_appid_from_pid: cannot find end of proc name in stat"
				);
				break;
			},
		};
		let proc_name = &proc_stat[proc_name_start..proc_name_end];

		// Check if this is a init/reaper process (PID 1 or a zombie reaper).
		// Gamescope treats "reaper" specially — it reads cmdline for the
		// actual SteamLaunch detection.
		let state_and_parent = &proc_stat[proc_name_end + 1..];
		// Parse: " state parent_pid ..."
		let parts: Vec<&str> = state_and_parent.split_whitespace().collect();
		if parts.len() < 2 {
			tracing::trace!(
				target: "focus",
				pid = next_pid,
				"get_appid_from_pid: not enough fields in stat, stopping walk"
			);
			break;
		}
		let parent_pid: u32 = match parts[1].parse() {
			Ok(p) => p,
			Err(_) => {
				tracing::trace!(
					target: "focus",
					pid = next_pid,
					"get_appid_from_pid: cannot parse parent_pid, stopping walk"
				);
				break;
			},
		};

		tracing::trace!(target: "focus", pid = next_pid, proc_name = %proc_name, parent_pid, "get_appid_from_pid: walking process");

		// Check cache first — avoids re-reading cmdline for the same PID.
		if let Some(cached) = cache_get(next_pid) {
			if cached != 0 {
				tracing::trace!(target: "focus", pid = next_pid, app_id = cached, "get_appid_from_pid: cache hit");
				return cached;
			}
			// Cache hit with 0 — this PID has no app_id. Skip cmdline read.
			if proc_name == "reaper" {
				break;
			}
			next_pid = parent_pid;
			continue;
		}

		// If this is a reaper process, read its cmdline to find SteamLaunch.
		if proc_name == "reaper" {
			let cmdline_path = format!("/proc/{}/cmdline", next_pid);
			if let Ok(cmdline) = std::fs::read(&cmdline_path) {
				let app_id = scan_cmdline_for_app_id(&cmdline);
				if app_id != 0 {
					cache_set(next_pid, app_id);
					tracing::debug!(target: "focus", pid = next_pid, app_id, "get_appid_from_pid: found app_id in reaper");
					return app_id;
				}
			}
			cache_set(next_pid, 0); // Cache miss result
			break;
		}

		// Steam client processes (BPM/overlay) have no game reaper; gamescope
		// derives their app id (769) from the cgroup scope. Detect by name.
		if proc_name == "steam" || proc_name == "steamwebhelper" {
			tracing::debug!(target: "focus", pid = next_pid, proc_name = %proc_name, "get_appid_from_pid: Steam client process -> app 769");
			return super::x11_focus::STEAM_BIG_PICTURE_APPID;
		}

		// If parent_pid is -1 or 0, we've reached the top of the tree.
		if parent_pid == 0 || parent_pid == u32::MAX {
			tracing::trace!(target: "focus", pid = next_pid, "get_appid_from_pid: reached root of process tree");
			break;
		}

		// Guard against PID 1 self-parenting (possible in PID namespaces).
		// On normal Linux PID 1 has parent 0, but in containers with PID
		// namespaces PID 1's parent can be itself — which would cause an
		// infinite loop since next_pid would stay 1 forever.
		if next_pid == 1 {
			tracing::trace!(target: "focus", pid = next_pid, "get_appid_from_pid: reached PID 1, stopping walk");
			break;
		}

		// Check the current process's cmdline for SteamLaunch or steamwebhelper.
		let cmdline_path = format!("/proc/{}/cmdline", next_pid);
		if let Ok(cmdline) = std::fs::read(&cmdline_path) {
			let app_id = scan_cmdline_for_app_id(&cmdline);
			if app_id != 0 {
				cache_set(next_pid, app_id);
				tracing::debug!(target: "focus", pid = next_pid, app_id, "get_appid_from_pid: found app_id");
				return app_id;
			}
		}
		cache_set(next_pid, 0); // Cache miss result

		// Walk up to the parent process.
		next_pid = parent_pid;
	}

	tracing::debug!(target: "focus", "get_appid_from_pid: no app_id found, returning 0");
	0
}

/// Scan a null-byte-separated cmdline for Steam app ID.
/// Detects two patterns:
///   - `SteamLaunch AppId=<N>` — classic Steam launch
///   - `--appids=<N>` — Proton steamwebhelper launch
///
/// Accepts raw bytes because `/proc/<pid>/cmdline` is not guaranteed UTF-8.
fn scan_cmdline_for_app_id(cmdline: &[u8]) -> u32 {
	let mut found_steam_launch = false;
	let mut app_id: u32 = 0;

	for part in cmdline.split(|&b| b == b'\0') {
		// Check for --appids= BEFORE "--" break — --appids= may appear
		// after "--" in some launchers (e.g., "steamwebhelper -- --appids=12345").
		if let Some(ids_str) = part.strip_prefix(b"--appids=") {
			// Proton steamwebhelper: --appids=<N> or --appids=<N>,<N>...
			for id_str in ids_str.split(|&b| b == b',') {
				if let Ok(s) = std::str::from_utf8(id_str.trim_ascii())
					&& let Ok(id) = s.parse::<u32>()
					&& id != 0
				{
					app_id = id;
					break;
				}
			}
			if app_id != 0 {
				break;
			}
		}

		if part == b"SteamLaunch" {
			found_steam_launch = true;
		} else if found_steam_launch
			&& part.starts_with(b"AppId=")
			&& let Some(id_str) = part.strip_prefix(b"AppId=")
			&& let Ok(s) = std::str::from_utf8(id_str.trim_ascii())
			&& let Ok(id) = s.parse::<u32>()
			&& id != 0
		{
			app_id = id;
			break;
		}
		// Also detect standalone AppId=<N> without SteamLaunch prefix
		// (some launchers may use just AppId=N directly).
		if part.starts_with(b"AppId=")
			&& let Some(id_str) = part.strip_prefix(b"AppId=")
			&& let Ok(s) = std::str::from_utf8(id_str.trim_ascii())
			&& let Ok(id) = s.parse::<u32>()
			&& id != 0
			&& app_id == 0
		{
			app_id = id;
		}
		// Note: we do NOT break on "--" because --appids= can appear
		// after "--" in some launchers (e.g., "steamwebhelper -- --appids=12345").
		// Since we only look for specific patterns (SteamLaunch, --appids=),
		// ignoring arbitrary args after "--" is harmless.
	}

	if app_id == 0 {
		// Replace null bytes with spaces for readable debug output.
		let readable: Vec<u8> = cmdline.iter().map(|&b| if b == b'\0' { b' ' } else { b }).collect();
		let readable = String::from_utf8_lossy(&readable);
		tracing::debug!(target: "focus", cmdline = %readable, "scan_cmdline_for_app_id: no app_id found in cmdline");
	}

	app_id
}

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
		unreachable!("Client has neither ClientState nor XWaylandClientData");
	}

	fn commit(&mut self, surface: &WlSurface) {
		// Mark the screen as dirty so the next timer tick renders and sends a frame.
		self.screen_dirty = true;

		// Apply pending color management state.
		if let Some(cm) = &mut self.color_management {
			cm.commit(surface);
		}

		// Whether this surface already had a buffer before this commit, to
		// detect its first presentation below.
		let had_buffer =
			smithay::backend::renderer::utils::with_renderer_surface_state(surface, |st| st.buffer().is_some())
				.unwrap_or(false);

		// Initialize RendererSurfaceState for this surface so that
		// space_render_elements can produce render elements from the
		// attached buffer. Without this call surfaces appear bufferless
		// and the rendered frame is always blank.
		on_commit_buffer_handler::<Self>(surface);

		// Ensure the surface is not a pending sync subsurface.
		if is_sync_subsurface(surface) {
			return;
		}

		// Re-evaluate focus when a window presents its first buffer, or the
		// focus-ready gate would exclude it forever (keyboard goes nowhere).
		if !had_buffer
			&& smithay::backend::renderer::utils::with_renderer_surface_state(surface, |st| st.buffer().is_some())
				.unwrap_or(false)
			&& (self
				.space
				.elements()
				.any(|w| w.wl_surface().is_some_and(|s| &*s == surface))
				|| self.override_surface.as_ref().is_some_and(|(s, _)| s == surface))
		{
			self.reevaluate_focus();
		}

		// If the surface is a toplevel, refresh the space.
		if let Some(window) = self
			.space
			.elements()
			.find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
			.cloned()
		{
			window.on_commit();

			// wlroots' wayland backend drops the size from the initial
			// configure (output not enabled yet); re-send it on each
			// mismatched commit so nested compositors adopt the session size.
			if let Some(toplevel) = window.toplevel()
				&& window.geometry().size != (self.width as i32, self.height as i32).into()
			{
				toplevel.with_pending_state(|state| {
					state.size = Some((self.width as i32, self.height as i32).into());
				});
				toplevel.send_configure();
			}

			// Damage sequence tracking (Task 2.1):
			// Increment damage_sequence for game windows (app_id != 0).
			// Only trigger focus dirty on window MAP, not on every commit,
			// to avoid focus jumping. We check if this window was just mapped
			// by comparing against the focused window's damage_sequence.
			if let Some(meta) = self.window_metadata.get_mut(&window) {
				if meta.has_game_id() {
					self.damage_sequence_counter += 1;
					meta.damage_sequence = self.damage_sequence_counter;
				}
				// The window we wanted to focus has now drawn: re-run focus so
				// the present can switch to it.
				if meta.outdated_interactive_focus {
					meta.outdated_interactive_focus = false;
					self.focus_state.mark_dirty();
				}
			}
		}

		// Handle popup commits.
		self.popups_commit(surface);
	}

	fn destroyed(&mut self, surface: &WlSurface) {
		tracing::debug!(surface_id = ?surface.id(), "surface destroyed");
		if let Some(cm) = &mut self.color_management {
			cm.surface_destroyed(surface);
		}
	}
}

impl MoonshineCompositor {
	fn popups_commit(&mut self, _surface: &WlSurface) {
		// Popup handling can be added later.
	}

	/// Find a `Window` by its Wayland surface.
	///
	/// Centralized helper to avoid copy-pasting the same
	/// `space.elements().find(|w| w.toplevel()...)` closure across
	/// `commit`, `toplevel_destroyed`, `fullscreen_request`,
	/// `unfullscreen_request`, and `app_id_changed`.
	fn find_window_by_surface(&self, surface: &WlSurface) -> Option<Window> {
		self.space
			.elements()
			.find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
			.cloned()
	}

	/// Find a `Window` by its X11 surface.
	///
	/// X11 counterpart to [`Self::find_window_by_surface`].
	fn find_window_by_x11_surface(&self, window: &X11Surface) -> Option<Window> {
		self.space
			.elements()
			.find(|w| w.x11_surface().map(|x| x == window).unwrap_or(false))
			.cloned()
	}

	/// The output rectangle, which every fullscreen window is held at.
	pub(crate) fn output_rect(&self) -> Rectangle<i32, Logical> {
		Rectangle::new((0, 0).into(), (self.width as i32, self.height as i32).into())
	}

	/// Helper to read an X11 window property via `X11Focus`, returning a
	/// default value when X11 focus infrastructure is not available.
	///
	/// Replaces the repeated `if let Some(x11_focus) = &self.x11_focus { … } else { default }` pattern
	/// in `x11_surface_metadata`.
	fn with_x11_focus<T: Default, F>(&self, f: F) -> T
	where
		F: FnOnce(&super::x11_focus::X11Focus) -> T,
	{
		self.x11_focus.as_ref().map(f).unwrap_or_default()
	}

	/// Unregister a window from the compositor's focus tracking state.
	///
	/// Cleans up the transient children index and removes window metadata.
	/// Centralized here to avoid copy-pasting the same ~8-line cleanup
	/// block across `toplevel_destroyed`, `unmapped_window`, and
	/// `destroyed_window`.
	fn unregister_window(&mut self, window: &Window) {
		if let Some(meta) = self.window_metadata.get(window)
			&& let Some(parent_id) = meta.transient_for
			&& let Some(children) = self.transient_children.get_mut(&parent_id)
		{
			children.retain(|w| w != window);
			if children.is_empty() {
				self.transient_children.remove(&parent_id);
			}
		}
		self.window_metadata.remove(window);
	}

	/// Check if an X11 window is transient-for (directly or transitively)
	/// the currently focused X11 window. Walks the transient-for chain
	/// upward to support nested popups (submenus, tooltips inside menus).
	fn is_transient_of_focused(&self, window_id: u32) -> bool {
		let focused_id = match self.focused_x11_window {
			Some(id) => id,
			None => return false,
		};
		if window_id == focused_id {
			return true;
		}
		let mut current = window_id;
		let mut visited = std::collections::HashSet::new();
		visited.insert(current);
		for _ in 0..10 {
			let parent = self
				.window_metadata
				.iter()
				.find(|(_, m)| m.x11_window_id == Some(current))
				.and_then(|(_, m)| m.transient_for);
			match parent {
				Some(p) if p == focused_id => return true,
				Some(p) if visited.insert(p) => current = p,
				_ => break,
			}
		}
		false
	}

	/// Apply a fullscreen state change requested by an X11 client.
	///
	/// Smithay's `XwmHandler` defaults are no-ops, so without this
	/// `_NET_WM_STATE_FULLSCREEN` is never set and clients that wait for the
	/// transition stall. `reevaluate_focus` then applies the geometry.
	///
	/// Gamescope: `handle_net_wm_state()`
	fn set_x11_fullscreen(&mut self, window: &X11Surface, fullscreen: bool) {
		if let Err(e) = window.set_fullscreen(fullscreen) {
			tracing::warn!("Failed to set X11 fullscreen state: {e}");
			return;
		}

		// `refresh_metadata` never re-reads this, so map time is otherwise the
		// only point it is set. `skip_and_not_fullscreen()` ranks focus on it.
		if let Some(elem) = self.find_window_by_x11_surface(window)
			&& let Some(meta) = self.window_metadata.get_mut(&elem)
		{
			meta.fullscreen = fullscreen;
		}

		self.reevaluate_focus();
	}

	/// Build WindowMetadata from an X11 surface for focus priority decisions.
	fn x11_surface_metadata(&self, window: &smithay::xwayland::xwm::X11Surface) -> WindowMetadata {
		// Derive app_id from _NET_WM_PID + process tree lookup.
		// Gamescope: get_appid_from_pid() walks /proc/<pid>/cmdline
		// looking for "SteamLaunch AppId=N" in the parent process chain.
		let pid = self.with_x11_focus(|xf| xf.get_window_pid(window.window_id()));
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			pid,
			"x11_surface_metadata: read _NET_WM_PID"
		);
		let mut app_id = if pid != 0 { get_appid_from_pid(pid) } else { 0 };

		// STEAM_GAME / STEAM_BIGPICTURE per-window properties.
		let steam_app_id = self.with_x11_focus(|xf| xf.get_steam_app_id(window.window_id()));
		let steam_legacy_big_picture = self.with_x11_focus(|xf| xf.is_steam_big_picture(window.window_id()));

		// Steam mode makes STEAM_GAME authoritative; otherwise every window is
		// its own app. Legacy Big Picture is Steam's own app id (769).
		if self.steam_mode {
			if steam_app_id != 0 {
				app_id = steam_app_id;
			}
		} else {
			app_id = window.window_id();
		}
		if steam_legacy_big_picture {
			app_id = super::x11_focus::STEAM_BIG_PICTURE_APPID;
			tracing::debug!(
				target: "focus",
				window_id = window.window_id(),
				app_id,
				"x11_surface_metadata: STEAM_BIGPICTURE detected"
			);
		}
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			app_id,
			"x11_surface_metadata: final app_id"
		);

		// Read STEAM_INPUT_FOCUS from the window property.
		// Mode 0 = normal (keyboard and pointer focus on same window).
		// Mode 2 = separate keyboard/pointer focus — keyboard stays
		// on the main window while pointer routes to overlay (Steam overlay).
		// Gamescope: reads steamInputFocusAtom property.
		let input_focus_mode = self.with_x11_focus(|xf| xf.get_input_focus_mode(window.window_id()));

		// Read STEAM_OVERLAY. Gamescope splits overlay windows into the
		// interactive overlay (full root-window width or asking for input) and
		// notifications (everything else) at focus time.
		let is_overlay = self.with_x11_focus(|xf| xf.get_steam_overlay_value(window.window_id())) != 0;
		let interactive_overlay = window.geometry().size.w >= self.width as i32 || input_focus_mode != 0;

		// Build WindowFlags: overlay/tray/streaming/VR classification.
		// Packed into a single u8 instead of 7 separate bool fields.
		let mut flags = WindowFlags::empty();
		if is_overlay {
			if interactive_overlay {
				flags.insert(WindowFlags::OVERLAY);
			} else {
				flags.insert(WindowFlags::NOTIFICATION);
			}
		}

		// External overlays, streaming clients, VR targets.
		let is_external_overlay = self.with_x11_focus(|xf| {
			let external = xf.is_gamescope_external_overlay(window.window_id());
			if external {
				flags.insert(WindowFlags::EXTERNAL_OVERLAY);
			}
			if xf.is_steam_streaming_client(window.window_id()) {
				flags.insert(WindowFlags::STREAMING_CLIENT);
			}
			if xf.is_steam_streaming_client_video(window.window_id()) {
				flags.insert(WindowFlags::STREAMING_CLIENT_VIDEO);
			}
			if xf.get_vr_overlay_target(window.window_id()) != 0 {
				flags.insert(WindowFlags::VR_OVERLAY_TARGET);
			}
			external
		});

		// Overlay windows are not apps: parts of the focus code don't expect an
		// app id on them.
		if is_external_overlay {
			app_id = 0;
		}

		// System tray icons (_NET_WM_WINDOW_TYPE_DOCK) are excluded from focus.
		if matches!(window.window_type(), Some(smithay::xwayland::xwm::WmWindowType::Dock)) {
			flags.insert(WindowFlags::SYS_TRAY_ICON);
		}

		// Read window opacity for overlay selection (highest opacity wins).
		// Gamescope: reads _NET_WM_WINDOW_OPACITY property.
		let opacity = self.with_x11_focus(|xf| xf.get_window_opacity(window.window_id()));

		// Wine window styles: `_WINE_HWND_STYLE` / `_WINE_HWND_EXSTYLE` (0 when
		// unset). `WS_DISABLED` (0x80000000) in the style bitmask marks a
		// disabled window.
		let (hwnd_style, hwnd_style_ex) = self.with_x11_focus(|xf| {
			(
				xf.get_window_style(window.window_id()),
				xf.get_window_style_ex(window.window_id()),
			)
		});

		// Detect dialog windows from _NET_WM_WINDOW_TYPE.
		let is_dialog = matches!(window.window_type(), Some(smithay::xwayland::xwm::WmWindowType::Dialog));

		let has_transient_parent = window.is_transient_for().is_some();

		// `maybe_a_dropdown` comes from the WM normal hints: the client gave a
		// program-specified position with static gravity at a non-negative
		// origin. `requested_width/height` come from fixed min==max size hints.
		let (maybe_a_dropdown, requested_width, requested_height) = match window.size_hints() {
			Some(hints) => {
				use smithay::reexports::x11rb::properties::WmSizeHintsSpecification;
				let positioned = matches!(
					hints.position,
					Some((WmSizeHintsSpecification::ProgramSpecified, x, y)) if x >= 0 && y >= 0
				);
				let static_gravity = hints.win_gravity.map(u32::from) == Some(10);
				let (requested_width, requested_height) = match (hints.min_size, hints.max_size) {
					(Some((min_w, min_h)), Some((max_w, max_h)))
						if min_w > 0 && min_h > 0 && max_w > 0 && max_h > 0 && max_w == min_w && max_h == min_h =>
					{
						(max_w as u32, max_h as u32)
					},
					_ => (0, 0),
				};
				(positioned && static_gravity, requested_width, requested_height)
			},
			None => (false, 0, 0),
		};

		// skipTaskbar/skipPager are read from _NET_WM_STATE in gamescope.
		// Read both flags in a single X11 roundtrip.
		// Fall back to heuristic if the property is unavailable.
		let (skip_taskbar, skip_pager) = self.with_x11_focus(|xf| xf.get_net_wm_state_skip_flags(window.window_id()));
		let skip_taskbar = skip_taskbar || (window.is_override_redirect() && has_transient_parent);
		let skip_pager = skip_pager || (window.is_override_redirect() && has_transient_parent);

		WindowMetadata {
			app_id,
			steam_app_id,
			steam_legacy_big_picture,
			is_overlay,
			has_hwnd_style: hwnd_style != 0,
			has_hwnd_style_ex: hwnd_style_ex != 0,
			// Smithay's XWM only surfaces InputOutput windows, so every window
			// here has c_class == InputOutput (matching gamescope's skip).
			// A window only gets metadata when it is mapped into the space
			// (map requests and override-redirect mappings), which is the same
			// set gamescope's window list holds. Smithay's `is_mapped()` tracks
			// wl_surface association instead and is still false at map time.
			map_state_viewable: true,
			class_input_output: true,
			outdated_interactive_focus: false,
			x11_window_id: Some(window.window_id()),
			pid,
			transient_for: window.is_transient_for(),
			skip_taskbar,
			skip_pager,
			is_dialog,
			maybe_a_dropdown,
			requested_width,
			requested_height,
			hwnd_style,
			hwnd_style_ex,
			map_sequence: 0, // assigned by caller
			override_redirect: window.is_override_redirect(),
			is_x11: true,
			geometry: window.geometry(),
			fullscreen: window.is_fullscreen(),
			opacity,
			input_focus_mode,
			flags,
			damage_sequence: 0, // assigned by caller for game windows
		}
	}

	/// Re-read live X11 properties for all tracked windows.
	///
	/// Updates STEAM_INPUT_FOCUS on overlay windows and refreshes app_id
	/// for X11 windows that still have app_id=0 (Steam sets _NET_WM_PID
	/// asynchronously after window creation).
	fn refresh_metadata(&mut self) {
		if let Some(x11_focus) = &self.x11_focus {
			// Re-read STEAM_INPUT_FOCUS for all Steam overlay windows.
			// Steam changes this property dynamically when the overlay is toggled.
			for (window, meta) in self.window_metadata.iter_mut() {
				if meta.is_overlay
					&& let Some(x11) = window.x11_surface()
				{
					let new_mode = x11_focus.get_input_focus_mode(x11.window_id());
					if new_mode != meta.input_focus_mode {
						tracing::debug!(
							target: "focus",
							window_id = x11.window_id(),
							old_mode = meta.input_focus_mode,
							new_mode,
							"STEAM_INPUT_FOCUS changed on overlay"
						);
						meta.input_focus_mode = new_mode;
					}
				}
			}

			// Refresh app_id for X11 windows that still have app_id=0.
			// _NET_WM_PID may not be set at map time — Steam sets it
			// asynchronously after the window is created.
			for (window, meta) in self.window_metadata.iter_mut() {
				if meta.app_id == 0
					&& let Some(x11) = window.x11_surface()
				{
					let pid = x11_focus.get_window_pid(x11.window_id());
					tracing::debug!(target: "focus", window_id = x11.window_id(), pid, "refresh_app_id: re-read _NET_WM_PID");
					if pid != 0 {
						let new_app_id = get_appid_from_pid(pid);
						if new_app_id != 0 {
							tracing::debug!(
								target: "focus",
								window_id = x11.window_id(),
								pid,
								app_id = new_app_id,
								"Refreshed app_id from _NET_WM_PID"
							);
							meta.app_id = new_app_id;
						}
					}
				}
			}
		}
	}

	/// Classify overlay, notification, external overlay, and input-focus
	/// windows. Gamescope: `DetermineAndApplyFocus()` window classification.
	///
	/// The interactive overlay spans the full root width or asks for input; any
	/// other Steam overlay is a notification (last one in stacking order wins).
	/// Only the highest-opacity external overlay is kept. `input_focus_window`
	/// is the last overlay that asked for input.
	fn classify_special_windows(&mut self, windows: &[Window]) {
		let mut best_overlay: Option<Window> = None;
		let mut best_notification: Option<Window> = None;
		let mut best_external_overlay: Option<Window> = None;
		let mut input_focus: Option<Window> = None;
		let mut max_overlay_opacity = 0u32;
		let mut max_external_overlay_opacity = 0u32;

		for window in windows {
			let Some(meta) = self.window_metadata.get(window) else {
				continue;
			};

			if meta.is_overlay {
				let interactive = meta.geometry.size.w >= self.width as i32 || meta.input_focus_mode != 0;
				if interactive && meta.opacity >= max_overlay_opacity {
					best_overlay = Some(window.clone());
					max_overlay_opacity = meta.opacity;
				} else if !interactive {
					best_notification = Some(window.clone());
				}

				if meta.input_focus_mode != 0 {
					input_focus = Some(window.clone());
				}
			}

			if meta.flags.contains(WindowFlags::EXTERNAL_OVERLAY) && meta.opacity > max_external_overlay_opacity {
				best_external_overlay = Some(window.clone());
				max_external_overlay_opacity = meta.opacity;
			}
		}

		let old_overlay = self.overlay_window.clone();
		let old_notification = self.notification_window.clone();
		let old_external_overlay = self.external_overlay_window.clone();
		let old_input_focus = self.input_focus_window.clone();
		self.overlay_window = best_overlay;
		self.notification_window = best_notification;
		self.external_overlay_window = best_external_overlay;
		self.input_focus_window = input_focus;
		if old_overlay != self.overlay_window
			|| old_notification != self.notification_window
			|| old_external_overlay != self.external_overlay_window
			|| old_input_focus != self.input_focus_window
		{
			self.focus_state.mark_dirty();
		}
	}

	/// Whether a window has a committed buffer (own surface or WSI override).
	fn window_has_buffer(&self, window: &Window) -> bool {
		let surface_has_buffer = |s: &WlSurface| {
			smithay::backend::renderer::utils::with_renderer_surface_state(s, |st| st.buffer().is_some())
				.unwrap_or(false)
		};
		if window.wl_surface().is_some_and(|s| surface_has_buffer(&s)) {
			return true;
		}
		let x11_id = window.x11_surface().map(|x| x.window_id());
		self.override_surface
			.as_ref()
			.is_some_and(|(s, render_window)| Some(*render_window) == x11_id && s.is_alive() && surface_has_buffer(s))
	}

	/// Defer to the Steam UI only while `requested` hasn't presented yet.
	fn defer_focus_to_steam_ui(&self, requested: &Window) -> bool {
		let current_is_steam_ui = self
			.focused_x11_window
			.and_then(|fid| {
				self.space
					.elements()
					.find(|we| we.x11_surface().is_some_and(|x| x.window_id() == fid))
			})
			.and_then(|fw| self.window_metadata.get(fw))
			.is_some_and(|m| m.is_steam());
		current_is_steam_ui && !self.window_has_buffer(requested)
	}

	/// Build the list of focus candidates.
	///
	/// Gamescope: `GetPossibleFocusWindows()` — skips system tray icons,
	/// overlays, external overlays, VR overlay targets, and the streaming
	/// client video window; under Steam control only game, Steam UI, and
	/// streaming-client windows qualify; finally requires a viewable
	/// InputOutput window with non-zero opacity.
	fn build_candidates(&self, windows: &[Window]) -> Vec<Window> {
		use super::VirtualConnectorStrategy;

		let steam_controlled = self.e_strategy() == VirtualConnectorStrategy::SteamControlled;
		let mut candidates = Vec::new();
		for window in windows {
			let Some(meta) = self.window_metadata.get(window) else {
				continue;
			};

			// Always skip system tray icons, overlays, and external overlays.
			if meta.flags.intersects(
				WindowFlags::OVERLAY
					| WindowFlags::NOTIFICATION
					| WindowFlags::EXTERNAL_OVERLAY
					| WindowFlags::SYS_TRAY_ICON,
			) {
				continue;
			}

			// Skip VR overlay targets (forwarded overlays are disabled).
			if meta.flags.contains(WindowFlags::VR_OVERLAY_TARGET) {
				continue;
			}

			// Skip the streaming client video window.
			if meta.flags.contains(WindowFlags::STREAMING_CLIENT_VIDEO) {
				continue;
			}

			// Under Steam control only game, Steam UI, and streaming-client
			// windows are focus candidates.
			if steam_controlled
				&& !(meta.has_game_id() || meta.is_steam() || meta.flags.contains(WindowFlags::STREAMING_CLIENT))
			{
				continue;
			}

			// A viewable InputOutput window with non-zero opacity (or the
			// streaming client) is a candidate.
			if meta.map_state_viewable
				&& meta.class_input_output
				&& (meta.opacity > 0 || meta.flags.contains(WindowFlags::STREAMING_CLIENT))
			{
				candidates.push(window.clone());
			}
		}

		// Highest priority first (`is_focus_priority_greater`, stable sort).
		candidates.sort_by_key(|w| {
			std::cmp::Reverse(
				self.window_metadata
					.get(w)
					.map(get_window_priority_key)
					.unwrap_or_default(),
			)
		});
		candidates
	}

	/// Pick the focus window and its override (dropdown) window.
	///
	/// Gamescope: `pick_primary_focus_and_override()` — Steam names the focus
	/// window (exact window, then app-id list in order), otherwise the
	/// highest-priority candidate wins. Transient links are followed and the
	/// override slot is resolved. On a single output `global_focus` is always
	/// false, so a picked window is committed even before it has a buffer; it is
	/// only flagged `outdated_interactive_focus` in that case.
	///
	/// `xdg-activation` is a Wayland concept with no gamescope equivalent: an
	/// explicit request that names a candidate takes precedence.
	fn pick_primary_focus_and_override(
		&mut self,
		candidates: &[Window],
		focus_control_window: Option<u32>,
		app_ids: &[u32],
		global_focus: bool,
		virtual_key: u64,
		strategy: super::VirtualConnectorStrategy,
	) -> (Option<Window>, Option<Window>, bool) {
		use super::{VirtualConnectorStrategy, focus};

		let mut focus: Option<Window> = None;
		let mut local_game_focused = false;

		// Wayland `xdg-activation` request (moonshine-only).
		if let Some(requested_surface) = self.focus_state.peek_requested_focus_surface().cloned() {
			if let Some(w) = candidates
				.iter()
				.find(|w| w.wl_surface().is_some_and(|s| *s == requested_surface))
			{
				if !self.defer_focus_to_steam_ui(w) {
					tracing::debug!(
						target: "focus",
						surface_id = ?requested_surface.id(),
						"xdg-activation: honoring explicit focus request"
					);
					focus = Some(w.clone());
				}
			} else if !self
				.space
				.elements()
				.any(|w| w.wl_surface().is_some_and(|s| *s == requested_surface))
			{
				tracing::debug!(
					target: "focus",
					surface_id = ?requested_surface.id(),
					"xdg-activation: requested surface gone, clearing request"
				);
				self.focus_state.clear_requested_focus_surface();
			}
		}

		let controlled_focus = strategy != VirtualConnectorStrategy::SingleApplication
			|| focus_control_window.is_some()
			|| !app_ids.is_empty();

		if focus.is_none() && controlled_focus {
			if strategy == VirtualConnectorStrategy::SteamControlled {
				if let Some(target) = focus_control_window
					&& let Some(w) = candidates
						.iter()
						.find(|w| w.x11_surface().is_some_and(|x| x.window_id() == target))
				{
					focus = Some(w.clone());
					local_game_focused = true;
				}

				if focus.is_none() {
					'steam: for appid in app_ids {
						for window in candidates {
							if self.window_metadata.get(window).is_some_and(|m| m.app_id == *appid) {
								focus = Some(window.clone());
								local_game_focused = true;
								break 'steam;
							}
						}
					}
				}
			} else {
				for window in candidates {
					let key = self
						.window_metadata
						.get(window)
						.map(|m| focus::virtual_connector_key(m, strategy))
						.unwrap_or(0);
					if key == virtual_key {
						focus = Some(window.clone());
						local_game_focused = true;
						break;
					}
				}
			}
		}

		if focus.is_none()
			&& (!global_focus || !controlled_focus)
			&& let Some(first) = candidates.first()
		{
			focus = Some(first.clone());
			local_game_focused = self.window_metadata.get(first).is_some_and(|m| m.app_id != 0);
		}

		// Follow transient links from the focus window unless Steam named it or
		// the strategy treats the window as its own connector.
		if focus_control_window.is_none()
			&& let Some(f) = focus.as_ref()
			&& f.x11_surface().is_some()
			&& !self
				.window_metadata
				.get(f)
				.is_some_and(|m| focus::win_treat_as_per_window(m, strategy))
		{
			let mut visited = std::collections::HashSet::new();
			while let Some(cur) = focus.clone() {
				let Some(cur_id) = cur.x11_surface().map(|x| x.window_id()) else {
					break;
				};
				if !visited.insert(cur_id) {
					break;
				}
				let next = candidates
					.iter()
					.find(|w| {
						self.window_metadata.get(*w).is_some_and(|m| {
							w.x11_surface().is_some_and(|x| x.window_id() != cur_id)
								&& m.transient_for == Some(cur_id)
								&& !m.is_dropdown()
						})
					})
					.cloned();
				match next {
					Some(next) => focus = Some(next),
					None => break,
				}
			}
		}

		let mut override_focus: Option<Window> = None;

		if focus.as_ref().is_some_and(|f| f.x11_surface().is_some())
			&& let Some(f) = focus.as_ref()
		{
			if !app_ids.is_empty() {
				override_focus = candidates
					.iter()
					.find(|w| {
						self.window_metadata.get(*w).is_some_and(|m| {
							let Some(fm) = self.window_metadata.get(f) else {
								return false;
							};
							m.is_override_redirect()
								&& focus::is_good_override_candidate(m, fm)
								&& m.app_id == fm.app_id
						})
					})
					.cloned();
			} else if !candidates.is_empty() {
				override_focus = candidates
					.iter()
					.find(|w| {
						self.window_metadata.get(*w).is_some_and(|m| {
							let Some(fm) = self.window_metadata.get(f) else {
								return false;
							};
							m.is_override_redirect() && focus::is_good_override_candidate(m, fm)
						})
					})
					.cloned();
			}
			self.resolve_transient_overrides(candidates, Some(f), &mut override_focus, false);
		}

		// A window without a committed buffer still becomes the focus target on a
		// single output; flag it so the real present can lag until it draws.
		if let Some(f) = focus.as_ref()
			&& !self.window_has_buffer(f)
			&& let Some(meta) = self.window_metadata.get_mut(f)
		{
			meta.outdated_interactive_focus = true;
		}

		if override_focus.is_none()
			&& let Some(f) = focus.as_ref()
			&& let Some(fm) = self.window_metadata.get(f).cloned()
		{
			if controlled_focus {
				'fake: for appid in app_ids {
					for window in candidates {
						let accepts = self.window_metadata.get(window).is_some_and(|m| {
							m.app_id == *appid
								&& m.is_dropdown() && focus::is_good_override_candidate(m, &fm)
								&& m.app_id == fm.app_id
						});
						if accepts {
							override_focus = Some(window.clone());
							break 'fake;
						}
					}
				}
			} else {
				for window in candidates {
					let accepts = self
						.window_metadata
						.get(window)
						.is_some_and(|m| m.is_dropdown() && focus::is_good_override_candidate(m, &fm));
					if accepts {
						override_focus = Some(window.clone());
						break;
					}
				}
			}
			self.resolve_transient_overrides(candidates, Some(f), &mut override_focus, true);
		}

		let committed = focus.as_ref().is_some_and(|f| self.window_has_buffer(f));
		let out_focus = if committed || !global_focus { focus } else { None };

		(out_focus, override_focus, local_game_focused)
	}

	/// Follow transient links from `focus` (or the current override) to find
	/// override-redirect/dropdown override windows.
	/// Gamescope: `resolveTransientOverrides` in `pick_primary_focus_and_override`.
	///
	/// Unlike gamescope this tracks visited windows, so a transient cycle can
	/// never hang the focus pass.
	fn resolve_transient_overrides(
		&self,
		candidates: &[Window],
		focus: Option<&Window>,
		override_focus: &mut Option<Window>,
		maybe: bool,
	) {
		let Some(focus_id) = focus.and_then(|f| f.x11_surface().map(|x| x.window_id())) else {
			return;
		};
		let mut visited = std::collections::HashSet::new();
		loop {
			let override_id = override_focus
				.as_ref()
				.and_then(|w| w.x11_surface().map(|x| x.window_id()));
			let mut found = false;
			for candidate in candidates {
				let Some(meta) = self.window_metadata.get(candidate) else {
					continue;
				};
				let Some(cid) = candidate.x11_surface().map(|x| x.window_id()) else {
					continue;
				};
				let is_dropdown = if maybe {
					meta.is_dropdown()
				} else {
					meta.is_override_redirect()
				};
				let parent_matches = match override_id {
					Some(oid) => meta.transient_for == Some(oid),
					None => meta.transient_for == Some(focus_id),
				};
				if override_id != Some(cid) && cid != focus_id && parent_matches && is_dropdown {
					if !visited.insert(cid) {
						continue;
					}
					*override_focus = Some(candidate.clone());
					found = true;
					break;
				}
			}
			if !found {
				break;
			}
		}
	}

	/// Same-app decoration windows painted above the focus window.
	/// Gamescope: `pick_decoration_windows()`.
	fn pick_decoration_windows(&self, focus: Option<&Window>) -> Vec<Window> {
		use super::focus;
		let Some(focus) = focus else {
			return Vec::new();
		};
		if focus.x11_surface().is_none() {
			return Vec::new();
		}
		let Some(focus_meta) = self.window_metadata.get(focus) else {
			return Vec::new();
		};

		let mut decorations: Vec<Window> = self
			.space
			.elements()
			.filter(|w| {
				let Some(meta) = self.window_metadata.get(*w) else {
					return false;
				};
				if !focus::is_same_app_override_decoration(meta, focus_meta)
					|| self.override_window.as_ref() == Some(*w)
				{
					return false;
				}
				if meta
					.flags
					.intersects(WindowFlags::OVERLAY | WindowFlags::EXTERNAL_OVERLAY)
				{
					return false;
				}
				if !meta.map_state_viewable || !meta.class_input_output || meta.opacity == 0 {
					return false;
				}
				let rect = meta.geometry;
				rect.loc.x + rect.size.w > 0 && rect.loc.y + rect.size.h > 0
			})
			.cloned()
			.collect();

		// `space.elements()` runs bottom to top; decorations paint top to bottom.
		decorations.reverse();
		decorations
	}

	/// The effective connector strategy. Moonshine has a single Xwayland
	/// (`index 0`) and a single output, so the configured strategy wins;
	/// Steam mode promotes `SingleApplication` to `SteamControlled`.
	/// Gamescope: `DetermineAndApplyFocus()` strategy selection + `-e`.
	fn e_strategy(&self) -> super::VirtualConnectorStrategy {
		use super::VirtualConnectorStrategy;
		if self.steam_mode && self.virtual_connector_strategy == VirtualConnectorStrategy::SingleApplication {
			VirtualConnectorStrategy::SteamControlled
		} else {
			self.virtual_connector_strategy
		}
	}

	/// Keep the previous override painted under a nested popup so a dialog does
	/// not blink out. Gamescope: `carry_override_underlay()`.
	fn carry_override_underlay(
		&self,
		focus: Option<&Window>,
		previous_override: Option<&Window>,
		previous_underlay: Option<&Window>,
	) -> Option<Window> {
		let mut underlay = previous_underlay.cloned();

		if let (Some(focus), Some(override_window), Some(previous_override)) =
			(focus, self.override_window.as_ref(), previous_override)
			&& override_window != previous_override
			&& previous_override != focus
		{
			let carries = match (
				self.window_metadata.get(previous_override),
				self.window_metadata.get(focus),
			) {
				(Some(pm), Some(fm)) => pm.pid == fm.pid && super::focus::is_good_override_candidate(pm, fm),
				_ => false,
			};
			if carries {
				underlay = Some(previous_override.clone());
			}
		}

		if let Some(u) = underlay.as_ref() {
			let keep = match (self.override_window.as_ref(), self.window_metadata.get(u), focus) {
				(None, _, _) => false,
				(Some(ow), _, _) if ow == u => true,
				(_, Some(um), Some(f)) => {
					u.x11_surface().is_some()
						&& um.map_state_viewable
						&& self
							.window_metadata
							.get(f)
							.is_some_and(|fm| super::focus::is_good_override_candidate(um, fm))
				},
				_ => false,
			};
			if !keep {
				underlay = None;
			}
		}

		underlay
	}

	/// Apply the selected focus window: set keyboard/pointer targets,
	/// XDG activation state, and Smithay keyboard focus.
	fn apply_focus(&mut self, best: &Window) {
		// Clear override window when primary focus changes.
		// Gamescope: `wlserver_clear_dropdowns()` — when focus moves,
		// all override windows are dismissed.
		let old_focused_x11 = self.focused_x11_window;
		let old_focused_window = self.focused_window.clone();

		if let Some(x11) = best.x11_surface() {
			self.focused_x11_window = Some(x11.window_id());
		} else {
			// Wayland-native window: clear X11 focus tracking so stale X11
			// window IDs don't linger and mislead subsequent focus evaluations.
			self.focused_x11_window = None;
		}
		self.focused_window = Some(best.clone());

		// Write the gamescope focus contract (FOCUSED_APP/GFX/WINDOW + displays).
		if let Some(ref x11_focus) = self.x11_focus {
			let focused_app_id = self.window_metadata.get(best).map(|m| m.app_id).unwrap_or(0);
			let focused_window_id = best.x11_surface().map(|x| x.window_id()).unwrap_or(0);
			x11_focus.set_focused_window_contract(focused_app_id, focused_window_id);
			// When the overlay is raised, input routes to the overlay while
			// rendering stays on the game (matches gamescope's
			// inputFocusWindow / focusWindow split).
			if self.overlay_raised {
				x11_focus.set_focused_app_split(super::x11_focus::STEAM_BIG_PICTURE_APPID, focused_app_id);
			}
		}

		// Focus changed if either the X11 window ID or the actual window changed.
		let focus_changed = old_focused_x11 != self.focused_x11_window
			|| old_focused_window.as_ref().and_then(|w| w.wl_surface()) != best.wl_surface();
		if focus_changed {
			self.clear_dropdowns();
		}

		// Input focus: the last overlay that asked for input, else the focus
		// window. Mode 2 keeps keyboard on the game while the overlay takes
		// pointer input. Gamescope: `DetermineAndApplyFocus()` input routing.
		let input_focus = self.input_focus_window.clone().unwrap_or_else(|| best.clone());
		let input_focus_mode = self
			.window_metadata
			.get(&input_focus)
			.map(|m| m.input_focus_mode)
			.unwrap_or(0);
		let keyboard_target: Option<Window> = if input_focus_mode == 2 {
			Some(best.clone())
		} else {
			Some(input_focus.clone())
		};

		// Pointer focus follows inputFocus — the overlay when it asks for input.
		let pointer_target: Option<Window> = Some(input_focus.clone());
		self.pointer_focus_window = pointer_target.clone();

		// Raise the input focus window. Gamescope does this on every focus pass
		// (`inputFocus->Raise()`): a window regaining focus comes back above a
		// previously raised window, and while the Steam overlay is up the input
		// focus *is* the overlay, so it is kept above the game. Smithay's
		// `map_element` always raises, so a game `configure_notify` would
		// otherwise put the game back on top.
		let on_top = self.space.elements().last() == Some(&input_focus);
		if !on_top && self.space.elements().any(|w| w == &input_focus) {
			self.space.raise_element(&input_focus, false);
			self.screen_dirty = true;
			tracing::debug!(
				target: "focus",
				x11_id = ?input_focus.x11_surface().map(|x| x.window_id()),
				"Raised input focus window"
			);
		}

		// Activation state: call set_activated on old and new XDG toplevels.
		// Deactivate the old window whether it was X11 or Wayland.
		// When the overlay is raised, skip activation — the overlay is already
		// activated by update_overlay_z_order, and activating the game would
		// clobber _NET_ACTIVE_WINDOW back to the game window.
		if !self.overlay_raised {
			if let Some(ref old_win) = old_focused_window
				&& old_win.toplevel().is_some()
				&& old_win != best
			{
				old_win.set_activated(false);
			}
			if best.toplevel().is_some() {
				best.set_activated(true);
			}
		}

		// Keyboard focus persistence.
		let primary_focus_changed = old_focused_x11 != self.focused_x11_window;
		if primary_focus_changed && let Some(x11) = keyboard_target.as_ref().and_then(|w| w.x11_surface()) {
			self.last_keyboard_focus_window = Some(x11.window_id());
		}

		// Set keyboard focus via Smithay seat.
		if let Some(keyboard) = self.seat.get_keyboard() {
			let serial = smithay::utils::SERIAL_COUNTER.next_serial();
			if let Some(ref target) = keyboard_target {
				tracing::debug!(
					target: "focus",
					x11_id = ?target.x11_surface().map(|x| x.window_id()),
					wl_surface = ?target.wl_surface().map(|s| s.id()),
					"Setting keyboard focus"
				);
				keyboard.set_focus(self, Some(KeyboardFocusTarget::from(target.clone())), serial);
			}
		}

		// Force XSetInputFocus for X11 windows — necessary for GloballyActive
		// / WM_TAKE_FOCUS windows (WmHints { input: false }, e.g. HFW under
		// Proton) where Smithay only sends WM_TAKE_FOCUS but never calls
		// XSetInputFocus, so the game never receives FocusIn → WM_ACTIVATE.
		//
		// `_NET_ACTIVE_WINDOW` tracks the real keyboard focus (wine >= 10 treats
		// it as the foreground window), and a window regaining focus is pulled
		// back to the normal WM state.
		let keyboard_focus_id = keyboard_target
			.as_ref()
			.and_then(|w| w.x11_surface())
			.map(|x| x.window_id());
		let input_changed =
			self.input_focus_mode != input_focus_mode || self.current_keyboard_focus_window != keyboard_focus_id;
		if let Some(ref x11_focus) = self.x11_focus
			&& let Some(kid) = keyboard_focus_id
		{
			if input_changed {
				x11_focus.set_input_focus_revert(kid, 0);
				x11_focus.set_net_active_window(kid);
				self.current_keyboard_focus_window = Some(kid);
			}
		} else if keyboard_focus_id.is_none() {
			self.current_keyboard_focus_window = None;
		}
		self.input_focus_mode = input_focus_mode;
		if let Some(prev) = old_focused_x11
			&& old_focused_x11 != self.focused_x11_window
			&& let Some(ref x11_focus) = self.x11_focus
		{
			x11_focus.set_wm_state_normal(prev);
		}

		// Send initial pointer motion event to the pointer focus window to establish
		// pointer focus so that subsequent mouse events are delivered there.
		// Use pointer_target (not keyboard_target) — when input_focus_mode != 0
		// the overlay gets pointer events while the game gets keyboard events.
		//
		// surface_loc is the window's origin in compositor space (from space
		// geometry), NOT the cursor position. Smithay computes cursor-within-
		// surface as (event.location - surface_loc), so using cursor_position
		// as surface_loc would always report the cursor at (0,0) inside the
		// window and cause XWayland to deliver EnterNotify with event_x/y=0.
		if let Some(ref target) = pointer_target {
			if let Some(x11) = target.x11_surface() {
				if let Some(wl_surface) = x11.wl_surface() {
					let window_loc = self
						.space
						.element_geometry(target)
						.map(|g| g.loc.to_f64())
						.unwrap_or_default();
					let pointer = self.seat.get_pointer().expect("pointer should exist");
					let serial = smithay::utils::SERIAL_COUNTER.next_serial();
					tracing::debug!(
						target: "focus",
						surface_id = ?wl_surface.id(),
						cursor = ?self.cursor_position,
						"apply_focus: sending initial pointer motion event"
					);
					pointer.motion(
						self,
						Some((wl_surface.clone(), window_loc)),
						&MotionEvent {
							location: self.cursor_position,
							serial,
							time: InputTime::from_millis(self.clock.now().as_millis()),
						},
					);
					pointer.frame(self);
				}
			} else if target.wl_surface().is_some() {
				// For Wayland targets, establish pointer focus by sending a
				// motion event. Without this, pointer focus stays on the
				// previous surface until the user moves the mouse.
				let window_loc = self
					.space
					.element_geometry(target)
					.map(|g| g.loc.to_f64())
					.unwrap_or_default();
				let pointer = self.seat.get_pointer().expect("pointer should exist");
				let serial = smithay::utils::SERIAL_COUNTER.next_serial();
				tracing::debug!(
					target: "focus",
					surface_id = ?target.wl_surface().as_ref().map(|s| s.id()),
					cursor = ?self.cursor_position,
					"apply_focus: sending initial pointer motion event for Wayland"
				);
				if let Some(surface_cow) = target.wl_surface() {
					let wl_surface = surface_cow.as_ref().clone();
					pointer.motion(
						self,
						Some((wl_surface, window_loc)),
						&MotionEvent {
							location: self.cursor_position,
							serial,
							time: InputTime::from_millis(self.clock.now().as_millis()),
						},
					);
					pointer.frame(self);
				}
			}
		}

		// Mark focus as applied.
		self.focus_state.apply();
	}

	/// Re-evaluate focus based on priority ranking.
	///
	/// Gamescope: `determine_and_apply_focus()` — picks the highest-priority
	/// window from the candidate list and sets keyboard focus.
	///
	/// Decomposed into steps for clarity:
	/// 1. `refresh_metadata()` — re-read live X11 properties
	/// 2. `classify_special_windows()` — overlay/notification/etc. classification
	/// 3. `build_candidates()` — filter and collect candidate windows
	/// 4. Steam control override + priority sort
	/// 5. `enforce_fullscreen_geometry()` — hold the winner at the output size
	/// 6. `apply_focus()` — set keyboard/pointer focus, activation
	pub fn reevaluate_focus(&mut self) {
		// Mark focus as dirty before recalculating.
		self.focus_state.mark_dirty();

		// The WSI swapchain can be created before its window is mapped, so
		// re-resolve the override target now that the window set may have
		// changed. Cheap when the current mapping is already rendered.
		self.resolve_override_window();

		// Step 1: Re-read live X11 properties.
		self.refresh_metadata();

		// Step 2: Classify overlay/notification/external overlay windows.
		let windows: Vec<_> = self.space.elements().cloned().collect();
		self.classify_special_windows(&windows);

		// Step 3: Build candidate list.
		let candidates = self.build_candidates(&windows);

		// Write focusable apps and windows so Steam knows which apps/windows
		// are focusable (controller routing). Gamescope: writes
		// GAMESCOPE_FOCUSABLE_APPS and GAMESCOPE_FOCUSABLE_WINDOWS to the root
		// window so Steam can route controller input to the correct app.
		// GAMESCOPE_FOCUSABLE_WINDOWS uses [window_id, app_id, pid] triplets.
		// When the overlay is raised, BPM (769) is excluded — it's the active
		// overlay, not a focusable target.
		if let Some(ref x11_focus) = self.x11_focus {
			let overlay_raised = self.overlay_raised;
			let is_focusable =
				|aid: u32| aid != 0 && (!overlay_raised || aid != super::x11_focus::STEAM_BIG_PICTURE_APPID);
			let focusable_app_ids: Vec<u32> = candidates
				.iter()
				.filter_map(|w| self.window_metadata.get(w).map(|m| m.app_id))
				.filter(|&aid| is_focusable(aid))
				.collect();
			let focusable_triplets: Vec<[u32; 3]> = candidates
				.iter()
				.filter_map(|w| {
					let x11 = w.x11_surface()?;
					let window_id = x11.window_id();
					let meta = self.window_metadata.get(w)?;
					let app_id = meta.app_id;
					if !is_focusable(app_id) {
						return None;
					}
					// Read PID from the cached metadata or fall back to 0.
					let pid = meta
						.x11_window_id
						.map(|_| {
							// We don't have PID stored in metadata; read it from X11.
							x11_focus.get_window_pid(window_id)
						})
						.unwrap_or(0);
					Some([window_id, app_id, pid])
				})
				.collect();
			if !focusable_app_ids.is_empty() {
				x11_focus.set_focusable_apps(&focusable_app_ids);
			} else {
				x11_focus.clear_focusable_apps();
			}
			if !focusable_triplets.is_empty() {
				x11_focus.set_focusable_windows(&focusable_triplets);
			} else {
				x11_focus.clear_focusable_windows();
			}
		}

		// Handle no candidates — clear old focus.
		if candidates.is_empty() {
			tracing::debug!(
				target: "focus",
				space_windows = windows.len(),
				"No focus candidates; clearing focus"
			);
			if self.focused_x11_window.is_some() {
				self.focused_x11_window = None;
			}
			// Always clear keyboard focus when there are no candidates,
			// even if the last focused window was a Wayland window
			// (focused_x11_window == None). Without this, Smithay keyboard
			// focus can remain on a dead Wayland surface.
			if let Some(keyboard) = self.seat.get_keyboard() {
				let serial = smithay::utils::SERIAL_COUNTER.next_serial();
				keyboard.set_focus(self, None, serial);
			}
			if let Some(ref x11_focus) = self.x11_focus {
				x11_focus.clear_focused_app();
			}
			self.focus_state.apply();
			return;
		}
		tracing::debug!(
			target: "focus",
			candidate_count = candidates.len(),
			candidates = ?candidates.iter().map(|w| (
				self.window_metadata.get(w).map(|m| m.app_id),
				w.x11_surface().map(|x| x.window_id()),
			)).collect::<Vec<_>>(),
			"Focus candidates"
		);

		// Refresh the scaling settings Steam publishes on the root window.
		let scaler = self.x11_focus.as_ref().and_then(|xf| xf.get_upscale_scaler());
		if let Some(value) = scaler {
			self.upscale_scaler = super::scaling::UpscaleScaler::from_atom_value(value);
		}
		let filter = self.x11_focus.as_ref().and_then(|xf| xf.get_scaling_filter());
		if let Some(value) = filter {
			self.upscale_filter = super::scaling::filter_from_atom_value(value);
		}
		let global_scale = self.x11_focus.as_ref().map(|xf| xf.get_global_scale());
		if let Some(value) = global_scale {
			self.global_scale = value;
		}

		// Step 4: Pick the focus window and its override (dropdown) window.
		let focus_control = self.x11_focus.as_ref().and_then(|xf| xf.read_focus_control());
		let focus_control_window = focus_control.as_ref().and_then(|f| f.window);
		let app_ids: Vec<u32> = focus_control
			.as_ref()
			.map(|f| f.app_ids.iter().map(|a| a.0).collect())
			.unwrap_or_default();
		let strategy = self.e_strategy();
		// Single output: the connector key is unused on the Steam/single-app paths.
		let virtual_key = 0u64;

		let previous_override = self.override_window.clone();
		let previous_underlay = self.override_underlay_window.clone();

		let (focus, override_window, local_game_focused) = self.pick_primary_focus_and_override(
			&candidates,
			focus_control_window,
			&app_ids,
			false,
			virtual_key,
			strategy,
		);
		self.override_window = override_window;
		tracing::debug!(
			target: "focus",
			focus_control_window = ?focus_control_window,
			app_ids = ?app_ids,
			strategy = ?strategy,
			focus = ?focus.as_ref().and_then(|w| w.x11_surface().map(|x| x.window_id())),
			override = ?self.override_window.as_ref().and_then(|w| w.x11_surface().map(|x| x.window_id())),
			local_game_focused,
			"Focus selection"
		);

		let Some(best) = focus else {
			self.focus_state.apply();
			return;
		};

		self.decoration_windows = self.pick_decoration_windows(Some(&best));
		self.override_underlay_window =
			self.carry_override_underlay(Some(&best), previous_override.as_ref(), previous_underlay.as_ref());

		// Step 5: Hold the focused game window at the output size.
		self.enforce_output_geometry(&best);

		// Step 6: Apply focus (keyboard/pointer target, activation, Smithay seat).
		self.apply_focus(&best);
	}

	/// Resize the focused game window to the output.
	///
	/// Gamescope holds the focus window at the output size regardless of the
	/// fullscreen hint, so a game running below the stream resolution is
	/// scaled up to fill the whole output. Only the main window is held —
	/// dialogs, dropdowns, and Steam overlay/notification windows keep their
	/// own size.
	///
	/// Correcting geometry here rather than refusing the client's
	/// `ConfigureRequest` fixes a wrong-sized window whatever caused it.
	///
	/// Gamescope: `determine_and_apply_focus()`
	fn enforce_output_geometry(&self, window: &Window) {
		let Some(meta) = self.window_metadata.get(window) else {
			return;
		};
		if !meta.should_fill_output() {
			return;
		}

		let Some(x11) = window.x11_surface() else {
			return;
		};
		let geo = self.output_rect();
		if x11.geometry().size == geo.size {
			return;
		}

		tracing::debug!(
			target: "focus",
			window_id = x11.window_id(),
			current = ?x11.geometry().size,
			output = ?geo.size,
			"Resizing game window to output size"
		);
		if let Err(e) = x11.configure(geo) {
			tracing::warn!("Failed to resize game window to output size: {e}");
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
			state.states.set(XdgToplevelState::Maximized);
		});
		surface.send_configure();

		// Resolve app_id from Wayland client PID (matches gamescope: wlserver.cpp:1870)
		let app_id = if let Some(client) = surface.wl_surface().client() {
			if let Ok(creds) = client.get_credentials(&self.display_handle) {
				get_appid_from_pid(creds.pid as u32)
			} else {
				0
			}
		} else {
			0
		};

		// Read fullscreen state before surface is consumed by new_wayland_window.
		let fullscreen = surface.with_committed_state(|state| {
			state
				.as_ref()
				.map(|s| s.states.contains(XdgToplevelState::Fullscreen))
				.unwrap_or(false)
		});

		let window = Window::new_wayland_window(surface);
		self.space.map_element(window.clone(), (0, 0), false);

		// Store metadata for focus priority decisions. A native Wayland
		// toplevel is mapped and opaque by definition; the X11-only
		// viewable/InputOutput/opacity checks in `build_candidates` must not
		// exclude it.
		self.map_sequence_counter += 1;
		let meta = WindowMetadata {
			app_id,
			map_sequence: self.map_sequence_counter,
			geometry: self.output_rect(),
			fullscreen,
			map_state_viewable: true,
			class_input_output: true,
			opacity: 255,
			..Default::default()
		};
		self.window_metadata.insert(window.clone(), meta);

		// Re-evaluate focus based on priority ranking.
		self.reevaluate_focus();
	}

	fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
		// Remove metadata for destroyed XDG toplevels and unmap from space.
		let target = surface.wl_surface();
		let window = self
			.space
			.elements()
			.find(|w| w.toplevel().map(|t| t.wl_surface() == target).unwrap_or(false))
			.cloned();

		if let Some(window) = window {
			self.unregister_window(&window);
			self.space.unmap_elem(&window);
		}

		// Re-evaluate focus — if the destroyed window had keyboard focus,
		// Smithay still targets that dead surface until focus is recomputed.
		self.reevaluate_focus();
	}

	fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
		surface.with_pending_state(|state| {
			state.states.set(XdgToplevelState::Fullscreen);
		});
		surface.send_configure();

		// Update fullscreen state in metadata.
		let target = surface.wl_surface();
		let geometry = self.output_rect();
		if let Some(window) = self.find_window_by_surface(target)
			&& let Some(meta) = self.window_metadata.get_mut(&window)
		{
			meta.fullscreen = true;
			meta.geometry = geometry;
		}
		self.reevaluate_focus();
	}

	fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
		surface.with_pending_state(|state| {
			state.states.unset(XdgToplevelState::Fullscreen);
		});
		surface.send_configure();

		// Update fullscreen state in metadata.
		let target = surface.wl_surface();
		let geometry = self.output_rect();
		if let Some(window) = self.find_window_by_surface(target)
			&& let Some(meta) = self.window_metadata.get_mut(&window)
		{
			meta.fullscreen = false;
			meta.geometry = geometry;
		}
		self.reevaluate_focus();
	}

	fn app_id_changed(&mut self, surface: ToplevelSurface) {
		// Update app_id when the Wayland client changes its app_id.
		let target = surface.wl_surface();
		if let Some(window) = self.find_window_by_surface(target)
			&& let Some(meta) = self.window_metadata.get_mut(&window)
			&& let Some(client) = surface.wl_surface().client()
			&& let Ok(creds) = client.get_credentials(&self.display_handle)
		{
			meta.app_id = get_appid_from_pid(creds.pid as u32);
		}
		self.reevaluate_focus();
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
		let window_id = focused.map(|f| f.window().x11_surface().map(|x| x.window_id()));
		tracing::debug!(target: "focus", window_id = ?window_id, "Keyboard focus changed");
	}

	fn led_state_changed(&mut self, _seat: &Seat<Self>, _led_state: smithay::input::keyboard::LedState) {}
}

impl TabletSeatHandler for MoonshineCompositor {
	type ToolFocus = WlSurface;

	fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
		self.cursor_status = image;
	}
}

// -- Selection / Data Device Handlers --

impl SelectionHandler for MoonshineCompositor {
	type SelectionUserData = ();
}

impl DataDeviceHandler for MoonshineCompositor {
	fn data_device_state(&mut self) -> &mut DataDeviceState {
		&mut self.data_device_state
	}
}

impl WaylandDndGrabHandler for MoonshineCompositor {}
impl DndGrabHandler for MoonshineCompositor {}

// -- Output Handler --

impl OutputHandler for MoonshineCompositor {
	fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

// -- Pointer Constraints Handler --

impl PointerConstraintsHandler for MoonshineCompositor {
	fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
		// Auto-activate constraints when the surface already has pointer focus.
		if let Some(current_focus) = pointer.current_focus()
			&& &current_focus == surface
		{
			with_pointer_constraint(surface, pointer, |constraint| {
				if let Some(c) = constraint {
					c.activate();
				}
			});
		}
	}

	fn remove_constraint(
		&mut self,
		_surface: &WlSurface,
		_pointer: &PointerHandle<Self>,
		_constraint: ConstraintRemove,
	) {
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
				.find_map(|window| (window.wl_surface().as_deref() == Some(surface)).then(|| window.geometry()))
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

// -- XDG Activation Handler --

impl XdgActivationHandler for MoonshineCompositor {
	fn activation_state(&mut self) -> &mut XdgActivationState {
		&mut self.activation_state
	}

	fn request_activation(
		&mut self,
		token: XdgActivationToken,
		token_data: XdgActivationTokenData,
		surface: WlSurface,
	) {
		// Ignore stale tokens (e.g. replayed portal/clipboard tokens).
		if token_data.timestamp.elapsed() > std::time::Duration::from_secs(10) {
			tracing::debug!(target: "focus", token = token.as_str(), "xdg-activation: ignoring stale token");
			return;
		}
		// Only track surfaces that belong to a toplevel window we manage.
		if self.find_window_by_surface(&surface).is_none() {
			tracing::debug!(target: "focus", surface_id = ?surface.id(), "xdg-activation: surface has no toplevel window");
			return;
		}
		tracing::debug!(
			target: "focus",
			token = token.as_str(),
			surface_id = ?surface.id(),
			app_id = ?token_data.app_id,
			"xdg-activation: activation requested"
		);
		self.focus_state.set_requested_focus_surface(surface);
		self.reevaluate_focus();
	}
}

delegate_dispatch2!(MoonshineCompositor);

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

		// Re-apply keyboard focus now that the wl_surface is available.
		//
		// When apply_focus() first ran (at X11 map-request time) the wl_surface
		// didn't exist yet, so Smithay couldn't send wl_keyboard.enter to the
		// game's surface.  For GloballyActive windows (WmHints { input: false },
		// WM_TAKE_FOCUS protocol — e.g. HFW under Proton) Smithay also does NOT
		// call XSetInputFocus; instead it sends a WM_TAKE_FOCUS ClientMessage
		// and waits for the client to call XSetInputFocus itself.  The game may
		// ignore that early WM_TAKE_FOCUS because it arrives before the window
		// is fully initialised.
		//
		// By forcing a leave→enter cycle here (clear focus, then re-apply) we
		// deliver a second WM_TAKE_FOCUS and a proper wl_keyboard.enter to the
		// game's actual wl_surface, matching the behaviour of the main branch's
		// ProxiedX11 → direct-Window focus transition that happened in the old
		// surface_associated handler.
		//
		// Only do this if the surface belongs to the currently focused window.
		let focused_id = self.focused_x11_window;
		if let Some(focused_id) = focused_id {
			if surface.window_id() != focused_id {
				return;
			}

			let focused_window = self
				.space
				.elements()
				.find(|w| w.x11_surface().is_some_and(|x| x.window_id() == focused_id))
				.cloned();

			if let Some(focused_window) = focused_window {
				tracing::debug!(
					target: "focus",
					window_id = focused_id,
					wl_surface = ?wl_surface.id(),
					"surface_associated: re-applying keyboard focus now that wl_surface is available"
				);

				if let Some(keyboard) = self.seat.get_keyboard() {
					// Clear focus first so the subsequent set_focus is seen as a
					// change (not "unchanged"), guaranteeing enter() is called and
					// WM_TAKE_FOCUS + wl_keyboard.enter are delivered.
					let serial = smithay::utils::SERIAL_COUNTER.next_serial();
					keyboard.set_focus(self, None, serial);
					let serial = smithay::utils::SERIAL_COUNTER.next_serial();
					keyboard.set_focus(self, Some(KeyboardFocusTarget::from(focused_window.clone())), serial);
				}

				// For GloballyActive windows (WmHints { input: false }, TakeFocus
				// protocol — e.g. HFW under Proton), Smithay's enter() only sends
				// WM_TAKE_FOCUS and never calls XSetInputFocus.  Wine may not
				// respond to WM_TAKE_FOCUS reliably at this stage, so we call
				// XSetInputFocus directly via our Xlib connection, mirroring
				// gamescope's sync_x11_focus().  This generates a direct FocusIn
				// event that Wine translates to WM_ACTIVATE, triggering audio
				// and cursor initialisation without requiring a click.
				if let Some(ref x11_focus) = self.x11_focus {
					x11_focus.set_input_focus(focused_id);
				}

				// Also establish pointer focus now that the surface exists.
				// surface_loc is the window's origin in compositor space (0,0
				// for fullscreen windows), not the cursor position.
				let window_loc = self
					.space
					.element_geometry(&focused_window)
					.map(|g| g.loc.to_f64())
					.unwrap_or_default();
				let pointer = self.seat.get_pointer().expect("pointer should exist");
				let serial = smithay::utils::SERIAL_COUNTER.next_serial();
				tracing::debug!(
					target: "focus",
					surface_id = ?wl_surface.id(),
					cursor = ?self.cursor_position,
					?window_loc,
					"surface_associated: sending initial pointer motion event"
				);
				pointer.motion(
					self,
					Some((wl_surface.clone(), window_loc)),
					&MotionEvent {
						location: self.cursor_position,
						serial,
						time: InputTime::from_millis(self.clock.now().as_millis()),
					},
				);
				pointer.frame(self);
			}
		}
	}
}

// -- XWM Handler (X11 Window Manager) --

impl XwmHandler for MoonshineCompositor {
	fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
		self.xwm.as_mut().expect("XWayland WM not initialized")
	}

	fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: smithay::xwayland::xwm::WmWindowProperty) {
		// STEAM_OVERLAY (forwarded as Other) drives overlay z-order: mark it
		// dirty so update_overlay_z_order runs this frame instead of polling.
		if let smithay::xwayland::xwm::WmWindowProperty::Other(atom) = property
			&& self
				.x11_focus
				.as_ref()
				.is_some_and(|xf| xf.steam_overlay_atom() as u32 == atom)
		{
			self.overlay_dirty = true;
			self.screen_dirty = true;

			// STEAM_OVERLAY is commonly set after the window is mapped, so the
			// map-time classification is stale. Refresh it here so the window is
			// treated as an overlay (candidate filtering and input focus).
			// Gamescope: re-reads isOverlay on this PropertyNotify.
			let is_overlay = self.with_x11_focus(|xf| xf.get_steam_overlay_value(window.window_id())) != 0;
			let root_width = self.width as i32;
			if let Some(elem) = self.find_window_by_x11_surface(&window)
				&& let Some(meta) = self.window_metadata.get_mut(&elem)
			{
				meta.is_overlay = is_overlay;
				meta.flags.remove(WindowFlags::OVERLAY | WindowFlags::NOTIFICATION);
				if is_overlay {
					if meta.geometry.size.w >= root_width || meta.input_focus_mode != 0 {
						meta.flags.insert(WindowFlags::OVERLAY);
					} else {
						meta.flags.insert(WindowFlags::NOTIFICATION);
					}
				}
			}
			self.reevaluate_focus();
			return;
		}

		// Only re-evaluate focus for properties that affect focus ranking.
		// Gamescope: handles PropertyNotify selectively for focus-relevant
		// atoms. Smithay forwards any other atom as WmWindowProperty::Other;
		// we re-read Steam properties in reevaluate_focus() itself.
		//
		// Focus-relevant properties:
		// - Pid: _NET_WM_PID → app_id resolution (critical!)
		// - Hints: WM hints → input focus
		// - TransientFor: transient parent → dropdown detection
		// - WindowType: dialog vs normal window
		let focus_relevant = matches!(
			property,
			smithay::xwayland::xwm::WmWindowProperty::Pid
				| smithay::xwayland::xwm::WmWindowProperty::Hints
				| smithay::xwayland::xwm::WmWindowProperty::TransientFor
				| smithay::xwayland::xwm::WmWindowProperty::WindowType
		);
		if !focus_relevant {
			return;
		}
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			?property,
			"X11 property notify (focus-relevant)"
		);

		// Re-read metadata fields that are derived from the changed property
		// and update the stored metadata. reevaluate_focus() only refreshes
		// STEAM_INPUT_FOCUS and app_id; stale Hints/TransientFor/WindowType
		// derived fields must be explicitly refreshed here.
		let win_elem = self
			.space
			.elements()
			.find(|e| e.x11_surface().is_some_and(|x| x.window_id() == window.window_id()))
			.cloned();
		if let Some(elem) = win_elem {
			let new_meta = self.x11_surface_metadata(&window);
			if let Some(meta) = self.window_metadata.get_mut(&elem) {
				// Selectively update only the fields that can change from these
				// property notifications. Preserve sequence numbers.
				match property {
					smithay::xwayland::xwm::WmWindowProperty::Hints => {
						meta.hwnd_style = new_meta.hwnd_style;
						meta.hwnd_style_ex = new_meta.hwnd_style_ex;
					},
					smithay::xwayland::xwm::WmWindowProperty::TransientFor => {
						let old_parent = meta.transient_for;
						let new_parent = new_meta.transient_for;
						meta.transient_for = new_parent;
						meta.skip_taskbar = new_meta.skip_taskbar;
						meta.skip_pager = new_meta.skip_pager;
						meta.maybe_a_dropdown = new_meta.maybe_a_dropdown;

						// Update transient_children index when transient_for changes.
						// Remove from old parent's child list.
						if let Some(old_parent) = old_parent
							&& let Some(children) = self.transient_children.get_mut(&old_parent)
						{
							children.retain(|w| *w != elem);
							if children.is_empty() {
								self.transient_children.remove(&old_parent);
							}
						}
						// Add to new parent's child list.
						if let Some(new_parent) = new_parent {
							self.transient_children
								.entry(new_parent)
								.or_default()
								.push(elem.clone());
						}
					},
					smithay::xwayland::xwm::WmWindowProperty::WindowType => {
						meta.is_dialog = new_meta.is_dialog;
						meta.maybe_a_dropdown = new_meta.maybe_a_dropdown;
					},
					_ => {}, // Pid handled by refresh_metadata() in reevaluate_focus
				}
			}
		}

		self.reevaluate_focus();
	}

	fn active_window_request(
		&mut self,
		_xwm: XwmId,
		window: X11Surface,
		_timestamp: u32,
		_currently_active_window: Option<X11Surface>,
	) {
		// `_NET_ACTIVE_WINDOW` only raises the window; focus is chosen by our
		// own selection (`pick_best_candidate`).
		let window_id = window.window_id();
		if let Some(w) = self.find_window_by_x11_surface(&window) {
			self.space.raise_element(&w, false);
		}
		tracing::debug!(target: "focus", window_id, "_NET_ACTIVE_WINDOW: raised window");
	}

	fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			title = ?window.title(),
			class = ?window.class(),
			"New X11 window"
		);
	}

	fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			title = ?window.title(),
			class = ?window.class(),
			"New X11 override-redirect window"
		);
	}

	fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			target: "focus",
			title = ?window.title(),
			class = ?window.class(),
			override_redirect = window.is_override_redirect(),
			wl_surface = ?window.wl_surface(),
			"X11 window map request"
		);

		// Grant the map request. The window keeps the geometry the client
		// asked for; `enforce_output_geometry` holds the focused game window
		// (fullscreen or Steam BPM) at the output size once focus is applied.
		if let Err(e) = window.set_mapped(true) {
			tracing::error!("Failed to set X11 window mapped: {e}");
			return;
		}
		let win = Window::new_x11_window(window.clone());
		self.space.map_element(win.clone(), (0, 0), true);

		// Store metadata for focus priority decisions.
		let mut meta = self.x11_surface_metadata(&window);

		// Set map_sequence and damage_sequence for game windows.
		self.map_sequence_counter += 1;
		meta.map_sequence = self.map_sequence_counter;
		// Only mark dirty on window MAP to avoid focus jumping.
		if meta.has_game_id() {
			meta.damage_sequence = self.damage_sequence_counter;
			// If this is a game window being mapped, check if it should
			// take focus from the current focused window.
			if let Some(focused_id) = self.focused_x11_window {
				// Only trigger focus dirty if this game window has a higher
				// damage sequence than the focused window, indicating it's
				// "newer" and should potentially take focus.
				if let Some(focused_meta) = self
					.window_metadata
					.values()
					.find(|m| m.x11_window_id == Some(focused_id))
					&& meta.damage_sequence > focused_meta.damage_sequence
				{
					self.focus_state.mark_dirty();
				}
			}
		} else {
			meta.damage_sequence = 0;
		}

		// Register transient children index (read transient_for before moving meta).
		let parent_id = meta.transient_for;
		self.window_metadata.insert(win.clone(), meta);

		if let Some(parent_id) = parent_id {
			self.transient_children.entry(parent_id).or_default().push(win.clone());
		}

		// Re-evaluate focus based on priority ranking.
		self.reevaluate_focus();
	}

	fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			target: "focus",
			title = ?window.title(),
			class = ?window.class(),
			geometry = ?window.geometry(),
			"X11 override-redirect window mapped"
		);
		let location = window.geometry().loc;
		let win = Window::new_x11_window(window.clone());
		// Map with activate=false so the game window keeps
		// _NET_WM_STATE_FOCUSED.  Wine uses that property to determine
		// the foreground window and only delivers input to it.
		self.space.map_element(win.clone(), location, false);

		// Store metadata for focus priority decisions (override-redirect
		// windows like dropdowns/menus are tracked but deprioritized).
		let meta = self.x11_surface_metadata(&window);
		let parent_id = meta.transient_for;
		let is_dropdown = meta.is_dropdown();

		// Insert metadata BEFORE calling is_transient_of_focused so that
		// the transient chain lookup can find this window's parent entry.
		self.window_metadata.insert(win.clone(), meta);

		let is_focused_child = self.is_transient_of_focused(window.window_id());

		// Register transient children index.
		if let Some(parent_id) = parent_id {
			self.transient_children.entry(parent_id).or_default().push(win.clone());
		}

		// Track as override window if it's a transient child of the
		// currently focused window. Override windows (dropdowns, menus,
		// tooltips) appear on top while the primary focus remains on
		// the main game window. Gamescope: `wlserver_notify_dropdown()`.
		if is_focused_child && is_dropdown {
			// Re-read metadata from map.
			let meta = self.window_metadata.get(&win).expect("metadata was just inserted");

			// Validate the dropdown is on-screen (Task 3.1).
			let output_size = self
				.output
				.current_mode()
				.map(|m| m.size)
				.unwrap_or((self.width as i32, self.height as i32).into());
			let geo = &meta.geometry;
			let on_screen = geo.loc.x + geo.size.w > 0
				&& geo.loc.x < output_size.w
				&& geo.loc.y + geo.size.h > 0
				&& geo.loc.y < output_size.h;
			if !on_screen {
				tracing::debug!(
					target: "focus",
					window_id = win.x11_surface().map(|x| x.window_id()),
					"Rejecting dropdown: off-screen"
				);
				return;
			}

			// Ensure notification/external-overlay classification is up to date
			// before notify_dropdown checks for conflicts with those windows.
			let windows: Vec<_> = self.space.elements().cloned().collect();
			self.classify_special_windows(&windows);

			// Register the dropdown with the compositor state.
			// This checks for conflicts with notification/external overlay windows.
			// Do NOT give keyboard focus to the dropdown — keyboard focus
			// stays on the previously focused window (keyboard focus
			// persistence). Gamescope keeps keyboard focus on the main game
			// window while dropdowns only receive pointer events.
			let _ = self.notify_dropdown(win.clone(), location.x, location.y);
		} else if !is_dropdown {
			// Non-dropdown override-redirect windows (STEAM_OVERLAY,
			// notifications, external overlays) also need focus
			// reevaluation so they can become overlay_window or update
			// pointer/keyboard routing immediately.
			self.reevaluate_focus();
		}
	}

	fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
		let unmapped_id = window.window_id();
		let was_focused = Some(unmapped_id) == self.focused_x11_window;

		// Check if the currently focused window is a transient child of
		// this window. If so, focus should return to the parent.
		let focused_id = self.focused_x11_window;
		let focused_is_transient_child = focused_id.is_some_and(|fid| {
			self.space
				.elements()
				.find(|e| e.x11_surface().is_some_and(|x| x.window_id() == fid))
				.and_then(|e| self.window_metadata.get(e))
				.is_some_and(|m| m.transient_for == Some(unmapped_id))
		});

		let maybe = self.find_window_by_x11_surface(&window);

		// Check if this window is an overlay/notification/external-overlay
		// that might be pointed to by pointer_focus_window or the special
		// window trackers, regardless of whether it was the focused window.
		let is_special = maybe.as_ref().is_some_and(|elem| {
			self.window_metadata.get(elem).is_some_and(|m| {
				m.flags.intersects(
					WindowFlags::OVERLAY
						| WindowFlags::NOTIFICATION
						| WindowFlags::EXTERNAL_OVERLAY
						| WindowFlags::STREAMING_CLIENT
						| WindowFlags::STREAMING_CLIENT_VIDEO
						| WindowFlags::VR_OVERLAY_TARGET,
				)
			})
		});

		if let Some(elem) = maybe {
			self.unregister_window(&elem);
			self.space.unmap_elem(&elem);

			// Clear override window if it was the unmapped window.
			if self.override_window.as_ref() == Some(&elem) {
				self.clear_dropdowns();
			}
		}
		if !window.is_override_redirect() {
			let _ = window.set_mapped(false);
		}

		// If the focused window was unmapped, or a transient child of the
		// focused window was unmapped, clear focus and re-evaluate.
		// Also re-evaluate when focused_x11_window is None (a Wayland window
		// had focus) to ensure Smithay keyboard focus is properly cleared.
		// Additionally re-evaluate when a special (overlay/notification/etc.)
		// window unmapped — pointer_focus_window and the GAMESCOPE focusable
		// lists may be stale even if the focused window wasn't affected.
		if was_focused || focused_is_transient_child || self.focused_x11_window.is_none() || is_special {
			if was_focused || focused_is_transient_child {
				self.focused_x11_window = None;
				if let Some(keyboard) = self.seat.get_keyboard() {
					let serial = smithay::utils::SERIAL_COUNTER.next_serial();
					keyboard.set_focus(self, None, serial);
				}
			}
			self.reevaluate_focus();
		}
	}

	fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
		// Remove metadata for destroyed X11 windows.
		let elem = self
			.space
			.elements()
			.find(|e| {
				e.x11_surface()
					.map(|x| x.window_id() == window.window_id())
					.unwrap_or(false)
			})
			.cloned();

		if let Some(elem) = elem {
			self.unregister_window(&elem);
		}
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
		// Gamescope holds the game window at the output size, so a game
		// running below the stream resolution is scaled up to fill the whole
		// output. This also anchors the WSI swapchain extent (read from the
		// X11 window) to the output before the game creates it.
		let elem = self.find_window_by_x11_surface(&window);
		let is_game = elem
			.as_ref()
			.is_some_and(|e| self.window_metadata.get(e).is_some_and(|m| m.should_fill_output()));
		if is_game {
			let _ = window.configure(self.output_rect());
			return;
		}

		// Grant geometry changes but ignore position (we control placement).
		// `enforce_output_geometry` corrects a game window that resizes itself
		// away from the output.
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
		// Update position in space and refresh geometry metadata.
		let Some(elem) = self.find_window_by_x11_surface(&window) else {
			return;
		};
		self.space.map_element(elem.clone(), geometry.loc, false);

		// A Steam overlay that resizes across the interactive/notification
		// boundary must switch flags; stale flags route input to the wrong
		// Steam window.
		let window_id = window.window_id();
		let root_width = self.width as i32;
		let needs_reclassify = self.window_metadata.get(&elem).is_some_and(|m| m.is_overlay);

		// Read steam_overlay_value BEFORE the mutable borrow to avoid conflict.
		let steam_overlay_value = if needs_reclassify {
			Some(self.with_x11_focus(|xf| xf.get_steam_overlay_value(window_id)))
		} else {
			None
		};

		if let Some(meta) = self.window_metadata.get_mut(&elem) {
			let old_flags = meta.flags;
			meta.geometry = geometry;

			// Only reclassify if the window has the STEAM_OVERLAY property.
			if let Some(sov) = steam_overlay_value
				&& sov != 0
			{
				let interactive = geometry.size.w >= root_width || meta.input_focus_mode != 0;
				if interactive {
					meta.flags.remove(WindowFlags::NOTIFICATION);
					meta.flags.insert(WindowFlags::OVERLAY);
				} else {
					meta.flags.remove(WindowFlags::OVERLAY);
					meta.flags.insert(WindowFlags::NOTIFICATION);
				}
			}

			// If classification changed, mark focus dirty so routing is updated.
			if meta.flags != old_flags {
				self.focus_state.mark_dirty();
			}
		}
	}

	fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			title = ?window.title(),
			"X11 fullscreen request"
		);
		self.set_x11_fullscreen(&window, true);
	}

	fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
		tracing::debug!(
			target: "focus",
			window_id = window.window_id(),
			title = ?window.title(),
			"X11 unfullscreen request"
		);
		self.set_x11_fullscreen(&window, false);
	}

	fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {
		// Interactive resize not needed for headless compositor.
	}

	fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
		// Interactive move not needed for headless compositor.
	}
}

#[cfg(test)]
mod tests {
	use super::scan_cmdline_for_app_id;

	// ---- cmdline scanning tests (T1) ----

	#[test]
	fn test_steam_launch_app_id() {
		assert_eq!(scan_cmdline_for_app_id(b"SteamLaunch\0AppId=12345"), 12345);
	}

	#[test]
	fn test_appids_flag() {
		assert_eq!(scan_cmdline_for_app_id(b"--appids=67890"), 67890);
	}

	#[test]
	fn test_appids_multiple_returns_first_nonzero() {
		assert_eq!(scan_cmdline_for_app_id(b"--appids=12345,67890"), 12345);
	}

	#[test]
	fn test_appids_after_double_dash() {
		// C3 fix: --appids= after -- should be detected
		assert_eq!(scan_cmdline_for_app_id(b"steamwebhelper\0--\0--appids=11111"), 11111);
	}

	#[test]
	fn test_steam_launch_no_app_id() {
		assert_eq!(scan_cmdline_for_app_id(b"SteamLaunch"), 0);
	}

	#[test]
	fn test_empty_cmdline() {
		assert_eq!(scan_cmdline_for_app_id(b""), 0);
	}

	#[test]
	fn test_app_id_zero_skipped() {
		assert_eq!(scan_cmdline_for_app_id(b"AppId=0"), 0);
	}

	#[test]
	fn test_steam_launch_with_app_id_zero_skipped() {
		// SteamLaunch followed by AppId=0 should return 0
		assert_eq!(scan_cmdline_for_app_id(b"SteamLaunch\0AppId=0"), 0);
	}

	#[test]
	fn test_appids_with_zero_and_nonzero() {
		// --appids=0,12345 should return 12345 (first non-zero)
		assert_eq!(scan_cmdline_for_app_id(b"--appids=0,12345"), 12345);
	}

	#[test]
	fn test_realistic_steam_cmdline() {
		let cmdline = b"/usr/bin/steamwebhelper\0--no-sandbox\0--appids=1091500";
		assert_eq!(scan_cmdline_for_app_id(cmdline), 1091500);
	}

	#[test]
	fn test_realistic_proton_cmdline() {
		let cmdline = b"/usr/bin/steam\0SteamLaunch\0--\0AppId=1172470\0--\0start";
		assert_eq!(scan_cmdline_for_app_id(cmdline), 1172470);
	}

	#[test]
	fn test_non_utf8_cmdline() {
		// Non-UTF8 bytes should not cause a panic — the scanner works on raw bytes.
		let cmdline = b"someproc\0\xff\xfe\0AppId=42424";
		assert_eq!(scan_cmdline_for_app_id(cmdline), 42424);
	}
}
