use anyhow::{bail, Context, Result};
use evdev::{
	uinput::{VirtualDevice, VirtualDeviceBuilder},
	AttributeSet,
};
use strum::IntoEnumIterator;
use strum_macros::{EnumIter, FromRepr};

#[derive(Debug, Eq, PartialEq, FromRepr, EnumIter)]
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
	pub fn from_bytes(buffer: &[u8]) -> Result<Self> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u8>()    // flags
			+ std::mem::size_of::<u16>() // key
			+ std::mem::size_of::<u8>()  // modifiers
			+ std::mem::size_of::<u16>() // padding
		;

		if buffer.len() < EXPECTED_SIZE {
			bail!(
				"Expected at least {EXPECTED_SIZE} bytes for Key, got {} bytes.",
				buffer.len()
			)
		}

		Key::from_repr(buffer[1]).with_context(|| format!("Unknown keycode: {}", buffer[5]))
	}
}

impl From<Key> for evdev::Key {
	fn from(val: Key) -> Self {
		match val {
			Key::Backspace => evdev::Key::KEY_BACKSPACE,
			Key::Tab => evdev::Key::KEY_TAB,
			Key::Clear => evdev::Key::KEY_CLEAR,
			Key::Return => evdev::Key::KEY_ENTER,
			Key::Shift => evdev::Key::KEY_LEFTSHIFT,
			Key::Control => evdev::Key::KEY_LEFTCTRL,
			Key::Alt => evdev::Key::KEY_LEFTALT,
			Key::Pause => evdev::Key::KEY_PAUSE,
			Key::Capslock => evdev::Key::KEY_CAPSLOCK,
			Key::Katakanahiragana => evdev::Key::KEY_KATAKANAHIRAGANA,
			Key::Hangeul => evdev::Key::KEY_HANGEUL,
			Key::Hanja => evdev::Key::KEY_HANJA,
			Key::Katakana => evdev::Key::KEY_KATAKANA,
			Key::Escape => evdev::Key::KEY_ESC,
			Key::Space => evdev::Key::KEY_SPACE,
			Key::PageUp => evdev::Key::KEY_PAGEUP,
			Key::PageDown => evdev::Key::KEY_PAGEDOWN,
			Key::End => evdev::Key::KEY_END,
			Key::Home => evdev::Key::KEY_HOME,
			Key::Left => evdev::Key::KEY_LEFT,
			Key::Up => evdev::Key::KEY_UP,
			Key::Right => evdev::Key::KEY_RIGHT,
			Key::Down => evdev::Key::KEY_DOWN,
			Key::Select => evdev::Key::KEY_SELECT,
			Key::Print => evdev::Key::KEY_PRINT,
			Key::SysRq => evdev::Key::KEY_SYSRQ,
			Key::Insert => evdev::Key::KEY_INSERT,
			Key::Delete => evdev::Key::KEY_DELETE,
			Key::Help => evdev::Key::KEY_HELP,
			Key::Num0 => evdev::Key::KEY_0,
			Key::Num1 => evdev::Key::KEY_1,
			Key::Num2 => evdev::Key::KEY_2,
			Key::Num3 => evdev::Key::KEY_3,
			Key::Num4 => evdev::Key::KEY_4,
			Key::Num5 => evdev::Key::KEY_5,
			Key::Num6 => evdev::Key::KEY_6,
			Key::Num7 => evdev::Key::KEY_7,
			Key::Num8 => evdev::Key::KEY_8,
			Key::Num9 => evdev::Key::KEY_9,
			Key::A => evdev::Key::KEY_A,
			Key::B => evdev::Key::KEY_B,
			Key::C => evdev::Key::KEY_C,
			Key::D => evdev::Key::KEY_D,
			Key::E => evdev::Key::KEY_E,
			Key::F => evdev::Key::KEY_F,
			Key::G => evdev::Key::KEY_G,
			Key::H => evdev::Key::KEY_H,
			Key::I => evdev::Key::KEY_I,
			Key::J => evdev::Key::KEY_J,
			Key::K => evdev::Key::KEY_K,
			Key::L => evdev::Key::KEY_L,
			Key::M => evdev::Key::KEY_M,
			Key::N => evdev::Key::KEY_N,
			Key::O => evdev::Key::KEY_O,
			Key::P => evdev::Key::KEY_P,
			Key::Q => evdev::Key::KEY_Q,
			Key::R => evdev::Key::KEY_R,
			Key::S => evdev::Key::KEY_S,
			Key::T => evdev::Key::KEY_T,
			Key::U => evdev::Key::KEY_U,
			Key::V => evdev::Key::KEY_V,
			Key::W => evdev::Key::KEY_W,
			Key::X => evdev::Key::KEY_X,
			Key::Y => evdev::Key::KEY_Y,
			Key::Z => evdev::Key::KEY_Z,
			Key::LeftMeta => evdev::Key::KEY_LEFTMETA,
			Key::RightMeta => evdev::Key::KEY_RIGHTMETA,
			Key::Sleep => evdev::Key::KEY_SLEEP,
			Key::Numpad0 => evdev::Key::KEY_KP0,
			Key::Numpad1 => evdev::Key::KEY_KP1,
			Key::Numpad2 => evdev::Key::KEY_KP2,
			Key::Numpad3 => evdev::Key::KEY_KP3,
			Key::Numpad4 => evdev::Key::KEY_KP4,
			Key::Numpad5 => evdev::Key::KEY_KP5,
			Key::Numpad6 => evdev::Key::KEY_KP6,
			Key::Numpad7 => evdev::Key::KEY_KP7,
			Key::Numpad8 => evdev::Key::KEY_KP8,
			Key::Numpad9 => evdev::Key::KEY_KP9,
			Key::NumpadAsterisk => evdev::Key::KEY_KPASTERISK,
			Key::NumpadPlus => evdev::Key::KEY_KPPLUS,
			Key::NumpadComma => evdev::Key::KEY_KPCOMMA,
			Key::NumpadMinus => evdev::Key::KEY_KPMINUS,
			Key::NumpadDot => evdev::Key::KEY_KPDOT,
			Key::NumpadSlash => evdev::Key::KEY_KPSLASH,
			Key::F1 => evdev::Key::KEY_F1,
			Key::F2 => evdev::Key::KEY_F2,
			Key::F3 => evdev::Key::KEY_F3,
			Key::F4 => evdev::Key::KEY_F4,
			Key::F5 => evdev::Key::KEY_F5,
			Key::F6 => evdev::Key::KEY_F6,
			Key::F7 => evdev::Key::KEY_F7,
			Key::F8 => evdev::Key::KEY_F8,
			Key::F9 => evdev::Key::KEY_F9,
			Key::F10 => evdev::Key::KEY_F10,
			Key::F11 => evdev::Key::KEY_F11,
			Key::F12 => evdev::Key::KEY_F12,
			Key::F13 => evdev::Key::KEY_F13,
			Key::F14 => evdev::Key::KEY_F14,
			Key::F15 => evdev::Key::KEY_F15,
			Key::F16 => evdev::Key::KEY_F16,
			Key::F17 => evdev::Key::KEY_F17,
			Key::F18 => evdev::Key::KEY_F18,
			Key::F19 => evdev::Key::KEY_F19,
			Key::F20 => evdev::Key::KEY_F20,
			Key::F21 => evdev::Key::KEY_F21,
			Key::F22 => evdev::Key::KEY_F22,
			Key::F23 => evdev::Key::KEY_F23,
			Key::F24 => evdev::Key::KEY_F24,
			Key::Numlock => evdev::Key::KEY_NUMLOCK,
			Key::Scroll => evdev::Key::KEY_SCROLLLOCK,
			Key::LeftShift => evdev::Key::KEY_LEFTSHIFT,
			Key::RightShift => evdev::Key::KEY_RIGHTSHIFT,
			Key::LeftControl => evdev::Key::KEY_LEFTCTRL,
			Key::RightControl => evdev::Key::KEY_RIGHTCTRL,
			Key::LeftAlt => evdev::Key::KEY_LEFTALT,
			Key::RightAlt => evdev::Key::KEY_RIGHTALT,
			Key::Semicolon => evdev::Key::KEY_SEMICOLON,
			Key::Equal => evdev::Key::KEY_EQUAL,
			Key::Comma => evdev::Key::KEY_COMMA,
			Key::Minus => evdev::Key::KEY_MINUS,
			Key::Dot => evdev::Key::KEY_DOT,
			Key::Slash => evdev::Key::KEY_SLASH,
			Key::Grave => evdev::Key::KEY_GRAVE,
			Key::LeftBrace => evdev::Key::KEY_LEFTBRACE,
			Key::Backslash => evdev::Key::KEY_BACKSLASH,
			Key::RightBrace => evdev::Key::KEY_RIGHTBRACE,
			Key::Apostrophe => evdev::Key::KEY_APOSTROPHE,
			Key::NonUsBackslash => evdev::Key::KEY_102ND,
		}
	}
}

pub struct Keyboard {
	device: VirtualDevice,
}

impl Keyboard {
	pub fn new() -> Result<Self> {
		let mut attributes = AttributeSet::new();
		for key in Key::iter() {
			attributes.insert(key.into());
		}

		let device = VirtualDeviceBuilder::new()
			.context("Failed to initiate virtual keyboard")?
			.name("Moonshine Keyboard")
			.with_keys(&attributes)
			.context("Failed to add keys to virtual keyboard")?
			.build()
			.context("Failed to create virtual keyboard")?;

		Ok(Self { device })
	}

	pub fn key_down(&mut self, key: Key) -> Result<()> {
		let button_event = evdev::InputEvent::new_now(evdev::EventType::KEY, Into::<evdev::Key>::into(key).code(), 1);

		self.device.emit(&[button_event]).context("Failed to press key")
	}

	pub fn key_up(&mut self, button: Key) -> Result<()> {
		let button_event =
			evdev::InputEvent::new_now(evdev::EventType::KEY, Into::<evdev::Key>::into(button).code(), 0);

		self.device.emit(&[button_event]).context("Failed to release key")
	}
}
