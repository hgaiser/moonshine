use inputtino::DeviceDefinition;
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
}

pub struct Keyboard {
	keyboard: inputtino::Keyboard,
}

impl Keyboard {
	pub fn new() -> Result<Self, ()> {
		let definition = DeviceDefinition::new(
			"Moonshine Keyboard",
			0xBEEF,
			0xDEAD,
			0x111,
			"00:11:22:33:44",
			"00:11:22:33:44",
		);
		let keyboard = inputtino::Keyboard::new(&definition)
			.map_err(|e| tracing::error!("Failed to create virtual keyboard: {e}"))?;

		Ok(Self { keyboard })
	}

	pub fn key_down(&mut self, key: Key) {
		self.keyboard.press_key(key as i16);
	}

	pub fn key_up(&mut self, key: Key) {
		self.keyboard.release_key(key as i16);
	}
}
