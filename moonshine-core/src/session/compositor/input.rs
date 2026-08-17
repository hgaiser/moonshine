//! Input event processing for the compositor.
//!
//! Input events from the Moonlight control stream are sent to the compositor
//! via a `calloop::channel`. The compositor injects them directly into the
//! Smithay `Seat` — no libei or EIS socket needed.

use smithay::backend::input::{
	Axis, ButtonState, KeyState, TabletToolCapabilities, TabletToolDescriptor, TabletToolType, TouchSlot,
};
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::input::touch::{DownEvent as TouchDownEvent, MotionEvent as TouchMotionEvent, UpEvent as TouchUpEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};
use smithay::wayland::tablet_manager::{TabletSeatTrait, TabletToolHandle};

use crate::session::compositor::state::MoonshineCompositor;

/// Input events sent from the control stream to the compositor.
///
/// These are transport-level events that cross the tokio→calloop boundary.
/// They carry only primitive data (keycodes, coordinates, button codes) so
/// they can be `Send` without any Wayland object references.
#[derive(Debug)]
pub(crate) enum CompositorInputEvent {
	/// A key was pressed. `keycode` is a Linux evdev keycode.
	KeyDown {
		keycode: u32,
	},
	/// A key was released. `keycode` is a Linux evdev keycode.
	KeyUp {
		keycode: u32,
	},
	/// Absolute pointer motion. Coordinates are in pixels relative to the
	/// Moonlight client's screen dimensions.
	MouseMoveAbsolute {
		x: i16,
		y: i16,
		screen_width: i16,
		screen_height: i16,
	},
	/// Relative pointer motion in pixels.
	MouseMoveRelative {
		dx: i16,
		dy: i16,
	},
	/// A mouse button was pressed. `button` is a Linux button code (e.g. BTN_LEFT = 0x110).
	MouseButtonDown {
		button: u32,
	},
	/// A mouse button was released.
	MouseButtonUp {
		button: u32,
	},
	/// Vertical scroll. Positive = scroll up, negative = scroll down.
	ScrollVertical {
		amount: i16,
	},
	/// Horizontal scroll.
	ScrollHorizontal {
		amount: i16,
	},
	/// Native touchscreen contact in normalized video coordinates.
	TouchDown {
		slot: u32,
		x: f32,
		y: f32,
	},
	TouchMove {
		slot: u32,
		x: f32,
		y: f32,
	},
	TouchUp {
		slot: u32,
	},
	TouchCancelAll,
	/// Native pen input in normalized video coordinates.
	Pen {
		event_kind: u8,
		tool_kind: u8,
		buttons: u8,
		x: f32,
		y: f32,
		pressure_or_distance: f32,
		rotation: u16,
		tilt: u8,
	},
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
		CompositorInputEvent::KeyDown { .. }
		| CompositorInputEvent::KeyUp { .. }
		| CompositorInputEvent::TouchDown { .. }
		| CompositorInputEvent::TouchMove { .. }
		| CompositorInputEvent::TouchUp { .. }
		| CompositorInputEvent::TouchCancelAll => {},
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
		CompositorInputEvent::TouchDown { slot, x, y } => {
			let location = normalized_pointer_location(state, x, y);
			let under = find_surface_at(state, location);
			if let Some(touch) = state.seat.get_touch() {
				touch.down(
					state,
					under,
					&TouchDownEvent {
						slot: TouchSlot::from(Some(slot)),
						location,
						serial,
						time,
					},
				);
				touch.frame(state);
			}
		},
		CompositorInputEvent::TouchMove { slot, x, y } => {
			let location = normalized_pointer_location(state, x, y);
			let under = find_surface_at(state, location);
			if let Some(touch) = state.seat.get_touch() {
				touch.motion(
					state,
					under,
					&TouchMotionEvent {
						slot: TouchSlot::from(Some(slot)),
						location,
						time,
					},
				);
				touch.frame(state);
			}
		},
		CompositorInputEvent::TouchUp { slot } => {
			if let Some(touch) = state.seat.get_touch() {
				touch.up(
					state,
					&TouchUpEvent {
						slot: TouchSlot::from(Some(slot)),
						serial,
						time,
					},
				);
				touch.frame(state);
			}
		},
		CompositorInputEvent::TouchCancelAll => {
			if let Some(touch) = state.seat.get_touch() {
				touch.cancel(state);
			}
		},
		CompositorInputEvent::Pen {
			event_kind,
			tool_kind,
			buttons,
			x,
			y,
			pressure_or_distance,
			rotation,
			tilt,
		} => process_pen_input(
			state,
			PenInput {
				event_kind,
				tool_kind,
				buttons,
				x,
				y,
				pressure_or_distance,
				rotation,
				tilt,
			},
			serial,
			time,
		),
	}
}

#[derive(Debug, Clone, Copy)]
struct PenInput {
	event_kind: u8,
	tool_kind: u8,
	buttons: u8,
	x: f32,
	y: f32,
	pressure_or_distance: f32,
	rotation: u16,
	tilt: u8,
}

const POINTER_EVENT_HOVER: u8 = 0x00;
const POINTER_EVENT_DOWN: u8 = 0x01;
const POINTER_EVENT_UP: u8 = 0x02;
const POINTER_EVENT_MOVE: u8 = 0x03;
const POINTER_EVENT_CANCEL: u8 = 0x04;
const POINTER_EVENT_BUTTON_ONLY: u8 = 0x05;
const POINTER_EVENT_HOVER_LEAVE: u8 = 0x06;
const POINTER_EVENT_CANCEL_ALL: u8 = 0x07;

const PEN_TOOL_ERASER: u8 = 0x02;
const ROTATION_UNKNOWN: u16 = 0xFFFF;
const TILT_UNKNOWN: u8 = 0xFF;

fn pen_tool_descriptor(tool_kind: u8) -> TabletToolDescriptor {
	let is_eraser = tool_kind == PEN_TOOL_ERASER;
	TabletToolDescriptor {
		tool_type: if is_eraser {
			TabletToolType::Eraser
		} else {
			TabletToolType::Pen
		},
		hardware_serial: u64::from(tool_kind),
		hardware_id_wacom: 0,
		capabilities: TabletToolCapabilities::PRESSURE
			| TabletToolCapabilities::DISTANCE
			| TabletToolCapabilities::TILT,
	}
}

fn get_pen_tool(state: &mut MoonshineCompositor, tool_kind: u8) -> TabletToolHandle {
	let tablet_seat = state.seat.tablet_seat();
	let descriptor = pen_tool_descriptor(tool_kind);
	if let Some(tool) = tablet_seat.get_tool(&descriptor) {
		return tool;
	}

	let display_handle = state.display_handle.clone();
	tablet_seat.add_tool::<MoonshineCompositor>(state, &display_handle, &descriptor)
}

fn release_pen(state: &mut MoonshineCompositor, time: u32) {
	let Some(tool_kind) = state.active_pen_tool_kind.take() else {
		state.pen_buttons = 0;
		return;
	};

	if let Some(tool) = state.seat.tablet_seat().get_tool(&pen_tool_descriptor(tool_kind)) {
		tool.tip_up(time);
		sync_pen_buttons(&tool, state.pen_buttons, 0, time);
		tool.proximity_out(time);
	}
	state.pen_buttons = 0;
}

fn sync_pen_buttons(tool: &TabletToolHandle, old_buttons: u8, new_buttons: u8, time: u32) {
	const BUTTONS: [(u8, u32); 3] = [
		(0x01, 0x14B), // BTN_STYLUS
		(0x02, 0x14C), // BTN_STYLUS2
		(0x04, 0x149), // BTN_STYLUS3
	];

	for (mask, button) in BUTTONS {
		if old_buttons & mask != new_buttons & mask {
			let state = if new_buttons & mask != 0 {
				ButtonState::Pressed
			} else {
				ButtonState::Released
			};
			tool.button(button, state, SERIAL_COUNTER.next_serial(), time);
		}
	}
}

fn process_pen_input(state: &mut MoonshineCompositor, event: PenInput, serial: smithay::utils::Serial, time: u32) {
	if matches!(
		event.event_kind,
		POINTER_EVENT_CANCEL | POINTER_EVENT_HOVER_LEAVE | POINTER_EVENT_CANCEL_ALL
	) {
		release_pen(state, time);
		return;
	}

	if event.event_kind == POINTER_EVENT_BUTTON_ONLY {
		if let Some(tool_kind) = state.active_pen_tool_kind {
			let tool = get_pen_tool(state, tool_kind);
			sync_pen_buttons(&tool, state.pen_buttons, event.buttons, time);
			state.pen_buttons = event.buttons;
		}
		return;
	}

	if state
		.active_pen_tool_kind
		.is_some_and(|active| active != event.tool_kind)
	{
		release_pen(state, time);
	}

	let tool = get_pen_tool(state, event.tool_kind);
	let tablet_seat = state.seat.tablet_seat();
	let Some(tablet) = tablet_seat.get_tablet(&state.pen_tablet_descriptor) else {
		return;
	};
	let location = normalized_pointer_location(state, event.x, event.y);
	let focus = find_surface_at(state, location);
	state.cursor_position = location;

	if state.active_pen_tool_kind.is_none() {
		let Some(focus) = focus.clone() else {
			return;
		};
		tool.proximity_in(location, focus, &tablet, serial, time);
		state.active_pen_tool_kind = Some(event.tool_kind);
	}

	let value = event.pressure_or_distance.clamp(0.0, 1.0) as f64;
	if event.event_kind == POINTER_EVENT_HOVER {
		tool.distance(value);
	} else {
		tool.pressure(value);
	}

	if event.rotation != ROTATION_UNKNOWN && event.tilt != TILT_UNKNOWN {
		let angle = (event.rotation as f64).to_radians();
		let magnitude = f64::from(event.tilt.min(90));
		tool.tilt((magnitude * angle.sin(), -magnitude * angle.cos()));
	}

	tool.motion(location, focus, &tablet, serial, time);
	sync_pen_buttons(&tool, state.pen_buttons, event.buttons, time);
	state.pen_buttons = event.buttons;

	match event.event_kind {
		POINTER_EVENT_DOWN | POINTER_EVENT_MOVE => {
			tool.tip_down(SERIAL_COUNTER.next_serial(), time);
		},
		POINTER_EVENT_UP => {
			tool.tip_up(time);
		},
		_ => {},
	}
}

fn normalized_pointer_location(state: &MoonshineCompositor, x: f32, y: f32) -> Point<f64, Logical> {
	let output_size = state
		.output
		.current_mode()
		.map(|mode| mode.size)
		.unwrap_or((state.width as i32, state.height as i32).into());
	let max_x = output_size.w.saturating_sub(1).max(0) as f64;
	let max_y = output_size.h.saturating_sub(1).max(0) as f64;
	Point::from((x.clamp(0.0, 1.0) as f64 * max_x, y.clamp(0.0, 1.0) as f64 * max_y))
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
	if let Some(ref override_win) = state.override_window
		&& let Some(geo) = state.space.element_geometry(override_win)
	{
		let override_min_x = geo.loc.x as f64;
		let override_min_y = geo.loc.y as f64;
		let override_max_x = (geo.loc.x + geo.size.w - 1) as f64;
		let override_max_y = (geo.loc.y + geo.size.h - 1) as f64;
		min_x = min_x.min(override_min_x);
		min_y = min_y.min(override_min_y);
		max_x = max_x.max(override_max_x);
		max_y = max_y.max(override_max_y);
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
	find_surface_at(state, state.cursor_position)
}

fn find_surface_at(
	state: &MoonshineCompositor,
	position: Point<f64, Logical>,
) -> Option<(
	<MoonshineCompositor as smithay::input::SeatHandler>::PointerFocus,
	Point<f64, Logical>,
)> {
	// Priority 1: Override window (dropdown) is active — route to it.
	// Only do this when the WSI bypass surface is NOT active.  When
	// override_surface is active the renderer replaces the entire space
	// with the bypass surface, so dropdown windows are not rendered —
	// routing to them would make invisible windows intercept clicks.
	if state.override_window.is_some()
		&& !state.is_override_active()
		&& let Some(ref override_win) = state.override_window
	{
		let override_loc = state.space.element_geometry(override_win)?.loc;
		let pos_within_override = position - override_loc.to_f64();
		if let Some((surface, surface_offset)) = override_win.surface_under(pos_within_override, WindowSurfaceType::ALL)
		{
			return Some((surface, surface_offset.to_f64() + override_loc.to_f64()));
		}
	}

	// Priority 2: WSI override surface active — route to the focused game window.
	if state.is_override_active() {
		if let Some(wid) = state.focused_x11_window {
			// XWayland path: find the focused X11 window and route events there.
			for window in state.space.elements() {
				if let Some(x11) = window.x11_surface()
					&& x11.window_id() == wid
				{
					let window_loc = state.space.element_geometry(window)?.loc;
					let pos_within_window = position - window_loc.to_f64();
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
	let (window, window_loc) = state.space.element_under(position)?;
	let pos_within_window = position - window_loc.to_f64();
	let (surface, surface_offset) = window.surface_under(pos_within_window, WindowSurfaceType::ALL)?;
	Some((surface, surface_offset.to_f64() + window_loc.to_f64()))
}
