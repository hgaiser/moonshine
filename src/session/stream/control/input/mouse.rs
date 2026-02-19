use strum_macros::FromRepr;

#[derive(Debug)]
pub struct MouseMoveAbsolute {
	pub x: i16,
	pub y: i16,
	pub screen_width: i16,
	pub screen_height: i16,
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
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for MouseMoveAbsolute, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			x: i16::from_be_bytes(buffer[0..2].try_into().unwrap()),
			y: i16::from_be_bytes(buffer[2..4].try_into().unwrap()),
			screen_width: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
			screen_height: i16::from_be_bytes(buffer[8..10].try_into().unwrap()),
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
			tracing::warn!(
				"Expected at least {} bytes for MouseMoveRelative, got {} bytes.",
				std::mem::size_of::<Self>(),
				buffer.len()
			);
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
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for MouseButton, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		MouseButton::from_repr(buffer[0]).ok_or_else(|| tracing::warn!("Unknown mouse button: {}", buffer[0]))
	}
}

impl From<MouseButton> for u32 {
	fn from(val: MouseButton) -> Self {
		match val {
			MouseButton::Left => 0x110,
			MouseButton::Middle => 0x112,
			MouseButton::Right => 0x111,
			MouseButton::Side => 0x113,
			MouseButton::Extra => 0x114,
		}
	}
}

#[derive(Debug)]
pub struct MouseScrollVertical {
	pub amount: i16,
}

impl MouseScrollVertical {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < std::mem::size_of::<Self>() {
			tracing::warn!(
				"Expected at least {} bytes for MouseScrollVertical, got {} bytes.",
				std::mem::size_of::<Self>(),
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			amount: i16::from_be_bytes(buffer[0..2].try_into().unwrap()),
		})
	}
}

#[derive(Debug)]
pub struct MouseScrollHorizontal {
	pub amount: i16,
}

impl MouseScrollHorizontal {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < std::mem::size_of::<Self>() {
			tracing::warn!(
				"Expected at least {} bytes for MouseScrollHorizontal, got {} bytes.",
				std::mem::size_of::<Self>(),
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			amount: i16::from_be_bytes(buffer[0..2].try_into().unwrap()),
		})
	}
}
