//! Keyboard focus target types for the compositor.
//!
//! Smithay's `SeatHandler::KeyboardFocus` type determines how keyboard focus
//! is dispatched. For X11 windows (via XWayland), focus must go through the
//! `X11Surface` implementation of `KeyboardTarget` — this calls
//! `XSetInputFocus` which is required for X11 clients to receive key events.
//!
//! Using a bare `WlSurface` as the focus target bypasses `XSetInputFocus`,
//! causing X11 games to never receive keyboard input.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::{Window, WindowSurface};
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;

use super::state::MoonshineCompositor;

/// Focus target for keyboard input.
///
/// Wraps a `Window` and dispatches keyboard events to the correct
/// underlying surface type (Wayland toplevel or X11 surface).
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
	Window(Window),
}

impl IsAlive for KeyboardFocusTarget {
	#[inline]
	fn alive(&self) -> bool {
		match self {
			KeyboardFocusTarget::Window(w) => w.alive(),
		}
	}
}

impl WaylandFocus for KeyboardFocusTarget {
	#[inline]
	fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
		match self {
			KeyboardFocusTarget::Window(w) => w.wl_surface(),
		}
	}
}

impl From<Window> for KeyboardFocusTarget {
	#[inline]
	fn from(w: Window) -> Self {
		KeyboardFocusTarget::Window(w)
	}
}

impl KeyboardTarget<MoonshineCompositor> for KeyboardFocusTarget {
	fn enter(
		&self,
		seat: &Seat<MoonshineCompositor>,
		data: &mut MoonshineCompositor,
		keys: Vec<KeysymHandle<'_>>,
		serial: Serial,
	) {
		match self {
			KeyboardFocusTarget::Window(w) => match w.underlying_surface() {
				WindowSurface::Wayland(w) => KeyboardTarget::enter(w.wl_surface(), seat, data, keys, serial),
				WindowSurface::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
			},
		}
	}

	fn leave(&self, seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, serial: Serial) {
		match self {
			KeyboardFocusTarget::Window(w) => match w.underlying_surface() {
				WindowSurface::Wayland(w) => KeyboardTarget::leave(w.wl_surface(), seat, data, serial),
				WindowSurface::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
			},
		}
	}

	fn key(
		&self,
		seat: &Seat<MoonshineCompositor>,
		data: &mut MoonshineCompositor,
		key: KeysymHandle<'_>,
		state: KeyState,
		serial: Serial,
		time: u32,
	) {
		match self {
			KeyboardFocusTarget::Window(w) => match w.underlying_surface() {
				WindowSurface::Wayland(w) => KeyboardTarget::key(w.wl_surface(), seat, data, key, state, serial, time),
				WindowSurface::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
			},
		}
	}

	fn modifiers(
		&self,
		seat: &Seat<MoonshineCompositor>,
		data: &mut MoonshineCompositor,
		modifiers: ModifiersState,
		serial: Serial,
	) {
		match self {
			KeyboardFocusTarget::Window(w) => match w.underlying_surface() {
				WindowSurface::Wayland(w) => KeyboardTarget::modifiers(w.wl_surface(), seat, data, modifiers, serial),
				WindowSurface::X11(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
			},
		}
	}
}
