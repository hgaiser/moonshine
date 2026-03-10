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

use super::state::MoonshineCompositor;

/// Input events sent from the control stream to the compositor.
///
/// These are transport-level events that cross the tokio→calloop boundary.
/// They carry only primitive data (keycodes, coordinates, button codes) so
/// they can be `Send` without any Wayland object references.
#[derive(Debug)]
pub enum CompositorInputEvent {
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
pub fn process_input(event: CompositorInputEvent, state: &mut MoonshineCompositor) {
	let serial = SERIAL_COUNTER.next_serial();
	let time = state.clock.now().as_millis();

	// specific pointer events (non-keyboard) should reset the cursor inactivity timer
	match event {
		CompositorInputEvent::KeyDown { .. } | CompositorInputEvent::KeyUp { .. } => {},
		_ => state.last_pointer_activity = std::time::Instant::now(),
	}

	// One-time X11 focus reset when HDR/gamescope WSI mode is active.
	if state.override_surface.is_some() && state.x11_focus_needs_reset {
		state.sync_x11_focus();
	}

	match event {
		CompositorInputEvent::KeyDown { keycode } => {
			tracing::trace!("Key down: {keycode}");

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
			tracing::trace!("Key up: {keycode}");

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
			tracing::trace!("Mouse absolute: ({x}, {y}) screen: ({screen_width}x{screen_height})");
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
			let pointer = state.seat.get_pointer().unwrap();
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
			tracing::trace!("Mouse relative: ({dx}, {dy})");

			let delta = Point::from((dx as f64, dy as f64));
			let pointer = state.seat.get_pointer().unwrap();

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
			tracing::trace!("Mouse button down: {button:#x}");

			let pointer = state.seat.get_pointer().unwrap();
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
			tracing::trace!("Mouse button up: {button:#x}");

			let pointer = state.seat.get_pointer().unwrap();
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
			tracing::trace!("Scroll vertical: {amount}");

			let pointer = state.seat.get_pointer().unwrap();
			pointer.axis(state, AxisFrame::new(time).value(Axis::Vertical, -(amount as f64)));
			pointer.frame(state);
		},
		CompositorInputEvent::ScrollHorizontal { amount } => {
			tracing::trace!("Scroll horizontal: {amount}");

			let pointer = state.seat.get_pointer().unwrap();
			pointer.axis(state, AxisFrame::new(time).value(Axis::Horizontal, amount as f64));
			pointer.frame(state);
		},
	}
}

/// Clamp the cursor position to the output bounds.
fn clamp_cursor(state: &mut MoonshineCompositor) {
	let output_size = state
		.output
		.current_mode()
		.map(|m| m.size)
		.unwrap_or((state.width as i32, state.height as i32).into());
	state.cursor_position.x = state.cursor_position.x.clamp(0.0, (output_size.w - 1) as f64);
	state.cursor_position.y = state.cursor_position.y.clamp(0.0, (output_size.h - 1) as f64);
}

/// Find the Wayland surface under the current cursor position.
///
/// In gamescope mode (override_surface is set), always routes to the focused
/// game window to match gamescope's input behavior. This prevents stray
/// override-redirect windows (Steam popups, etc.) from intercepting pointer
/// events.
fn find_surface_under(
	state: &MoonshineCompositor,
) -> Option<(
	<MoonshineCompositor as smithay::input::SeatHandler>::PointerFocus,
	Point<f64, Logical>,
)> {
	if state.override_surface.is_some() {
		let wid = state.focused_x11_window?;
		for window in state.space.elements() {
			if let Some(x11) = window.x11_surface() {
				if x11.window_id() == wid {
					let window_loc = state.space.element_geometry(window)?.loc;
					let pos_within_window = state.cursor_position - window_loc.to_f64();
					let (surface, surface_offset) = window.surface_under(pos_within_window, WindowSurfaceType::ALL)?;
					return Some((surface, surface_offset.to_f64() + window_loc.to_f64()));
				}
			}
		}
		return None;
	}

	let (window, window_loc) = state.space.element_under(state.cursor_position)?;
	let pos_within_window = state.cursor_position - window_loc.to_f64();
	let (surface, surface_offset) = window.surface_under(pos_within_window, WindowSurfaceType::ALL)?;
	Some((surface, surface_offset.to_f64() + window_loc.to_f64()))
}
