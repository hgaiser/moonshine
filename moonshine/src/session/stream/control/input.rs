#[repr(u32)]
enum InputEventType {
	KeyDown = 0x00000003,
	KeyUp = 0x00000004,
	MouseMoveAbsolute = 0x00000005,
	// MouseMoveRelative = 0x00000006 (pre gen5)
	MouseMoveRelative = 0x00000007,
	MouseButtonDown = 0x00000008,
	MouseButtonUp = 0x00000009,
}

impl TryFrom<u32> for InputEventType {
	type Error = ();

	fn try_from(v: u32) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::KeyDown as u32 => Ok(Self::KeyDown),
			x if x == Self::KeyUp as u32 => Ok(Self::KeyUp),
			x if x == Self::MouseMoveAbsolute as u32 => Ok(Self::MouseMoveAbsolute),
			x if x == Self::MouseMoveRelative as u32 => Ok(Self::MouseMoveRelative),
			x if x == Self::MouseButtonDown as u32 => Ok(Self::MouseButtonDown),
			x if x == Self::MouseButtonUp as u32 => Ok(Self::MouseButtonUp),
			_ => {
				log::warn!("Unknown event type: {v}");
				Err(())
			},
		}
	}
}

#[derive(Debug)]
pub enum InputEvent {
	KeyDown(KeyEvent),
	KeyUp(KeyEvent),
	MouseMoveAbsolute(MouseMoveAbsolute),
	MouseMoveRelative(MouseMoveRelative),
	MouseButtonDown(MouseButton),
	MouseButtonUp(MouseButton),
}

#[derive(Debug)]
pub struct MouseMoveAbsolute {
	pub x: i16,
	pub y: i16,
	pub padding: u16,
	pub width: i16,
	pub height: i16,
}

#[derive(Debug)]
pub struct MouseMoveRelative {
	pub x: i16,
	pub y: i16,
}

#[repr(u8)]
pub enum KeyModifier {
	Shift = 0x01,
	Ctrl = 0x02,
	Alt = 0x04,
	Meta = 0x08,
}

#[derive(Debug)]
pub struct KeyEvent {
	pub flags: u8,
	pub key: u16,
	pub modifiers: u8,
	pub padding: u16,
}

// impl Into<enigo::Key> for KeyEvent {
// 	fn into(self) -> enigo::Key {
// 		enigo::Key::
// 	}
// }

#[derive(Debug)]
#[repr(u8)]
pub enum MouseButton {
	Left = 0x01,
	Middle = 0x02,
	Right = 0x03,
	X1 = 0x04, // What are these?
	X2 = 0x05, // What are these?
}

impl TryFrom<u8> for MouseButton {
	type Error = ();

	fn try_from(v: u8) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::Left as u8 => Ok(Self::Left),
			x if x == Self::Middle as u8 => Ok(Self::Middle),
			x if x == Self::Right as u8 => Ok(Self::Right),
			x if x == Self::X1 as u8 => Ok(Self::X1),
			x if x == Self::X2 as u8 => Ok(Self::X2),
			_ => Err(()),
		}
	}
}

impl Into<enigo::MouseButton> for MouseButton {
	fn into(self) -> enigo::MouseButton {
		match self {
			MouseButton::Left => enigo::MouseButton::Left,
			MouseButton::Middle => enigo::MouseButton::Middle,
			MouseButton::Right => enigo::MouseButton::Right,
			MouseButton::X1 => enigo::MouseButton::Back,
			MouseButton::X2 => enigo::MouseButton::Forward,
		}
	}
}

impl InputEvent {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			log::warn!("Expected control message to have at least 4 bytes, got {}", buffer.len());
			return Err(());
		}

		match u32::from_le_bytes(buffer[..4].try_into().unwrap()).try_into()? {
			InputEventType::KeyDown => {
				if buffer.len() != std::mem::size_of::<KeyEvent>() + 4 {
					log::warn!(
						"Expected KeyDown message to have exactly {} bytes, got {}",
						std::mem::size_of::<KeyEvent>() + 4,
						buffer.len()
					);
					return Err(());
				}

				Ok(InputEvent::KeyDown(KeyEvent {
					flags: buffer[4],
					key: u16::from_le_bytes(buffer[5..7].try_into().unwrap()) & 0x00FF,
					modifiers: buffer[7],
					padding: 0,
				}))
			},
			InputEventType::KeyUp => {
				if buffer.len() != std::mem::size_of::<KeyEvent>() + 4 {
					log::warn!(
						"Expected KeyUp message to have exactly {} bytes, got {}",
						std::mem::size_of::<KeyEvent>() + 4,
						buffer.len()
					);						return Err(());
				}

				Ok(InputEvent::KeyUp(KeyEvent {
					flags: buffer[4],
					key: u16::from_le_bytes(buffer[5..7].try_into().unwrap()) & 0x00FF,
					modifiers: buffer[7],
					padding: 0,
				}))
			},
			InputEventType::MouseMoveAbsolute => {
				if buffer.len() != std::mem::size_of::<MouseMoveAbsolute>() + 4 {
					log::warn!(
						"Expected absolute mouse movement message to have exactly {} bytes, got {}",
						std::mem::size_of::<MouseMoveAbsolute>() + 4,
						buffer.len()
					);
					return Err(());
				}

				// Moonlight seems to send { x, y, unused, width, height }.
				// We don't seem to need the width and height?
				Ok(InputEvent::MouseMoveAbsolute(MouseMoveAbsolute {
					x: i16::from_be_bytes(buffer[4..6].try_into().unwrap()),
					y: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
					padding: 0,
					width: i16::from_be_bytes(buffer[10..12].try_into().unwrap()),
					height: i16::from_be_bytes(buffer[12..14].try_into().unwrap()),
				}))
			},
			InputEventType::MouseMoveRelative => {
				// Expect 2 i16's.
				if buffer.len() != std::mem::size_of::<MouseMoveRelative>() + 4 {
					log::warn!("Expected relative mouse movement message to have exactly 8 bytes, got {}", buffer.len());
					return Err(());
				}

				Ok(InputEvent::MouseMoveRelative(MouseMoveRelative {
					x: i16::from_be_bytes(buffer[4..6].try_into().unwrap()),
					y: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
				}))
			},
			InputEventType::MouseButtonDown => {
				// Expect 1 u8.
				if buffer.len() != 1 + 4 {
					log::warn!("Expected mouse button down message to have exactly 5 bytes, got {}", buffer.len());
					return Err(());
				}

				Ok(InputEvent::MouseButtonDown(buffer[4].try_into()?))
			},
			InputEventType::MouseButtonUp => {
				// Expect 1 u8.
				if buffer.len() != 1 + 4 {
					log::warn!("Expected mouse button up message to have exactly 5 bytes, got {}", buffer.len());
					return Err(());
				}

				Ok(InputEvent::MouseButtonUp(buffer[4].try_into()?))
			},
		}
	}
}
