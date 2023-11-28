use evdev::{uinput::{VirtualDeviceBuilder, VirtualDevice}, AttributeSet, RelativeAxisType, Key};

#[derive(Debug)]
#[repr(u8)]
pub enum MouseButton {
	Left = 0x01,
	Middle = 0x02,
	Right = 0x03,
	Side = 0x04,
	Extra = 0x05,
}

impl From<MouseButton> for Key {
	fn from(val: MouseButton) -> Self {
		match val {
			MouseButton::Left => Key::BTN_LEFT,
			MouseButton::Middle => Key::BTN_MIDDLE,
			MouseButton::Right => Key::BTN_RIGHT,
			MouseButton::Side => Key::BTN_SIDE,
			MouseButton::Extra => Key::BTN_EXTRA,
		}
	}
}

impl TryFrom<u8> for MouseButton {
	type Error = ();

	fn try_from(v: u8) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::Left as u8 => Ok(Self::Left),
			x if x == Self::Middle as u8 => Ok(Self::Middle),
			x if x == Self::Right as u8 => Ok(Self::Right),
			x if x == Self::Side as u8 => Ok(Self::Side),
			x if x == Self::Extra as u8 => Ok(Self::Extra),
			_ => Err(()),
		}
	}
}

pub struct Mouse {
	device: VirtualDevice,
}

impl Mouse {
	pub fn new() -> Result<Self, ()> {
		let device = VirtualDeviceBuilder::new()
			.map_err(|e| log::error!("Failed to initiate virtual mouse: {e}"))?
			.name("moonshine-mouse")
			.with_relative_axes(&AttributeSet::from_iter([
				RelativeAxisType::REL_X,
				RelativeAxisType::REL_Y,
				RelativeAxisType::REL_WHEEL,
				RelativeAxisType::REL_HWHEEL,
			]))
			.map_err(|e| log::error!("Failed to enable relative axes for virtual mouse: {e}"))?
			// .with_absolute_axis(UinputAbsSetup::)
			// .map_err(|e| log::error!("Failed to enable absolute axes for virtual mouse: {e}"))?
			.with_keys(&AttributeSet::from_iter([
				Key::BTN_LEFT,
				Key::BTN_MIDDLE,
				Key::BTN_RIGHT,
				Key::BTN_FORWARD,
				Key::BTN_BACK,
			]))
			.map_err(|e| log::error!("Failed to add keys to virtual mouse: {e}"))?
			.build()
			.map_err(|e| log::error!("Failed to create virtual mouse: {e}"))?;

		Ok(Self { device })
	}

	pub fn move_relative(&mut self, x: i32, y: i32) -> Result<(), ()> {
		let event_x = evdev::InputEvent::new_now(
			evdev::EventType::RELATIVE,
			RelativeAxisType::REL_X.0,
			x,
		);
		let event_y = evdev::InputEvent::new_now(
			evdev::EventType::RELATIVE,
			RelativeAxisType::REL_Y.0,
			y,
		);
		self.device.emit(&[event_x, event_y])
			.map_err(|e| log::error!("Failed to make relative mouse movement: {e}"))
	}

	pub fn button_down(&mut self, button: MouseButton) -> Result<(), ()> {
		let button_event = evdev::InputEvent::new_now(
			evdev::EventType::KEY,
			Into::<Key>::into(button).code(),
			1
		);

		self.device.emit(&[button_event])
			.map_err(|e| log::error!("Failed to press mouse button: {e}"))
	}

	pub fn button_up(&mut self, button: MouseButton) -> Result<(), ()> {
		let button_event = evdev::InputEvent::new_now(
			evdev::EventType::KEY,
			Into::<Key>::into(button).code(),
			0
		);

		self.device.emit(&[button_event])
			.map_err(|e| log::error!("Failed to release mouse button: {e}"))
	}
}