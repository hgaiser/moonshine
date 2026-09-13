//! Window focus management for the Moonshine compositor.
//!
//! Two concerns are handled here:
//!
//! 1. **KeyboardFocusTarget** — A wrapper around `Window` that implements
//!    Smithay's `KeyboardTarget`, `IsAlive`, and `WaylandFocus` traits.
//!    This is the type used by Smithay's seat keyboard focus system.
//!
//! 2. **Focus priority ranking** — Gamescope-style logic for deciding which
//!    window should receive focus when multiple windows are present. See the
//!    module-level documentation for the full decision tree.

use std::borrow::Cow;

use bitflags::bitflags;
use smithay::backend::input::{InputTime, KeyState};
use smithay::desktop::{Window, WindowSurface};
use smithay::input::Seat;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;

use crate::session::compositor::state::MoonshineCompositor;

// ============================================================================
// KeyboardFocusTarget — Smithay keyboard focus wrapper
// ============================================================================

/// Focus target for keyboard input. Wraps a `Window` and delegates keyboard
/// events to the underlying Wayland or X11 surface.
///
/// This was simplified from an enum (with `X11`, `Wayland`, and `ProxiedX11`
/// variants) to a newtype wrapper. The `ProxiedX11` variant was removed when
/// the proxy surface mechanism was eliminated — proxy surfaces were used to
/// route keyboard events through an intermediary X11 window, but the current
/// architecture routes keyboard input directly via `Window::underlying_surface()`
/// dispatch, making the proxy indirection unnecessary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KeyboardFocusTarget(Window);

impl KeyboardFocusTarget {
	/// Get a reference to the inner Window.
	pub fn window(&self) -> &Window {
		&self.0
	}
}

impl IsAlive for KeyboardFocusTarget {
	#[inline]
	fn alive(&self) -> bool {
		self.0.alive()
	}
}

impl WaylandFocus for KeyboardFocusTarget {
	#[inline]
	fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
		self.0.wl_surface()
	}
}

impl From<Window> for KeyboardFocusTarget {
	#[inline]
	fn from(w: Window) -> Self {
		KeyboardFocusTarget(w)
	}
}

/// Delegate a `KeyboardTarget` method to the underlying Wayland or X11 surface.
///
/// Collapses four nearly-identical match arms (`enter`, `leave`, `key`,
/// `modifiers`) into a single macro invocation.
macro_rules! delegate_keyboard {
	($method:ident($($param:ident : $ty:ty),*) -> $ret:ty) => {
		fn $method(&self, $($param: $ty),*) -> $ret {
			match self.0.underlying_surface() {
				WindowSurface::Wayland(w) => KeyboardTarget::$method(w.wl_surface(), $($param),*),
				WindowSurface::X11(s) => KeyboardTarget::$method(s, $($param),*),
			}
		}
	};
}

impl KeyboardTarget<MoonshineCompositor> for KeyboardFocusTarget {
	delegate_keyboard!(enter(seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, keys: Vec<KeysymHandle<'_>>, serial: Serial) -> ());
	delegate_keyboard!(leave(seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, serial: Serial) -> ());
	delegate_keyboard!(key(seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: InputTime) -> ());
	delegate_keyboard!(modifiers(seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, modifiers: ModifiersState, serial: Serial) -> ());
}

// ============================================================================
// Focus priority ranking — gamescope-style decision tree
// ============================================================================

/// Metadata collected about each window for focus priority decisions.
/// Mirrors the relevant fields from `steamcompmgr_win_t` in gamescope.
#[derive(Debug, Clone, Default)]
pub(crate) struct WindowMetadata {
	/// Application/game ID (0 = not a game). Mirrors `steamcompmgr_win_t::appID`.
	/// Gamescope detects this via _NET_WM_PID + walking `/proc/<pid>/cmdline`
	/// for "SteamLaunch AppId=N" in the parent process chain.
	pub app_id: u32,

	/// `STEAM_GAME` property: the app id the window belongs to (0 when unset).
	/// Mirrors `steamcompmgr_win_t::steamAppID`.
	pub steam_app_id: u32,

	/// `STEAM_LEGACY_BIG_PICTURE` property set. Mirrors
	/// `steamcompmgr_win_t::isSteamLegacyBigPicture`.
	pub steam_legacy_big_picture: bool,

	/// Raw `STEAM_OVERLAY` property: the window is a Steam overlay/notification
	/// before the interactive/notification split. Mirrors `steamcompmgr_win_t::isOverlay`.
	pub is_overlay: bool,

	/// `_WINE_HWND_STYLE`/`_WINE_HWND_EXSTYLE` were present.
	pub has_hwnd_style: bool,
	pub has_hwnd_style_ex: bool,

	/// X11 map_state == IsViewable.
	pub map_state_viewable: bool,

	/// X11 window c_class == InputOutput (false for InputOnly).
	pub class_input_output: bool,

	/// Set when the window was picked while it had no committed buffer, so the
	/// real presenting focus can lag until it draws. Gamescope:
	/// `steamcompmgr_win_t::outdatedInteractiveFocus`.
	pub outdated_interactive_focus: bool,

	/// X11 window ID of this window (None for Wayland-only windows).
	pub x11_window_id: Option<u32>,

	/// Owning client PID (0 when unknown).
	pub pid: u32,

	/// X11 window ID of the transient-for parent (None = no parent).
	pub transient_for: Option<u32>,

	/// Window has _NET_WM_STATE_SKIP_TASKBAR set.
	pub skip_taskbar: bool,

	/// Window has _NET_WM_STATE_SKIP_PAGER set.
	pub skip_pager: bool,

	/// Window type is _NET_WM_WINDOW_TYPE_DIALOG.
	pub is_dialog: bool,

	/// Window identified as a dropdown candidate (popup menu, etc.).
	pub maybe_a_dropdown: bool,

	/// Fixed requested size from min==max size hints (0 when unset).
	pub requested_width: u32,
	pub requested_height: u32,

	/// `_WINE_HWND_STYLE` bits (0 when unset).
	pub hwnd_style: u32,

	/// `_WINE_HWND_EXSTYLE` bits (0 when unset).
	pub hwnd_style_ex: u32,

	/// Monotonically increasing sequence number assigned at window map time.
	/// Used as a tiebreaker for game windows.
	pub map_sequence: u64,

	/// Window is override-redirect.
	pub override_redirect: bool,

	/// True if this window is an X11 window (vs native Wayland).
	pub is_x11: bool,

	/// Window geometry — used to detect 1x1 "useless" windows.
	pub geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,

	/// Whether the window is fullscreen.
	pub fullscreen: bool,

	/// Window opacity (0-255). Used to select the highest-opacity overlay.
	/// Gamescope: `win->opacity` — 0 = fully transparent, 255 = opaque.
	pub opacity: u32,

	/// STEAM_INPUT_FOCUS window property.
	/// 0 = normal (keyboard and pointer focus on same window).
	/// 2 = separate keyboard/pointer focus — keyboard stays on main window
	///     while pointer routes to overlay (used by Steam overlay).
	/// Gamescope: reads steamInputFocusAtom property.
	pub input_focus_mode: u32,

	/// Classification flags for overlay/tray/streaming windows.
	/// Packed into a single u8 — these 7 bools are set once during metadata
	/// construction and read only in `build_candidates` filtering.
	/// See `WindowFlags` for individual bit definitions.
	pub flags: WindowFlags,

	/// Monotonically increasing damage sequence counter, incremented on each
	/// surface commit for game windows (app_id != 0). Used to detect when
	/// a game window has drawn since the last focus change.
	pub damage_sequence: u64,
}

impl WindowMetadata {
	/// An opaque Steam focus identifier, not necessarily an X11 resource.
	pub fn steam_window_id(&self) -> u32 {
		self.x11_window_id
			.unwrap_or(0x8000_0000 | (self.map_sequence as u32 & 0x7fff_ffff))
	}

	/// Returns `true` if this window has a game ID (non-zero appID).
	/// Gamescope: `win_has_game_id()`
	pub fn has_game_id(&self) -> bool {
		self.app_id != 0
	}

	/// Returns `true` if the window is override-redirect.
	/// Gamescope: `win_is_override_redirect()` — ignores 1x1 "useless" windows.
	pub fn is_override_redirect(&self) -> bool {
		self.override_redirect && !self.is_useless()
	}

	/// Returns `true` if the window is a system tray icon or 1x1 ("useless").
	pub fn is_useless(&self) -> bool {
		self.flags.contains(WindowFlags::SYS_TRAY_ICON) || (self.geometry.size.w == 1 && self.geometry.size.h == 1)
	}

	/// Returns `true` if the window is a dropdown candidate.
	pub fn is_dropdown(&self) -> bool {
		// Warframe (230410): positioned transient popups with skip flags.
		if self.app_id == 230410
			&& self.maybe_a_dropdown
			&& self.transient_for.is_some()
			&& (self.skip_pager || self.skip_taskbar)
		{
			return !self.is_useless();
		}
		// Antichamber (219890) splash screen.
		if self.app_id == 219890 {
			return false;
		}

		// WS_EX_CONTROLPARENT | WS_EX_LAYERED, excluding WS_EX_APPWINDOW.
		const WS_EX_CONTROLPARENT: u32 = 0x0001_0000;
		const WS_EX_APPWINDOW: u32 = 0x0004_0000;
		const WS_EX_LAYERED: u32 = 0x0008_0000;
		if self.has_hwnd_style_ex
			&& (self.hwnd_style_ex & (WS_EX_CONTROLPARENT | WS_EX_LAYERED)) == (WS_EX_CONTROLPARENT | WS_EX_LAYERED)
			&& self.hwnd_style_ex & WS_EX_APPWINDOW == 0
		{
			return true;
		}

		// Forza Horizon 4/5 background window.
		if (self.app_id == 1293830 || self.app_id == 1551360)
			&& self.maybe_a_dropdown
			&& self.requested_width == 0
			&& self.requested_height == 0
		{
			return false;
		}

		let valid_maybe_a_dropdown = self.maybe_a_dropdown
			&& ((!self.is_dialog || (self.transient_for.is_none() && self.skip_and_not_fullscreen()))
				&& (self.skip_pager || self.skip_taskbar));

		(valid_maybe_a_dropdown || self.is_override_redirect()) && !self.is_useless()
	}

	/// Returns `true` if the window is disabled (`WS_DISABLED`, 0x80000000).
	pub fn is_disabled(&self) -> bool {
		if !self.has_hwnd_style {
			return false;
		}
		self.hwnd_style & 0x8000_0000 != 0
	}

	/// Returns `true` if this is a Steam Big Picture window.
	/// Gamescope: detects STEAM_LEGACY_BIG_PICTURE property.
	pub fn is_steam_big_picture(&self) -> bool {
		self.app_id == crate::session::compositor::x11_focus::STEAM_BIG_PICTURE_APPID
	}

	/// Returns `true` if the window is Steam's own UI.
	/// Gamescope: `window_is_steam()` — legacy BPM flag or app id 769.
	pub fn is_steam(&self) -> bool {
		self.steam_legacy_big_picture || self.app_id == crate::session::compositor::x11_focus::STEAM_BIG_PICTURE_APPID
	}

	/// Returns `true` if the window should be held at the output size.
	///
	/// Only a window that actually went fullscreen is held at the output size;
	/// a windowed launcher (e.g. Rockstar Games Launcher at 1280x600) keeps its
	/// own size and is scaled by the compositor instead. Steam Big Picture
	/// counts even without the fullscreen state set. Dialogs, dropdowns, and
	/// overlay/notification windows keep their own size.
	pub fn should_fill_output(&self) -> bool {
		(self.fullscreen || self.is_steam_big_picture())
			&& self.has_game_id()
			&& self.transient_for.is_none()
			&& !self.is_dropdown()
			&& !self
				.flags
				.intersects(WindowFlags::OVERLAY | WindowFlags::NOTIFICATION | WindowFlags::EXTERNAL_OVERLAY)
	}

	/// Returns `true` if the window has skipTaskbar AND skipPager but is not fullscreen.
	/// Gamescope: `win_skip_and_not_fullscreen()`
	pub fn skip_and_not_fullscreen(&self) -> bool {
		(self.skip_taskbar && self.skip_pager) && !self.fullscreen
	}
}

bitflags! {
	/// Classification flags for overlay, tray, streaming, and VR windows.
	///
	/// These replace 7 individual bool fields on `WindowMetadata`, cutting the
	/// struct from 23 to ~16 fields and improving cache locality.
	/// Each flag corresponds to a gamescope `steamcompmgr_win_t` boolean field.
	#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
	pub struct WindowFlags: u8 {
		/// Steam overlay (STEAM_OVERLAY != 0 AND width > 1200).
		const OVERLAY = 1 << 0;
		/// Steam notification (STEAM_OVERLAY != 0 AND width <= 1200).
		const NOTIFICATION = 1 << 1;
		/// External overlay (GAMESCOPE_EXTERNAL_OVERLAY property).
		const EXTERNAL_OVERLAY = 1 << 2;
		/// Steam streaming client (STEAM_STREAMING_CLIENT property).
		const STREAMING_CLIENT = 1 << 3;
		/// Steam streaming client video (STEAM_STREAMING_CLIENT_VIDEO property).
		const STREAMING_CLIENT_VIDEO = 1 << 4;
		/// System tray icon (_NET_SYSTEM_TRAY_OPCODE client message).
		const SYS_TRAY_ICON = 1 << 5;
		/// VR overlay target (SteamGamescopeVROverlayTarget property).
		const VR_OVERLAY_TARGET = 1 << 6;

		/// Combined mask: all flags that cause a window to be skipped as a
		/// focus candidate in `build_candidates`.
		const SKIP_FOCUS = Self::OVERLAY.bits()
			| Self::NOTIFICATION.bits()
			| Self::EXTERNAL_OVERLAY.bits()
			| Self::STREAMING_CLIENT.bits()
			| Self::STREAMING_CLIENT_VIDEO.bits()
			| Self::SYS_TRAY_ICON.bits()
			| Self::VR_OVERLAY_TARGET.bits();
	}
}

/// Compute a priority key for a window. Higher tuple values mean higher focus
/// priority.
///
/// This replaces the old `window_priority_greater` comparison function. The old
/// approach returned `Ordering` via a cascading if-chain, but step 11
/// (transient-child promotion via direct parent-child check) violated
/// transitivity — which means `sort_by()` could produce unspecified behavior
/// (Rust's sort requires a strict total ordering).
///
/// The tuple approach guarantees a strict total ordering because tuple
/// comparison is inherently transitive.
///
/// Gamescope: `is_focus_priority_greater()` — cascading decision tree.
pub(crate) fn get_window_priority_key(
	w: &WindowMetadata,
) -> (bool, bool, bool, bool, bool, bool, bool, bool, bool, u64, u64) {
	(
		w.has_game_id(),              // 1. Game windows over non-game
		!w.is_override_redirect(),    // 2. Non-override-redirect over override-redirect
		!w.is_useless(),              // 3. Non-1x1 over 1x1 (useless deprioritized)
		!w.is_dropdown(),             // 4. Non-dropdown over dropdown
		!w.is_disabled(),             // 5. Non-disabled over disabled
		!w.skip_and_not_fullscreen(), // 6. Non-skip-taskbar-pager over skip
		!w.is_dialog,                 // 7. Non-dialog over dialog (among dropdowns)
		!w.is_x11,                    // 8. XDG (non-X11) over X11
		w.transient_for.is_none(),    // 9. No transient parent over has parent (among dropdowns)
		w.map_sequence,               // 10. Later map_sequence for games (newer preferred)
		w.damage_sequence,            // 12. Most recent damage wins among games
	)
}

/// Returns `true` if the window is treated as its own "connector" under the
/// given strategy, in which case transient links are not followed for focus.
/// Gamescope: `win_treat_as_per_window()`.
pub(crate) fn win_treat_as_per_window(
	meta: &WindowMetadata,
	strategy: crate::session::compositor::VirtualConnectorStrategy,
) -> bool {
	use crate::session::compositor::VirtualConnectorStrategy;
	match strategy {
		VirtualConnectorStrategy::PerWindow => true,
		VirtualConnectorStrategy::PerAppId => meta.app_id == 0,
		VirtualConnectorStrategy::SingleApplication | VirtualConnectorStrategy::SteamControlled => false,
	}
}

/// Reserved bit marking a non-Steam window key. Gamescope:
/// `k_ulNonSteamWindowBit`.
const NON_STEAM_WINDOW_BIT: u64 = 1 << 63;
/// Reserved bit used by the Steam bootstrapper key. Gamescope: `k_ulReservedBit`.
const RESERVED_BIT: u64 = 1 << 62;

/// The virtual connector key for a window under the given strategy.
/// Gamescope: `steamcompmgr_win_t::GetVirtualConnectorKey()`.
pub(crate) fn virtual_connector_key(
	meta: &WindowMetadata,
	strategy: crate::session::compositor::VirtualConnectorStrategy,
) -> u64 {
	use crate::session::compositor::VirtualConnectorStrategy;
	match strategy {
		VirtualConnectorStrategy::SingleApplication | VirtualConnectorStrategy::SteamControlled => 0,
		VirtualConnectorStrategy::PerAppId => {
			if meta.steam_legacy_big_picture {
				1 | RESERVED_BIT
			} else if meta.app_id != 0 {
				meta.app_id as u64
			} else {
				NON_STEAM_WINDOW_BIT | meta.map_sequence
			}
		},
		VirtualConnectorStrategy::PerWindow => meta.map_sequence,
	}
}

/// Returns `true` if two windows belong to the same app (by app id or Steam
/// game id).
pub(crate) fn windows_share_app(a: &WindowMetadata, b: &WindowMetadata) -> bool {
	(a.app_id != 0 && a.app_id == b.app_id) || (a.steam_app_id != 0 && a.steam_app_id == b.steam_app_id)
}

/// Same-app override-redirect windows from another process are decorations
/// (e.g. a highlight overlay), not dropdowns.
pub(crate) fn is_same_app_override_decoration(candidate: &WindowMetadata, focus: &WindowMetadata) -> bool {
	if candidate.pid == focus.pid || !windows_share_app(candidate, focus) {
		return false;
	}
	// WS_EX_LAYERED | WS_EX_TRANSPARENT
	const WS_EX_LAYERED: u32 = 0x0008_0000;
	const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
	candidate.is_override_redirect()
		&& candidate.has_hwnd_style_ex
		&& (candidate.hwnd_style_ex & (WS_EX_LAYERED | WS_EX_TRANSPARENT)) == (WS_EX_LAYERED | WS_EX_TRANSPARENT)
}

/// Returns `true` if `override` is a valid override slot for `focus`.
pub(crate) fn is_good_override_candidate(override_meta: &WindowMetadata, focus: &WindowMetadata) -> bool {
	let rect = override_meta.geometry;
	if is_same_app_override_decoration(override_meta, focus) {
		return false;
	}
	if override_meta.pid != focus.pid && !windows_share_app(override_meta, focus) {
		return false;
	}
	override_meta.x11_window_id != focus.x11_window_id
		&& (rect.loc.x + rect.size.w) > 0
		&& (rect.loc.y + rect.size.h) > 0
}

/// Focus state for the compositor. Tracks whether focus needs reevaluation.
#[derive(Debug, Default)]
pub(crate) struct FocusState {
	/// Whether focus needs to be recalculated.
	dirty: bool,
	/// Wayland surface that requested activation via `xdg-activation`.
	requested_focus_surface: Option<WlSurface>,
}

impl FocusState {
	/// Mark focus as dirty — something focus-relevant has changed.
	/// Gamescope: `MakeFocusDirty()`
	pub fn mark_dirty(&mut self) {
		tracing::trace!(target: "focus", "FocusState marked dirty");
		self.dirty = true;
	}

	/// Mark focus as applied (clean) after focus has been set.
	pub fn apply(&mut self) {
		tracing::trace!(target: "focus", "FocusState applied (cleaned)");
		self.dirty = false;
	}

	/// Store an explicit focus request from an `xdg-activation` request.
	/// Replaces any previously pending surface request.
	pub fn set_requested_focus_surface(&mut self, surface: WlSurface) {
		tracing::debug!(target: "focus", surface_id = ?surface.id(), "xdg-activation: storing explicit focus request");
		self.requested_focus_surface = Some(surface);
		self.mark_dirty();
	}

	/// Peek at the pending `xdg-activation` focus request without consuming it.
	pub fn peek_requested_focus_surface(&self) -> Option<&WlSurface> {
		self.requested_focus_surface.as_ref()
	}

	/// Clear the pending `xdg-activation` focus request.
	pub fn clear_requested_focus_surface(&mut self) {
		self.requested_focus_surface = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_meta(fields: &[(&str, &str)]) -> WindowMetadata {
		let mut m = WindowMetadata::default();
		for &(key, val) in fields {
			match key {
				"app_id" => m.app_id = val.parse().unwrap(),
				"steam_app_id" => m.steam_app_id = val.parse().unwrap(),
				"steam_legacy_big_picture" => m.steam_legacy_big_picture = val.parse().unwrap(),
				"override_redirect" => m.override_redirect = val.parse().unwrap(),
				"maybe_a_dropdown" => m.maybe_a_dropdown = val.parse().unwrap(),
				"disabled" => {
					m.has_hwnd_style = true;
					m.hwnd_style = if val.parse().unwrap() { 0x8000_0000 } else { 0 };
				},
				"is_dialog" => m.is_dialog = val.parse().unwrap(),
				"is_x11" => m.is_x11 = val.parse().unwrap(),
				"skip_taskbar" => m.skip_taskbar = val.parse().unwrap(),
				"skip_pager" => m.skip_pager = val.parse().unwrap(),
				"fullscreen" => m.fullscreen = val.parse().unwrap(),
				"map_sequence" => m.map_sequence = val.parse().unwrap(),
				"damage_sequence" => m.damage_sequence = val.parse().unwrap(),
				"width" => m.geometry.size.w = val.parse().unwrap(),
				"height" => m.geometry.size.h = val.parse().unwrap(),
				_ => panic!("unknown field: {}", key),
			}
		}
		m
	}

	#[test]
	fn test_steam_big_picture_is_held_at_output_size_without_the_fullscreen_state() {
		assert!(make_meta(&[("app_id", "769")]).should_fill_output());
	}

	#[test]
	fn test_a_windowed_game_keeps_its_own_size_until_fullscreen() {
		assert!(!make_meta(&[("app_id", "12345")]).should_fill_output());
		assert!(make_meta(&[("app_id", "12345"), ("fullscreen", "true")]).should_fill_output());
	}

	#[test]
	fn test_dialog_and_dropdown_are_not_held_at_output_size() {
		assert!(
			!make_meta(&[
				("app_id", "12345"),
				("override_redirect", "true"),
				("width", "100"),
				("height", "100"),
			])
			.should_fill_output()
		);
		let mut m = make_meta(&[("app_id", "12345")]);
		m.transient_for = Some(12345);
		assert!(!m.should_fill_output());
	}

	// ---- Priority key tests (T2) ----

	#[test]
	fn test_game_wins_over_non_game() {
		let game = make_meta(&[("app_id", "12345")]);
		let non_game = make_meta(&[("app_id", "0")]);
		assert!(get_window_priority_key(&game) > get_window_priority_key(&non_game));
	}

	#[test]
	fn test_non_override_wins_over_override() {
		let normal = make_meta(&[("override_redirect", "false")]);
		let override_w = make_meta(&[("override_redirect", "true")]);
		assert!(get_window_priority_key(&normal) > get_window_priority_key(&override_w));
	}

	#[test]
	fn test_non_useless_wins_over_useless() {
		let normal = make_meta(&[("width", "100"), ("height", "100")]);
		let useless = make_meta(&[("width", "1"), ("height", "1")]);
		assert!(get_window_priority_key(&normal) > get_window_priority_key(&useless));
	}

	#[test]
	fn test_non_dropdown_wins_over_dropdown() {
		let normal = make_meta(&[("maybe_a_dropdown", "false")]);
		let dropdown = make_meta(&[
			("maybe_a_dropdown", "true"),
			("override_redirect", "true"),
			("width", "100"),
			("height", "100"),
		]);
		assert!(get_window_priority_key(&normal) > get_window_priority_key(&dropdown));
	}

	#[test]
	fn test_non_disabled_wins_over_disabled() {
		let normal = make_meta(&[("disabled", "false")]);
		let disabled = make_meta(&[("disabled", "true")]);
		assert!(get_window_priority_key(&normal) > get_window_priority_key(&disabled));
	}

	#[test]
	fn test_non_skip_wins_over_skip() {
		let normal = make_meta(&[("skip_taskbar", "false"), ("skip_pager", "false")]);
		let skip = make_meta(&[
			("skip_taskbar", "true"),
			("skip_pager", "true"),
			("fullscreen", "false"),
		]);
		assert!(get_window_priority_key(&normal) > get_window_priority_key(&skip));
	}

	#[test]
	fn test_identical_windows_equal() {
		let a = make_meta(&[("app_id", "123"), ("map_sequence", "5")]);
		let b = make_meta(&[("app_id", "123"), ("map_sequence", "5")]);
		assert_eq!(get_window_priority_key(&a), get_window_priority_key(&b));
	}

	#[test]
	fn test_transitivity() {
		// Build three windows where all have different priority levels.
		// A > B, B > C implies A > C.
		let a = make_meta(&[
			("app_id", "12345"),
			("override_redirect", "false"),
			("disabled", "false"),
			("width", "100"),
			("height", "100"),
		]);
		let b = make_meta(&[
			("app_id", "0"),
			("override_redirect", "false"),
			("disabled", "false"),
			("width", "100"),
			("height", "100"),
		]);
		let c = make_meta(&[
			("app_id", "0"),
			("override_redirect", "true"),
			("disabled", "false"),
			("width", "100"),
			("height", "100"),
		]);
		let ka = get_window_priority_key(&a);
		let kb = get_window_priority_key(&b);
		let kc = get_window_priority_key(&c);
		assert!(ka > kb, "A > B");
		assert!(kb > kc, "B > C");
		assert!(ka > kc, "A > C (transitivity)");
	}

	#[test]
	fn test_transitivity_chain_of_10() {
		// Generate 10 windows with monotonically decreasing priority and
		// verify all pairwise comparisons are transitive.
		let windows: Vec<WindowMetadata> = (0..10)
			.map(|i| WindowMetadata {
				map_sequence: 10 - i,
				app_id: 12345,
				..Default::default()
			})
			.collect();
		let keys: Vec<_> = windows.iter().map(get_window_priority_key).collect();

		// Verify strict total ordering: for all i < j, keys[i] >= keys[j]
		for i in 0..keys.len() {
			for j in (i + 1)..keys.len() {
				assert!(
					keys[i] >= keys[j],
					"keys[{}] {:?} should be >= keys[{}] {:?}",
					i,
					keys[i],
					j,
					keys[j]
				);
			}
		}
		// Verify transitivity: for all i < j < k, keys[i] >= keys[k]
		for i in 0..keys.len() {
			for j in (i + 1)..keys.len() {
				for k in (j + 1)..keys.len() {
					if keys[i] >= keys[j] && keys[j] >= keys[k] {
						assert!(keys[i] >= keys[k], "transitivity violated: i={} j={} k={}", i, j, k);
					}
				}
			}
		}
	}

	#[test]
	fn test_antisymmetry() {
		// For any two windows: if key(a) > key(b), then key(b) < key(a).
		let a = make_meta(&[("app_id", "12345")]);
		let b = make_meta(&[("app_id", "0")]);
		let ka = get_window_priority_key(&a);
		let kb = get_window_priority_key(&b);
		if ka > kb {
			assert!(kb < ka);
		}
		if kb > ka {
			assert!(ka < kb);
		}
		if ka == kb {
			assert!(kb == ka);
		}
	}

	#[test]
	fn test_map_sequence_tiebreaker_for_games() {
		let newer = make_meta(&[("app_id", "12345"), ("map_sequence", "10")]);
		let older = make_meta(&[("app_id", "12345"), ("map_sequence", "5")]);
		assert!(get_window_priority_key(&newer) > get_window_priority_key(&older));
	}

	#[test]
	fn test_damage_sequence_tiebreaker_for_games() {
		let recent = make_meta(&[("app_id", "12345"), ("damage_sequence", "100")]);
		let stale = make_meta(&[("app_id", "12345"), ("damage_sequence", "50")]);
		assert!(get_window_priority_key(&recent) > get_window_priority_key(&stale));
	}

	#[test]
	fn test_xdg_wins_over_x11() {
		let xdg = make_meta(&[("is_x11", "false")]);
		let x11 = make_meta(&[("is_x11", "true")]);
		assert!(get_window_priority_key(&xdg) > get_window_priority_key(&x11));
	}

	#[test]
	fn test_non_dialog_wins_over_dialog_among_dropdowns() {
		// Both are dropdowns (via override_redirect + non-1x1).
		let non_dialog = make_meta(&[
			("override_redirect", "true"),
			("is_dialog", "false"),
			("width", "100"),
			("height", "100"),
		]);
		let dialog = make_meta(&[
			("override_redirect", "true"),
			("is_dialog", "true"),
			("width", "100"),
			("height", "100"),
		]);
		assert!(get_window_priority_key(&non_dialog) > get_window_priority_key(&dialog));
	}

	#[test]
	fn test_windows_share_app_by_steam_game_id() {
		let a = make_meta(&[("app_id", "0"), ("steam_app_id", "12345")]);
		let b = make_meta(&[("app_id", "0"), ("steam_app_id", "12345")]);
		let c = make_meta(&[("app_id", "0"), ("steam_app_id", "999")]);
		assert!(windows_share_app(&a, &b));
		assert!(!windows_share_app(&a, &c));
	}

	#[test]
	fn test_legacy_big_picture_is_steam() {
		assert!(make_meta(&[("app_id", "0"), ("steam_legacy_big_picture", "true")]).is_steam());
		assert!(make_meta(&[("app_id", "769")]).is_steam());
		assert!(!make_meta(&[("app_id", "12345")]).is_steam());
	}

	#[test]
	fn test_one_by_one_override_is_not_override_redirect() {
		let one_by_one = make_meta(&[("override_redirect", "true"), ("width", "1"), ("height", "1")]);
		assert!(!one_by_one.is_override_redirect());
		let normal = make_meta(&[("override_redirect", "true"), ("width", "100"), ("height", "100")]);
		assert!(normal.is_override_redirect());
	}

	#[test]
	fn test_disabled_requires_the_wine_style_property() {
		// `hwnd_style` set without `has_hwnd_style` must not count as disabled.
		let m = WindowMetadata {
			hwnd_style: 0x8000_0000,
			..Default::default()
		};
		assert!(!m.is_disabled());
	}

	#[test]
	fn test_per_window_strategy_treats_every_window_as_its_own_connector() {
		use crate::session::compositor::VirtualConnectorStrategy;
		let m = make_meta(&[("app_id", "12345"), ("map_sequence", "7")]);
		assert!(win_treat_as_per_window(&m, VirtualConnectorStrategy::PerWindow));
		assert!(!win_treat_as_per_window(&m, VirtualConnectorStrategy::SteamControlled));
		assert!(!win_treat_as_per_window(
			&m,
			VirtualConnectorStrategy::SingleApplication
		));
		assert_eq!(virtual_connector_key(&m, VirtualConnectorStrategy::PerWindow), 7);
		assert_eq!(virtual_connector_key(&m, VirtualConnectorStrategy::SteamControlled), 0);
		assert_eq!(virtual_connector_key(&m, VirtualConnectorStrategy::PerAppId), 12345);
	}
}
