//! Input from the Moonlight control stream, in the compositor seat's terms.
//!
//! The control stream sends events straight to ferriscope's seat through
//! [`CompositorInput`]. Keys, buttons and relative motion pass through as they
//! are; the helpers here convert the events whose units differ: absolute
//! motion and touch (client screen or normalised coordinates to output
//! coordinates) and scroll (Moonlight's 120 units per detent to wheel notches).

use std::sync::{Arc, OnceLock};

use ferriscope_core::{InputEvent, InputSender};

/// The compositor's seat, connected once the compositor has launched.
///
/// Events sent before then, or after the compositor stopped, are dropped.
#[derive(Clone)]
pub(crate) struct CompositorInput {
	sender: Arc<OnceLock<InputSender>>,
	/// The compositor output size, which absolute and touch input map onto.
	width: u32,
	height: u32,
}

impl CompositorInput {
	pub(crate) fn new(width: u32, height: u32) -> Self {
		Self {
			sender: Arc::default(),
			width,
			height,
		}
	}

	/// Connect the launched compositor's seat.
	pub(super) fn connect(&self, sender: InputSender) {
		let _ = self.sender.set(sender);
	}

	/// Send one event to the seat.
	pub(crate) fn send(&self, event: InputEvent) {
		if let Some(sender) = self.sender.get() {
			let _ = sender.send(event);
		}
	}

	/// Send one touch event. `wl_touch` clients act on `frame`, and each
	/// Moonlight touch event is one complete frame.
	pub(crate) fn send_touch(&self, event: InputEvent) {
		self.send(event);
		self.send(InputEvent::TouchFrame);
	}

	/// Absolute pointer motion from a position on the client's screen of
	/// `screen_width` × `screen_height`, rescaled to the output.
	pub(crate) fn pointer_motion(&self, x: i16, y: i16, screen_width: i16, screen_height: i16) -> InputEvent {
		let scale = |value: i16, screen: i16, output: u32| {
			if screen > 0 {
				f64::from(value) / f64::from(screen) * f64::from(output)
			} else {
				f64::from(value)
			}
		};
		InputEvent::PointerMotion {
			x: scale(x, screen_width, self.width),
			y: scale(y, screen_height, self.height),
		}
	}

	/// A touch position in normalised video coordinates, in output coordinates.
	/// ferriscope applies the same output fill as for the pointer.
	pub(crate) fn touch_point(&self, x: f32, y: f32) -> (f64, f64) {
		(
			f64::from(x) * f64::from(self.width),
			f64::from(y) * f64::from(self.height),
		)
	}
}

/// A scroll of `amount` in Moonlight's high-resolution units (120 per detent,
/// positive up or right) as wheel notches in the Wayland `wl_pointer.axis`
/// sign convention (positive down or right), so the vertical sign is flipped.
///
/// ferriscope expands a notch to exactly one continuous `wl_pointer.axis` unit
/// and 120 `axis_value120`, matching gamescope (`wlserver_mousewheel`,
/// `wlserver.cpp:3131-3138`).
pub(crate) fn scroll(amount: i16, horizontal: bool) -> InputEvent {
	let notches = f64::from(amount) / 120.0;
	InputEvent::PointerAxis {
		horizontal,
		value: if horizontal { notches } else { -notches },
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rescales_absolute_motion_to_output() {
		let input = CompositorInput::new(1920, 1080);
		assert_eq!(
			input.pointer_motion(50, 100, 100, 200),
			InputEvent::PointerMotion { x: 960.0, y: 540.0 }
		);
		// Without a screen size the position is taken as is.
		assert_eq!(
			input.pointer_motion(50, 100, 0, 0),
			InputEvent::PointerMotion { x: 50.0, y: 100.0 }
		);
	}

	#[test]
	fn maps_scroll_to_pointer_axis_with_wayland_sign() {
		// One detent up: Moonlight positive, flipped to Wayland's positive-down.
		assert_eq!(
			scroll(120, false),
			InputEvent::PointerAxis {
				horizontal: false,
				value: -1.0
			}
		);
		assert_eq!(
			scroll(-120, false),
			InputEvent::PointerAxis {
				horizontal: false,
				value: 1.0
			}
		);
		// Horizontal is already right-positive in both conventions (no flip).
		assert_eq!(
			scroll(60, true),
			InputEvent::PointerAxis {
				horizontal: true,
				value: 0.5
			}
		);
	}

	#[test]
	fn touch_is_scaled_from_normalized_to_output_coordinates() {
		assert_eq!(CompositorInput::new(1920, 1080).touch_point(0.5, 0.25), (960.0, 270.0));
	}

	#[test]
	fn events_before_launch_are_dropped() {
		// Not connected yet: sending must not panic or block.
		CompositorInput::new(1920, 1080).send(InputEvent::Key {
			code: 30,
			pressed: true,
		});
	}
}
