use evdev::{uinput::{VirtualDeviceBuilder, VirtualDevice}, AttributeSet, RelativeAxisType, Key};
use strum_macros::FromRepr;

#[derive(Debug)]
pub struct MouseMoveAbsolute {
	x: i16,
	y: i16,
	width: i16,
	height: i16,
}

impl MouseMoveAbsolute {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<i16>()   // x
			+ std::mem::size_of::<i16>() // y
			+ std::mem::size_of::<i16>() // padding
			+ std::mem::size_of::<i16>() // width
			+ std::mem::size_of::<i16>() // height
		;

		if buffer.len() < EXPECTED_SIZE {
			log::warn!("Expected at least {EXPECTED_SIZE} bytes for MouseMoveAbsolute, got {} bytes.", buffer.len());
			return Err(());
		}

		Ok(Self {
			x: i16::from_be_bytes(buffer[0..2].try_into().unwrap()),
			y: i16::from_be_bytes(buffer[2..4].try_into().unwrap()),
			width: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
			height: i16::from_be_bytes(buffer[8..10].try_into().unwrap()),
		})
	}
}

#[derive(Debug)]
pub struct MouseMoveRelative {
	pub x: i16,
	pub y: i16,
}

impl MouseMoveRelative {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < std::mem::size_of::<Self>() {
			log::warn!("Expected at least {} bytes for MouseMoveRelative, got {} bytes.", std::mem::size_of::<Self>(), buffer.len());
			return Err(());
		}

		Ok(Self {
			x: i16::from_be_bytes(buffer[0..2].try_into().unwrap()),
			y: i16::from_be_bytes(buffer[2..4].try_into().unwrap()),
		})
	}
}

#[derive(Debug, Eq, PartialEq, FromRepr)]
#[repr(u8)]
pub enum MouseButton {
	Left = 0x01,
	Middle = 0x02,
	Right = 0x03,
	Side = 0x04,
	Extra = 0x05,
}

impl MouseButton {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize = std::mem::size_of::<u8>(); // button

		if buffer.len() < EXPECTED_SIZE {
			log::warn!("Expected at least {EXPECTED_SIZE} bytes for MouseButton, got {} bytes.", buffer.len());
			return Err(());
		}

		MouseButton::from_repr(buffer[0]).ok_or_else(|| log::warn!("Unknown mouse button: {}", buffer[0]))
	}
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

pub struct Mouse {
	device: VirtualDevice,
}

impl Mouse {
	pub fn new() -> Result<Self, ()> {
		let device = VirtualDeviceBuilder::new()
			.map_err(|e| log::error!("Failed to initiate virtual mouse: {e}"))?
			.name("Moonshine Mouse")
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