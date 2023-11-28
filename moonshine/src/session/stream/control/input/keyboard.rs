use evdev::{uinput::{VirtualDevice, VirtualDeviceBuilder}, AttributeSet};
use strum::IntoEnumIterator;
use strum_macros::{FromRepr, EnumIter};

// __CONVERT(0x08 /* VKEY_BACK */, KEY_BACKSPACE, 0x7002A, XK_BackSpace);
// __CONVERT(0x09 /* VKEY_TAB */, KEY_TAB, 0x7002B, XK_Tab);
// __CONVERT(0x0C /* VKEY_CLEAR */, KEY_CLEAR, UNKNOWN, XK_Clear);
// __CONVERT(0x0D /* VKEY_RETURN */, KEY_ENTER, 0x70028, XK_Return);
// __CONVERT(0x10 /* VKEY_SHIFT */, KEY_LEFTSHIFT, 0x700E1, XK_Shift_L);
// __CONVERT(0x11 /* VKEY_CONTROL */, KEY_LEFTCTRL, 0x700E0, XK_Control_L);
// __CONVERT(0x12 /* VKEY_MENU */, KEY_LEFTALT, UNKNOWN, XK_Alt_L);
// __CONVERT(0x13 /* VKEY_PAUSE */, KEY_PAUSE, UNKNOWN, XK_Pause);
// __CONVERT(0x14 /* VKEY_CAPITAL */, KEY_CAPSLOCK, 0x70039, XK_Caps_Lock);
// __CONVERT(0x15 /* VKEY_KANA */, KEY_KATAKANAHIRAGANA, UNKNOWN, XK_Kana_Shift);
// __CONVERT(0x16 /* VKEY_HANGUL */, KEY_HANGEUL, UNKNOWN, XK_Hangul);
// __CONVERT(0x17 /* VKEY_JUNJA */, KEY_HANJA, UNKNOWN, XK_Hangul_Jeonja);
// __CONVERT(0x19 /* VKEY_KANJI */, KEY_KATAKANA, UNKNOWN, XK_Kanji);
// __CONVERT(0x1B /* VKEY_ESCAPE */, KEY_ESC, 0x70029, XK_Escape);
// __CONVERT(0x20 /* VKEY_SPACE */, KEY_SPACE, 0x7002C, XK_space);
// __CONVERT(0x21 /* VKEY_PRIOR */, KEY_PAGEUP, 0x7004B, XK_Page_Up);
// __CONVERT(0x22 /* VKEY_NEXT */, KEY_PAGEDOWN, 0x7004E, XK_Page_Down);
// __CONVERT(0x23 /* VKEY_END */, KEY_END, 0x7004D, XK_End);
// __CONVERT(0x24 /* VKEY_HOME */, KEY_HOME, 0x7004A, XK_Home);
// __CONVERT(0x25 /* VKEY_LEFT */, KEY_LEFT, 0x70050, XK_Left);
// __CONVERT(0x26 /* VKEY_UP */, KEY_UP, 0x70052, XK_Up);
// __CONVERT(0x27 /* VKEY_RIGHT */, KEY_RIGHT, 0x7004F, XK_Right);
// __CONVERT(0x28 /* VKEY_DOWN */, KEY_DOWN, 0x70051, XK_Down);
// __CONVERT(0x29 /* VKEY_SELECT */, KEY_SELECT, UNKNOWN, XK_Select);
// __CONVERT(0x2A /* VKEY_PRINT */, KEY_PRINT, UNKNOWN, XK_Print);
// __CONVERT(0x2C /* VKEY_SNAPSHOT */, KEY_SYSRQ, 0x70046, XK_Sys_Req);
// __CONVERT(0x2D /* VKEY_INSERT */, KEY_INSERT, 0x70049, XK_Insert);
// __CONVERT(0x2E /* VKEY_DELETE */, KEY_DELETE, 0x7004C, XK_Delete);
// __CONVERT(0x2F /* VKEY_HELP */, KEY_HELP, UNKNOWN, XK_Help);
// __CONVERT(0x30 /* VKEY_0 */, KEY_0, 0x70027, XK_0);
// __CONVERT(0x31 /* VKEY_1 */, KEY_1, 0x7001E, XK_1);
// __CONVERT(0x32 /* VKEY_2 */, KEY_2, 0x7001F, XK_2);
// __CONVERT(0x33 /* VKEY_3 */, KEY_3, 0x70020, XK_3);
// __CONVERT(0x34 /* VKEY_4 */, KEY_4, 0x70021, XK_4);
// __CONVERT(0x35 /* VKEY_5 */, KEY_5, 0x70022, XK_5);
// __CONVERT(0x36 /* VKEY_6 */, KEY_6, 0x70023, XK_6);
// __CONVERT(0x37 /* VKEY_7 */, KEY_7, 0x70024, XK_7);
// __CONVERT(0x38 /* VKEY_8 */, KEY_8, 0x70025, XK_8);
// __CONVERT(0x39 /* VKEY_9 */, KEY_9, 0x70026, XK_9);
// __CONVERT(0x41 /* VKEY_A */, KEY_A, 0x70004, XK_A);
// __CONVERT(0x42 /* VKEY_B */, KEY_B, 0x70005, XK_B);
// __CONVERT(0x43 /* VKEY_C */, KEY_C, 0x70006, XK_C);
// __CONVERT(0x44 /* VKEY_D */, KEY_D, 0x70007, XK_D);
// __CONVERT(0x45 /* VKEY_E */, KEY_E, 0x70008, XK_E);
// __CONVERT(0x46 /* VKEY_F */, KEY_F, 0x70009, XK_F);
// __CONVERT(0x47 /* VKEY_G */, KEY_G, 0x7000A, XK_G);
// __CONVERT(0x48 /* VKEY_H */, KEY_H, 0x7000B, XK_H);
// __CONVERT(0x49 /* VKEY_I */, KEY_I, 0x7000C, XK_I);
// __CONVERT(0x4A /* VKEY_J */, KEY_J, 0x7000D, XK_J);
// __CONVERT(0x4B /* VKEY_K */, KEY_K, 0x7000E, XK_K);
// __CONVERT(0x4C /* VKEY_L */, KEY_L, 0x7000F, XK_L);
// __CONVERT(0x4D /* VKEY_M */, KEY_M, 0x70010, XK_M);
// __CONVERT(0x4E /* VKEY_N */, KEY_N, 0x70011, XK_N);
// __CONVERT(0x4F /* VKEY_O */, KEY_O, 0x70012, XK_O);
// __CONVERT(0x50 /* VKEY_P */, KEY_P, 0x70013, XK_P);
// __CONVERT(0x51 /* VKEY_Q */, KEY_Q, 0x70014, XK_Q);
// __CONVERT(0x52 /* VKEY_R */, KEY_R, 0x70015, XK_R);
// __CONVERT(0x53 /* VKEY_S */, KEY_S, 0x70016, XK_S);
// __CONVERT(0x54 /* VKEY_T */, KEY_T, 0x70017, XK_T);
// __CONVERT(0x55 /* VKEY_U */, KEY_U, 0x70018, XK_U);
// __CONVERT(0x56 /* VKEY_V */, KEY_V, 0x70019, XK_V);
// __CONVERT(0x57 /* VKEY_W */, KEY_W, 0x7001A, XK_W);
// __CONVERT(0x58 /* VKEY_X */, KEY_X, 0x7001B, XK_X);
// __CONVERT(0x59 /* VKEY_Y */, KEY_Y, 0x7001C, XK_Y);
// __CONVERT(0x5A /* VKEY_Z */, KEY_Z, 0x7001D, XK_Z);
// __CONVERT(0x5B /* VKEY_LWIN */, KEY_LEFTMETA, 0x700E3, XK_Meta_L);
// __CONVERT(0x5C /* VKEY_RWIN */, KEY_RIGHTMETA, 0x700E7, XK_Meta_R);
// __CONVERT(0x5F /* VKEY_SLEEP */, KEY_SLEEP, UNKNOWN, UNKNOWN);
// __CONVERT(0x60 /* VKEY_NUMPAD0 */, KEY_KP0, 0x70062, XK_KP_0);
// __CONVERT(0x61 /* VKEY_NUMPAD1 */, KEY_KP1, 0x70059, XK_KP_1);
// __CONVERT(0x62 /* VKEY_NUMPAD2 */, KEY_KP2, 0x7005A, XK_KP_2);
// __CONVERT(0x63 /* VKEY_NUMPAD3 */, KEY_KP3, 0x7005B, XK_KP_3);
// __CONVERT(0x64 /* VKEY_NUMPAD4 */, KEY_KP4, 0x7005C, XK_KP_4);
// __CONVERT(0x65 /* VKEY_NUMPAD5 */, KEY_KP5, 0x7005D, XK_KP_5);
// __CONVERT(0x66 /* VKEY_NUMPAD6 */, KEY_KP6, 0x7005E, XK_KP_6);
// __CONVERT(0x67 /* VKEY_NUMPAD7 */, KEY_KP7, 0x7005F, XK_KP_7);
// __CONVERT(0x68 /* VKEY_NUMPAD8 */, KEY_KP8, 0x70060, XK_KP_8);
// __CONVERT(0x69 /* VKEY_NUMPAD9 */, KEY_KP9, 0x70061, XK_KP_9);
// __CONVERT(0x6A /* VKEY_MULTIPLY */, KEY_KPASTERISK, 0x70055, XK_KP_Multiply);
// __CONVERT(0x6B /* VKEY_ADD */, KEY_KPPLUS, 0x70057, XK_KP_Add);
// __CONVERT(0x6C /* VKEY_SEPARATOR */, KEY_KPCOMMA, UNKNOWN, XK_KP_Separator);
// __CONVERT(0x6D /* VKEY_SUBTRACT */, KEY_KPMINUS, 0x70056, XK_KP_Subtract);
// __CONVERT(0x6E /* VKEY_DECIMAL */, KEY_KPDOT, 0x70063, XK_KP_Decimal);
// __CONVERT(0x6F /* VKEY_DIVIDE */, KEY_KPSLASH, 0x70054, XK_KP_Divide);
// __CONVERT(0x70 /* VKEY_F1 */, KEY_F1, 0x70046, XK_F1);
// __CONVERT(0x71 /* VKEY_F2 */, KEY_F2, 0x70047, XK_F2);
// __CONVERT(0x72 /* VKEY_F3 */, KEY_F3, 0x70048, XK_F3);
// __CONVERT(0x73 /* VKEY_F4 */, KEY_F4, 0x70049, XK_F4);
// __CONVERT(0x74 /* VKEY_F5 */, KEY_F5, 0x7004a, XK_F5);
// __CONVERT(0x75 /* VKEY_F6 */, KEY_F6, 0x7004b, XK_F6);
// __CONVERT(0x76 /* VKEY_F7 */, KEY_F7, 0x7004c, XK_F7);
// __CONVERT(0x77 /* VKEY_F8 */, KEY_F8, 0x7004d, XK_F8);
// __CONVERT(0x78 /* VKEY_F9 */, KEY_F9, 0x7004e, XK_F9);
// __CONVERT(0x79 /* VKEY_F10 */, KEY_F10, 0x70044, XK_F10);
// __CONVERT(0x7A /* VKEY_F11 */, KEY_F11, 0x70044, XK_F11);
// __CONVERT(0x7B /* VKEY_F12 */, KEY_F12, 0x70045, XK_F12);
// __CONVERT(0x7C /* VKEY_F13 */, KEY_F13, 0x7003a, XK_F13);
// __CONVERT(0x7D /* VKEY_F14 */, KEY_F14, 0x7003b, XK_F14);
// __CONVERT(0x7E /* VKEY_F15 */, KEY_F15, 0x7003c, XK_F15);
// __CONVERT(0x7F /* VKEY_F16 */, KEY_F16, 0x7003d, XK_F16);
// __CONVERT(0x80 /* VKEY_F17 */, KEY_F17, 0x7003e, XK_F17);
// __CONVERT(0x81 /* VKEY_F18 */, KEY_F18, 0x7003f, XK_F18);
// __CONVERT(0x82 /* VKEY_F19 */, KEY_F19, 0x70040, XK_F19);
// __CONVERT(0x83 /* VKEY_F20 */, KEY_F20, 0x70041, XK_F20);
// __CONVERT(0x84 /* VKEY_F21 */, KEY_F21, 0x70042, XK_F21);
// __CONVERT(0x85 /* VKEY_F22 */, KEY_F12, 0x70043, XK_F12);
// __CONVERT(0x86 /* VKEY_F23 */, KEY_F23, 0x70044, XK_F23);
// __CONVERT(0x87 /* VKEY_F24 */, KEY_F24, 0x70045, XK_F24);
// __CONVERT(0x90 /* VKEY_NUMLOCK */, KEY_NUMLOCK, 0x70053, XK_Num_Lock);
// __CONVERT(0x91 /* VKEY_SCROLL */, KEY_SCROLLLOCK, 0x70047, XK_Scroll_Lock);
// __CONVERT(0xA0 /* VKEY_LSHIFT */, KEY_LEFTSHIFT, 0x700E1, XK_Shift_L);
// __CONVERT(0xA1 /* VKEY_RSHIFT */, KEY_RIGHTSHIFT, 0x700E5, XK_Shift_R);
// __CONVERT(0xA2 /* VKEY_LCONTROL */, KEY_LEFTCTRL, 0x700E0, XK_Control_L);
// __CONVERT(0xA3 /* VKEY_RCONTROL */, KEY_RIGHTCTRL, 0x700E4, XK_Control_R);
// __CONVERT(0xA4 /* VKEY_LMENU */, KEY_LEFTALT, 0x7002E, XK_Alt_L);
// __CONVERT(0xA5 /* VKEY_RMENU */, KEY_RIGHTALT, 0x700E6, XK_Alt_R);
// __CONVERT(0xBA /* VKEY_OEM_1 */, KEY_SEMICOLON, 0x70033, XK_semicolon);
// __CONVERT(0xBB /* VKEY_OEM_PLUS */, KEY_EQUAL, 0x7002E, XK_equal);
// __CONVERT(0xBC /* VKEY_OEM_COMMA */, KEY_COMMA, 0x70036, XK_comma);
// __CONVERT(0xBD /* VKEY_OEM_MINUS */, KEY_MINUS, 0x7002D, XK_minus);
// __CONVERT(0xBE /* VKEY_OEM_PERIOD */, KEY_DOT, 0x70037, XK_period);
// __CONVERT(0xBF /* VKEY_OEM_2 */, KEY_SLASH, 0x70038, XK_slash);
// __CONVERT(0xC0 /* VKEY_OEM_3 */, KEY_GRAVE, 0x70035, XK_grave);
// __CONVERT(0xDB /* VKEY_OEM_4 */, KEY_LEFTBRACE, 0x7002F, XK_braceleft);
// __CONVERT(0xDC /* VKEY_OEM_5 */, KEY_BACKSLASH, 0x70031, XK_backslash);
// __CONVERT(0xDD /* VKEY_OEM_6 */, KEY_RIGHTBRACE, 0x70030, XK_braceright);
// __CONVERT(0xDE /* VKEY_OEM_7 */, KEY_APOSTROPHE, 0x70034, XK_apostrophe);
// __CONVERT(0xE2 /* VKEY_NON_US_BACKSLASH */, KEY_102ND, 0x70064, XK_backslash);

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
	pub fn new() -> Result<Self, ()> {
		let mut attributes = AttributeSet::new();
		for key in Key::iter() {
			attributes.insert(key.into());
		}

		let device = VirtualDeviceBuilder::new()
			.map_err(|e| log::error!("Failed to initiate virtual keyboard: {e}"))?
			.name("moonshine-keyboard")
			.with_keys(&attributes)
			.map_err(|e| log::error!("Failed to add keys to virtual keyboard: {e}"))?
			.build()
			.map_err(|e| log::error!("Failed to create virtual keyboard: {e}"))?;

		Ok(Self { device })
	}

	pub fn key_down(&mut self, key: Key) -> Result<(), ()> {
		let button_event = evdev::InputEvent::new_now(
			evdev::EventType::KEY,
			Into::<evdev::Key>::into(key).code(),
			1
		);

		self.device.emit(&[button_event])
			.map_err(|e| log::error!("Failed to press key: {e}"))
	}

	pub fn key_up(&mut self, button: Key) -> Result<(), ()> {
		let button_event = evdev::InputEvent::new_now(
			evdev::EventType::KEY,
			Into::<evdev::Key>::into(button).code(),
			0
		);

		self.device.emit(&[button_event])
			.map_err(|e| log::error!("Failed to release key: {e}"))
	}
}