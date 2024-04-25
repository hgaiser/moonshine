use anyhow::{bail, Context, Result};
use evdev::{
	uinput::{VirtualDevice, VirtualDeviceBuilder},
	AbsInfo,
	AbsoluteAxisType,
	AttributeSet,
	Key,
	RelativeAxisType,
	UinputAbsSetup,
};
use strum_macros::FromRepr;

#[derive(Debug)]
pub struct MouseMoveAbsolute {
	pub x: i16,
	pub y: i16,
	// width: i16,
	// height: i16,
}

impl MouseMoveAbsolute {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<i16>()   // x
			+ std::mem::size_of::<i16>() // y
			+ std::mem::size_of::<i16>() // padding
			+ std::mem::size_of::<i16>() // width
			+ std::mem::size_of::<i16>() // height
		;

		if buffer.len() < EXPECTED_SIZE {
			bail!(
				"Expected at least {EXPECTED_SIZE} bytes for MouseMoveAbsolute, got {} bytes.",
				buffer.len()
			)
		}

		Ok(Self {
			x: i16::from_be_bytes(buffer[0..2].try_into()?),
			y: i16::from_be_bytes(buffer[2..4].try_into()?),
			// width: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
			// height: i16::from_be_bytes(buffer[8..10].try_into().unwrap()),
		})
	}
}

#[derive(Debug)]
pub struct MouseMoveRelative {
	pub x: i16,
	pub y: i16,
}

impl MouseMoveRelative {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self> {
		if buffer.len() < std::mem::size_of::<Self>() {
			bail!(
				"Expected at least {} bytes for MouseMoveRelative, got {} bytes.",
				std::mem::size_of::<Self>(),
				buffer.len()
			)
		}

		Ok(Self {
			x: i16::from_be_bytes(buffer[0..2].try_into()?),
			y: i16::from_be_bytes(buffer[2..4].try_into()?),
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
	pub fn from_bytes(buffer: &[u8]) -> Result<Self> {
		const EXPECTED_SIZE: usize = std::mem::size_of::<u8>(); // button

		if buffer.len() < EXPECTED_SIZE {
			bail!(
				"Expected at least {EXPECTED_SIZE} bytes for MouseButton, got {} bytes.",
				buffer.len()
			)
		}

		MouseButton::from_repr(buffer[0]).with_context(|| format!("Unknown mouse button: {}", buffer[0]))
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

#[derive(Debug)]
pub struct MouseScrollVertical {
	pub amount: i16,
}

impl MouseScrollVertical {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self> {
		if buffer.len() < std::mem::size_of::<Self>() {
			bail!(
				"Expected at least {} bytes for MouseScrollVertical, got {} bytes.",
				std::mem::size_of::<Self>(),
				buffer.len()
			);
		}

		Ok(Self {
			amount: i16::from_be_bytes(buffer[0..2].try_into()?),
		})
	}
}

#[derive(Debug)]
pub struct MouseScrollHorizontal {
	pub amount: i16,
}

impl MouseScrollHorizontal {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self> {
		if buffer.len() < std::mem::size_of::<Self>() {
			bail!(
				"Expected at least {} bytes for MouseScrollHorizontal, got {} bytes.",
				std::mem::size_of::<Self>(),
				buffer.len()
			)
		}

		Ok(Self {
			amount: i16::from_be_bytes(buffer[0..2].try_into()?),
		})
	}
}

pub struct Mouse {
	device: VirtualDevice,
}

impl Mouse {
	pub fn new() -> Result<Self> {
		let device = VirtualDeviceBuilder::new()
			.context("Failed to initiate virtual mouse")?
			.name("Moonshine Mouse")
			.with_relative_axes(&AttributeSet::from_iter([
				RelativeAxisType::REL_X,
				RelativeAxisType::REL_Y,
				RelativeAxisType::REL_WHEEL_HI_RES,
				RelativeAxisType::REL_HWHEEL_HI_RES,
			]))
			.context("Failed to enable relative axes for virtual mouse")?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_X,
				AbsInfo::new(0, 0, 3000, 0, 0, 1),
			))
			.context("Failed to enable absolute axis for virtual mouse")?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_Y,
				AbsInfo::new(0, 0, 3000, 0, 0, 1),
			))
			.context("Failed to enable absolute axis for virtual mouse")?
			.with_keys(&AttributeSet::from_iter([
				Key::BTN_LEFT,
				Key::BTN_MIDDLE,
				Key::BTN_RIGHT,
				Key::BTN_FORWARD,
				Key::BTN_BACK,
			]))
			.context("Failed to add keys to virtual mouse")?
			.build()
			.context("Failed to create virtual mouse")?;

		Ok(Self { device })
	}

	pub fn move_relative(&mut self, x: i32, y: i32) -> Result<()> {
		let events = [
			evdev::InputEvent::new_now(evdev::EventType::RELATIVE, RelativeAxisType::REL_X.0, x),
			evdev::InputEvent::new_now(evdev::EventType::RELATIVE, RelativeAxisType::REL_Y.0, y),
		];
		self.device
			.emit(&events)
			.context("Failed to make relative mouse movement")
	}

	pub fn move_absolute(&mut self, x: i32, y: i32) -> Result<()> {
		tracing::info!("x: {x}, y: {y}");
		let events = [
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, x),
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, y),
		];
		self.device
			.emit(&events)
			.context("Failed to make absolute mouse movement")
	}

	pub fn button_down(&mut self, button: MouseButton) -> Result<()> {
		let button_event = evdev::InputEvent::new_now(evdev::EventType::KEY, Into::<Key>::into(button).code(), 1);

		self.device
			.emit(&[button_event])
			.context("Failed to press mouse button")
	}

	pub fn button_up(&mut self, button: MouseButton) -> Result<()> {
		let button_event = evdev::InputEvent::new_now(evdev::EventType::KEY, Into::<Key>::into(button).code(), 0);

		self.device
			.emit(&[button_event])
			.context("Failed to release mouse button")
	}

	pub fn scroll_vertical(&mut self, amount: i16) -> Result<()> {
		let events = [evdev::InputEvent::new_now(
			evdev::EventType::RELATIVE,
			RelativeAxisType::REL_WHEEL_HI_RES.0,
			amount as i32,
		)];
		self.device.emit(&events).context("Failed to scroll vertically")
	}

	pub fn scroll_horizontal(&mut self, amount: i16) -> Result<()> {
		let events = [evdev::InputEvent::new_now(
			evdev::EventType::RELATIVE,
			RelativeAxisType::REL_HWHEEL_HI_RES.0,
			amount as i32,
		)];
		self.device.emit(&events).context("Failed to scroll horizontally")
	}
}
