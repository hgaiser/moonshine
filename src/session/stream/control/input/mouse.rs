use reis::ei::button::ButtonState;
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

pub struct Mouse {
	device: Option<reis::ei::Device>,
	pointer: Option<reis::ei::Pointer>,
	pointer_absolute: Option<reis::ei::PointerAbsolute>,
	button: Option<reis::ei::Button>,
	scroll: Option<reis::ei::Scroll>,
}

impl Mouse {
	pub fn new() -> Result<Self, ()> {
		Ok(Self {
			device: None,
			pointer: None,
			pointer_absolute: None,
			button: None,
			scroll: None,
		})
	}

	pub fn set_device(&mut self, device: reis::ei::Device) {
		self.device = Some(device);
	}

	pub fn set_pointer(&mut self, pointer: reis::ei::Pointer) {
		self.pointer = Some(pointer);
	}

	pub fn set_pointer_absolute(&mut self, pointer_absolute: reis::ei::PointerAbsolute) {
		self.pointer_absolute = Some(pointer_absolute);
	}

	pub fn set_button(&mut self, button: reis::ei::Button) {
		self.button = Some(button);
	}

	pub fn set_scroll(&mut self, scroll: reis::ei::Scroll) {
		self.scroll = Some(scroll);
	}

	pub fn move_relative(&mut self, x: i32, y: i32) {
		if let Some(pointer) = &self.pointer {
			pointer.motion_relative(x as f32, y as f32);
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}

	pub fn move_absolute(&mut self, x: i32, y: i32, _screen_width: i32, _screen_height: i32) {
		if let Some(pointer_absolute) = &self.pointer_absolute {
			// TODO: Map coordinates if needed, but libei expects absolute coordinates within the region.
			// Assuming x, y are already correct for the region.
			pointer_absolute.motion_absolute(x as f32, y as f32);
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}

	pub fn button_down(&mut self, button: MouseButton) {
		if let Some(btn) = &self.button {
			btn.button(button.into(), ButtonState::Press);
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}

	pub fn button_up(&mut self, button: MouseButton) {
		if let Some(btn) = &self.button {
			btn.button(button.into(), ButtonState::Released);
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}

	pub fn scroll_vertical(&mut self, amount: i16) {
		if let Some(scroll) = &self.scroll {
			scroll.scroll_discrete(0, -amount as i32);
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}

	pub fn scroll_horizontal(&mut self, amount: i16) {
		if let Some(scroll) = &self.scroll {
			// let clicks = amount as f32 / 120.0;
			scroll.scroll_discrete(amount as i32, 0);
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}
}
