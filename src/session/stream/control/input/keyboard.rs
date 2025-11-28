use reis::ei;
use reis::ei::keyboard::KeyState;
use strum_macros::FromRepr;

#[derive(Debug, Eq, PartialEq, FromRepr)]
#[repr(u8)]
pub enum Key {
	Backspace = 0x08,
	Tab = 0x09,
	Clear = 0x0C,
	Return = 0x0D,
	Shift = 0x10,
	Control = 0x11,
	Alt = 0x12,
	Pause = 0x13,
	Capslock = 0x14,
	Katakanahiragana = 0x15,
	Hangeul = 0x16,
	Hanja = 0x17,
	Katakana = 0x19,
	Escape = 0x1B,
	Space = 0x20,
	PageUp = 0x21,
	PageDown = 0x22,
	End = 0x23,
	Home = 0x24,
	Left = 0x25,
	Up = 0x26,
	Right = 0x27,
	Down = 0x28,
	Select = 0x29,
	Print = 0x2A,
	SysRq = 0x2C,
	Insert = 0x2D,
	Delete = 0x2E,
	Help = 0x2F,
	Num0 = 0x30,
	Num1 = 0x31,
	Num2 = 0x32,
	Num3 = 0x33,
	Num4 = 0x34,
	Num5 = 0x35,
	Num6 = 0x36,
	Num7 = 0x37,
	Num8 = 0x38,
	Num9 = 0x39,
	A = 0x41,
	B = 0x42,
	C = 0x43,
	D = 0x44,
	E = 0x45,
	F = 0x46,
	G = 0x47,
	H = 0x48,
	I = 0x49,
	J = 0x4A,
	K = 0x4B,
	L = 0x4C,
	M = 0x4D,
	N = 0x4E,
	O = 0x4F,
	P = 0x50,
	Q = 0x51,
	R = 0x52,
	S = 0x53,
	T = 0x54,
	U = 0x55,
	V = 0x56,
	W = 0x57,
	X = 0x58,
	Y = 0x59,
	Z = 0x5A,
	LeftMeta = 0x5B,
	RightMeta = 0x5C,
	Sleep = 0x5F,
	Numpad0 = 0x60,
	Numpad1 = 0x61,
	Numpad2 = 0x62,
	Numpad3 = 0x63,
	Numpad4 = 0x64,
	Numpad5 = 0x65,
	Numpad6 = 0x66,
	Numpad7 = 0x67,
	Numpad8 = 0x68,
	Numpad9 = 0x69,
	NumpadAsterisk = 0x6A,
	NumpadPlus = 0x6B,
	NumpadComma = 0x6C,
	NumpadMinus = 0x6D,
	NumpadDot = 0x6E,
	NumpadSlash = 0x6F,
	F1 = 0x70,
	F2 = 0x71,
	F3 = 0x72,
	F4 = 0x73,
	F5 = 0x74,
	F6 = 0x75,
	F7 = 0x76,
	F8 = 0x77,
	F9 = 0x78,
	F10 = 0x79,
	F11 = 0x7A,
	F12 = 0x7B,
	F13 = 0x7C,
	F14 = 0x7D,
	F15 = 0x7E,
	F16 = 0x7F,
	F17 = 0x80,
	F18 = 0x81,
	F19 = 0x82,
	F20 = 0x83,
	F21 = 0x84,
	F22 = 0x85,
	F23 = 0x86,
	F24 = 0x87,
	Numlock = 0x90,
	Scroll = 0x91,
	LeftShift = 0xA0,
	RightShift = 0xA1,
	LeftControl = 0xA2,
	RightControl = 0xA3,
	LeftAlt = 0xA4,
	RightAlt = 0xA5,
	Semicolon = 0xBA,
	Equal = 0xBB,
	Comma = 0xBC,
	Minus = 0xBD,
	Dot = 0xBE,
	Slash = 0xBF,
	Grave = 0xC0,
	LeftBrace = 0xDB,
	Backslash = 0xDC,
	RightBrace = 0xDD,
	Apostrophe = 0xDE,
	NonUsBackslash = 0xE2,
}

impl Key {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u8>()    // flags
			+ std::mem::size_of::<u16>() // key
			+ std::mem::size_of::<u8>()  // modifiers
			+ std::mem::size_of::<u16>() // padding
		;

		if buffer.len() < EXPECTED_SIZE {
			tracing::warn!("Expected at least {EXPECTED_SIZE} bytes for Key, got {} bytes.", buffer.len());
			return Err(());
		}


		Key::from_repr(buffer[1]).ok_or_else(|| tracing::warn!("Unknown keycode: {}", buffer[5]))
	}

	fn to_linux_keycode(&self) -> Option<u32> {
		match self {
			Key::Backspace => Some(14),
			Key::Tab => Some(15),
			Key::Return => Some(28),
			Key::Shift => Some(42),
			Key::Control => Some(29),
			Key::Alt => Some(56),
			Key::Pause => Some(119),
			Key::Capslock => Some(58),
			Key::Escape => Some(1),
			Key::Space => Some(57),
			Key::PageUp => Some(104),
			Key::PageDown => Some(109),
			Key::End => Some(107),
			Key::Home => Some(102),
			Key::Left => Some(105),
			Key::Up => Some(103),
			Key::Right => Some(106),
			Key::Down => Some(108),
			Key::Insert => Some(110),
			Key::Delete => Some(111),
			Key::Num0 => Some(11),
			Key::Num1 => Some(2),
			Key::Num2 => Some(3),
			Key::Num3 => Some(4),
			Key::Num4 => Some(5),
			Key::Num5 => Some(6),
			Key::Num6 => Some(7),
			Key::Num7 => Some(8),
			Key::Num8 => Some(9),
			Key::Num9 => Some(10),
			Key::A => Some(30),
			Key::B => Some(48),
			Key::C => Some(46),
			Key::D => Some(32),
			Key::E => Some(18),
			Key::F => Some(33),
			Key::G => Some(34),
			Key::H => Some(35),
			Key::I => Some(23),
			Key::J => Some(36),
			Key::K => Some(37),
			Key::L => Some(38),
			Key::M => Some(50),
			Key::N => Some(49),
			Key::O => Some(24),
			Key::P => Some(25),
			Key::Q => Some(16),
			Key::R => Some(19),
			Key::S => Some(31),
			Key::T => Some(20),
			Key::U => Some(22),
			Key::V => Some(47),
			Key::W => Some(17),
			Key::X => Some(45),
			Key::Y => Some(21),
			Key::Z => Some(44),
			Key::LeftMeta => Some(125),
			Key::RightMeta => Some(126),
			Key::Numpad0 => Some(82),
			Key::Numpad1 => Some(79),
			Key::Numpad2 => Some(80),
			Key::Numpad3 => Some(81),
			Key::Numpad4 => Some(75),
			Key::Numpad5 => Some(76),
			Key::Numpad6 => Some(77),
			Key::Numpad7 => Some(71),
			Key::Numpad8 => Some(72),
			Key::Numpad9 => Some(73),
			Key::NumpadAsterisk => Some(55),
			Key::NumpadPlus => Some(78),
			Key::NumpadMinus => Some(74),
			Key::NumpadDot => Some(83),
			Key::NumpadSlash => Some(98),
			Key::F1 => Some(59),
			Key::F2 => Some(60),
			Key::F3 => Some(61),
			Key::F4 => Some(62),
			Key::F5 => Some(63),
			Key::F6 => Some(64),
			Key::F7 => Some(65),
			Key::F8 => Some(66),
			Key::F9 => Some(67),
			Key::F10 => Some(68),
			Key::F11 => Some(87),
			Key::F12 => Some(88),
			Key::Numlock => Some(69),
			Key::Scroll => Some(70),
			Key::LeftShift => Some(42),
			Key::RightShift => Some(54),
			Key::LeftControl => Some(29),
			Key::RightControl => Some(97),
			Key::LeftAlt => Some(56),
			Key::RightAlt => Some(100),
			Key::Semicolon => Some(39),
			Key::Equal => Some(13),
			Key::Comma => Some(51),
			Key::Minus => Some(12),
			Key::Dot => Some(52),
			Key::Slash => Some(53),
			Key::Grave => Some(41),
			Key::LeftBrace => Some(26),
			Key::Backslash => Some(43),
			Key::RightBrace => Some(27),
			Key::Apostrophe => Some(40),
			_ => None,
		}
	}
}

pub struct Keyboard {
	device: Option<ei::Device>,
	keyboard: Option<ei::Keyboard>,
}

impl Keyboard {
	pub fn new() -> Result<Self, ()> {
		Ok(Self { device: None, keyboard: None })
	}

	pub fn set_device(&mut self, device: ei::Device) {
		self.device = Some(device);
	}

	pub fn set_keyboard(&mut self, keyboard: ei::Keyboard) {
		self.keyboard = Some(keyboard);
	}

	pub fn key_down(&mut self, key: Key) {
		if let Some(keyboard) = &self.keyboard {
			if let Some(keycode) = key.to_linux_keycode() {
				keyboard.key(keycode, KeyState::Press);
			}
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}

	pub fn key_up(&mut self, key: Key) {
		if let Some(keyboard) = &self.keyboard {
			if let Some(keycode) = key.to_linux_keycode() {
				keyboard.key(keycode, KeyState::Released);
			}
		}
		if let Some(device) = &self.device {
			device.frame(0, 0);
		}
	}
}
