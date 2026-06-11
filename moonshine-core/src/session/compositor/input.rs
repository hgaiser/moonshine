//! Input event processing for the compositor.
//!
//! Input events from the Moonlight control stream are sent to the compositor
//! via a `calloop::channel`. The compositor injects them directly into the
//! Smithay `Seat` — no libei or EIS socket needed.

use smithay::backend::input::{Axis, ButtonState, KeyState};
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint};

use crate::session::compositor::state::MoonshineCompositor;

/// Input events sent from the control stream to the compositor.
///
/// These are transport-level events that cross the tokio→calloop boundary.
/// They carry only primitive data (keycodes, coordinates, button codes) so
/// they can be `Send` without any Wayland object references.
#[derive(Debug)]
pub(crate) enum CompositorInputEvent {
	/// A key was pressed. `keycode` is a Linux evdev keycode.
	KeyDown { keycode: u32 },
	/// A key was released. `keycode` is a Linux evdev keycode.
	KeyUp { keycode: u32 },
	/// Absolute pointer motion. Coordinates are in pixels relative to the
	/// Moonlight client's screen dimensions.
	MouseMoveAbsolute {
		x: i16,
		y: i16,
		screen_width: i16,
		screen_height: i16,
	},
	/// Relative pointer motion in pixels.
	MouseMoveRelative { dx: i16, dy: i16 },
	/// A mouse button was pressed. `button` is a Linux button code (e.g. BTN_LEFT = 0x110).
	MouseButtonDown { button: u32 },
	/// A mouse button was released.
	MouseButtonUp { button: u32 },
	/// Vertical scroll. Positive = scroll up, negative = scroll down.
	ScrollVertical { amount: i16 },
	/// Horizontal scroll.
	ScrollHorizontal { amount: i16 },
}

/// Process an input event received from the Moonlight control stream.
///
/// Called on the compositor's calloop thread when the channel source fires.
/// Events are injected directly into the Smithay Seat — no libei needed
/// since we *are* the compositor.
pub(crate) fn process_input(event: CompositorInputEvent, state: &mut MoonshineCompositor) {
	let serial = SERIAL_COUNTER.next_serial();
	let time = state.clock.now().as_millis();

	// specific pointer events (non-keyboard) should reset the cursor inactivity timer
	match event {
		CompositorInputEvent::KeyDown { .. } | CompositorInputEvent::KeyUp { .. } => {},
		_ => state.last_pointer_activity = Some(std::time::Instant::now()),
	}

	match event {
		CompositorInputEvent::KeyDown { keycode } => {
			tracing::trace!(target: "input", "Key down: {keycode}");

			if let Some(keyboard) = state.seat.get_keyboard() {
				keyboard.input::<(), _>(
					state,
					Keycode::from(keycode + 8),
					KeyState::Pressed,
					serial,
					time,
					|_, _, _| FilterResult::Forward,
				);
			}
		},
		CompositorInputEvent::KeyUp { keycode } => {
			tracing::trace!(target: "input", "Key up: {keycode}");

			if let Some(keyboard) = state.seat.get_keyboard() {
				keyboard.input::<(), _>(
					state,
					Keycode::from(keycode + 8),
					KeyState::Released,
					serial,
					time,
					|_, _, _| FilterResult::Forward,
				);
			}
		},
		CompositorInputEvent::MouseMoveAbsolute {
			x,
			y,
			screen_width,
			screen_height,
		} => {
			tracing::trace!(target: "input", "Mouse absolute: ({x}, {y}) screen: ({screen_width}x{screen_height})");
			let output_size = state
				.output
				.current_mode()
				.map(|m| m.size)
				.unwrap_or((state.width as i32, state.height as i32).into());

			let new_x = if screen_width > 0 {
				x as f64 / screen_width as f64 * output_size.w as f64
			} else {
				x as f64
			};
			let new_y = if screen_height > 0 {
				y as f64 / screen_height as f64 * output_size.h as f64
			} else {
				y as f64
			};

			state.cursor_position = Point::from((new_x, new_y));
			clamp_cursor(state);

			let under = find_surface_under(state);
			let pointer = state.seat.get_pointer().expect("pointer should exist");
			pointer.motion(
				state,
				under,
				&MotionEvent {
					location: state.cursor_position,
					serial,
					time,
				},
			);
			pointer.frame(state);
		},
		CompositorInputEvent::MouseMoveRelative { dx, dy } => {
			tracing::trace!(target: "input", "Mouse relative: ({dx}, {dy})");

			let delta = Point::from((dx as f64, dy as f64));
			let pointer = state.seat.get_pointer().expect("pointer should exist");

			// Check for pointer constraints (lock/confine).
			let mut pointer_locked = false;
			let under = find_surface_under(state);

			if let Some((ref surface, ref _surface_loc)) = under {
				with_pointer_constraint(surface, &pointer, |constraint| match constraint {
					Some(constraint) if constraint.is_active() => match &*constraint {
						PointerConstraint::Locked(_) => {
							pointer_locked = true;
						},
						PointerConstraint::Confined(_) => {},
					},
					_ => {},
				});
			}

			pointer.relative_motion(
				state,
				under.clone(),
				&RelativeMotionEvent {
					delta,
					delta_unaccel: delta,
					utime: time as u64,
				},
			);

			state.cursor_position += delta;
			clamp_cursor(state);

			if pointer_locked {
				pointer.frame(state);
				return;
			}

			pointer.motion(
				state,
				under,
				&MotionEvent {
					location: state.cursor_position,
					serial,
					time,
				},
			);
			pointer.frame(state);
		},
		CompositorInputEvent::MouseButtonDown { button } => {
			tracing::trace!(target: "input", "Mouse button down: {button:#x}");

			let pointer = state.seat.get_pointer().expect("pointer should exist");
			pointer.button(
				state,
				&ButtonEvent {
					serial,
					time,
					button,
					state: ButtonState::Pressed,
				},
			);
			pointer.frame(state);
		},
		CompositorInputEvent::MouseButtonUp { button } => {
			tracing::trace!(target: "input", "Mouse button up: {button:#x}");

			let pointer = state.seat.get_pointer().expect("pointer should exist");
			pointer.button(
				state,
				&ButtonEvent {
					serial,
					time,
					button,
					state: ButtonState::Released,
				},
			);
			pointer.frame(state);
		},
		CompositorInputEvent::ScrollVertical { amount } => {
			tracing::trace!(target: "input", "Scroll vertical: {amount}");

			let pointer = state.seat.get_pointer().expect("pointer should exist");
			pointer.axis(
				state,
				AxisFrame::new(time)
					.value(Axis::Vertical, -(amount as f64) / 120.0 * 15.0)
					.v120(Axis::Vertical, -amount as i32),
			);
			pointer.frame(state);
		},
		CompositorInputEvent::ScrollHorizontal { amount } => {
			tracing::trace!(target: "input", "Scroll horizontal: {amount}");

			let pointer = state.seat.get_pointer().expect("pointer should exist");
			pointer.axis(
				state,
				AxisFrame::new(time)
					.value(Axis::Horizontal, amount as f64 / 120.0 * 15.0)
					.v120(Axis::Horizontal, amount as i32),
			);
			pointer.frame(state);
		},
	}
}

/// Clamp the cursor position to the output bounds, expanded to include
/// the override window's geometry if one is active.
///
/// Gamescope: expands cursor bounds when a dropdown/override window is active
/// so the cursor can move within the dropdown area without being clamped
/// to the output bounds.
fn clamp_cursor(state: &mut MoonshineCompositor) {
	let output_size = state
		.output
		.current_mode()
		.map(|m| m.size)
		.unwrap_or((state.width as i32, state.height as i32).into());

	// Start with output bounds.
	let mut min_x: f64 = 0.0;
	let mut min_y: f64 = 0.0;
	let mut max_x: f64 = (output_size.w - 1) as f64;
	let mut max_y: f64 = (output_size.h - 1) as f64;

	// Expand bounds to include the override window's geometry if active.
	// This allows the cursor to move within the dropdown area.
	if let Some(ref override_win) = state.override_window {
		if let Some(geo) = state.space.element_geometry(override_win) {
			let override_min_x = geo.loc.x as f64;
			let override_min_y = geo.loc.y as f64;
			let override_max_x = (geo.loc.x + geo.size.w - 1) as f64;
			let override_max_y = (geo.loc.y + geo.size.h - 1) as f64;
			min_x = min_x.min(override_min_x);
			min_y = min_y.min(override_min_y);
			max_x = max_x.max(override_max_x);
			max_y = max_y.max(override_max_y);
		}
	}

	state.cursor_position.x = state.cursor_position.x.clamp(min_x, max_x);
	state.cursor_position.y = state.cursor_position.y.clamp(min_y, max_y);
}

/// Find the Wayland surface under the current cursor position.
///
/// Priority order:
/// 1. If override_window (dropdown) is active AND override_surface is NOT
///    active, route to the dropdown.  When the WSI bypass is active the
///    renderer replaces the entire space with the bypass surface, so
///    dropdown windows are not rendered — routing to them would make them
///    receive clicks while being invisible.
/// 2. If override_surface is active (WSI bypass), route to the focused game window.
/// 3. Otherwise, find the surface under the cursor normally.
fn find_surface_under(
	state: &MoonshineCompositor,
) -> Option<(
	<MoonshineCompositor as smithay::input::SeatHandler>::PointerFocus,
	Point<f64, Logical>,
)> {
	// Priority 1: Override window (dropdown) is active — route to it.
	// Only do this when the WSI bypass surface is NOT active.  When
	// override_surface is active the renderer replaces the entire space
	// with the bypass surface, so dropdown windows are not rendered —
	// routing to them would make invisible windows intercept clicks.
	if state.override_window.is_some() && !state.is_override_active() {
		if let Some(ref override_win) = state.override_window {
			let override_loc = state.space.element_geometry(override_win)?.loc;
			let pos_within_override = state.cursor_position - override_loc.to_f64();
			if let Some((surface, surface_offset)) =
				override_win.surface_under(pos_within_override, WindowSurfaceType::ALL)
			{
				return Some((surface, surface_offset.to_f64() + override_loc.to_f64()));
			}
		}
	}

	// Priority 2: WSI override surface active — route to the focused game window.
	if state.is_override_active() {
		if let Some(wid) = state.focused_x11_window {
			// XWayland path: find the focused X11 window and route events there.
			for window in state.space.elements() {
				if let Some(x11) = window.x11_surface() {
					if x11.window_id() == wid {
						let window_loc = state.space.element_geometry(window)?.loc;
						let pos_within_window = state.cursor_position - window_loc.to_f64();
						// Try finding a sub-surface under the cursor first.
						if let Some((surface, surface_offset)) =
							window.surface_under(pos_within_window, WindowSurfaceType::ALL)
						{
							return Some((surface, surface_offset.to_f64() + window_loc.to_f64()));
						}
						// If the X11 window has no buffer (ICD renders to the
						// bypass surface), use the toplevel wl_surface directly
						// so pointer events still reach XWayland.
						if let Some(wl_surface) = x11.wl_surface() {
							return Some((wl_surface, window_loc.to_f64()));
						}
						return None;
					}
				}
			}
			return None;
		} else {
			// Native Wayland path (x11_win == 0): the override surface is a
			// fullscreen bypass surface at the output origin.  Route pointer
			// events directly to it so the application receives input.
			if let Some((ref override_surface, _)) = state.override_surface {
				return Some((override_surface.clone(), Point::from((0.0, 0.0))));
			}
		}
	}

	// Priority 3: Normal cursor-based surface finding.
	let (window, window_loc) = state.space.element_under(state.cursor_position)?;
	let pos_within_window = state.cursor_position - window_loc.to_f64();
	let (surface, surface_offset) = window.surface_under(pos_within_window, WindowSurfaceType::ALL)?;
	Some((surface, surface_offset.to_f64() + window_loc.to_f64()))
}
