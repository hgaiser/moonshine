use evdev::{uinput::{VirtualDevice, VirtualDeviceBuilder}, AttributeSet, Key, FFEffectType, UinputAbsSetup, AbsoluteAxisType, AbsInfo, InputId};
use strum::IntoEnumIterator;
use strum_macros::{FromRepr, EnumIter};

#[derive(Debug, FromRepr)]
#[repr(u8)]
enum GamepadKind {
	Unknown = 0x00,
	Xbox = 0x01,
	PlayStation = 0x02,
	Nintendo = 0x03,
}

#[derive(Copy, Clone, Debug)]
#[repr(u16)]
enum GamepadCapability {
	/// Reports values between 0x00 and 0xFF for trigger axes.
	AnalogTriggers = 0x01,

	/// Can rumble.
	Rumble = 0x02,

	/// Can rumble triggers.
	TriggerRumble = 0x04,

	/// Reports touchpad events.
	Touchpad = 0x08,

	/// Can report accelerometer events.
	Acceleration = 0x10,

	/// Can report gyroscope events.
	Gyro = 0x20,

	/// Reports battery state.
	BatteryState = 0x40,

	// Can set RGB LED state.
	RgbLed = 0x80,
}

#[derive(Copy, Clone, Debug, EnumIter)]
#[repr(u32)]
enum GamepadButton {
	// Button flags.
	Up              = 0x00000001,
	Down            = 0x00000002,
	Left            = 0x00000004,
	Right           = 0x00000008,
	Start           = 0x00000010,
	Select          = 0x00000020,
	LeftStickClick  = 0x00000040,
	RightStickClick = 0x00000080,
	LB              = 0x00000100,
	RB              = 0x00000200,
	Home            = 0x00000400,
	A               = 0x00001000,
	B               = 0x00002000,
	X               = 0x00004000,
	Y               = 0x00008000,

	// Extended buttons (Sunshine / Moonshine only)
	Paddle1  = 0x00010000,
	Paddle2  = 0x00020000,
	Paddle3  = 0x00040000,
	Paddle4  = 0x00080000,
	Touchpad = 0x00100000, // Touchpad buttons on Sony controllers.
	Misc     = 0x00200000, // Share/Mic/Capture/Mute buttons on various controllers.
}

impl From<GamepadButton> for Key {
	fn from(val: GamepadButton) -> Self {
		match val {
			GamepadButton::Up => Key::BTN_DPAD_UP,
			GamepadButton::Down => Key::BTN_DPAD_DOWN,
			GamepadButton::Left => Key::BTN_DPAD_LEFT,
			GamepadButton::Right => Key::BTN_DPAD_RIGHT,
			GamepadButton::Start => Key::BTN_START,
			GamepadButton::Select => Key::BTN_BACK,
			GamepadButton::LeftStickClick => Key::BTN_THUMBL,
			GamepadButton::RightStickClick => Key::BTN_THUMBR,
			GamepadButton::LB => Key::BTN_TL,
			GamepadButton::RB => Key::BTN_TR,
			GamepadButton::Home => Key::BTN_MODE,
			GamepadButton::A => Key::BTN_SOUTH,
			GamepadButton::B => Key::BTN_EAST,
			GamepadButton::X => Key::BTN_WEST,
			GamepadButton::Y => Key::BTN_NORTH,
			GamepadButton::Paddle1 => Key::BTN_DPAD_DOWN, // TODO
			GamepadButton::Paddle2 => Key::BTN_DPAD_DOWN, // TODO
			GamepadButton::Paddle3 => Key::BTN_DPAD_DOWN, // TODO
			GamepadButton::Paddle4 => Key::BTN_DPAD_DOWN, // TODO
			GamepadButton::Touchpad => Key::BTN_TOUCH,
			GamepadButton::Misc => Key::BTN_DPAD_DOWN, // TODO
		}
	}
}

#[derive(Debug)]
pub struct GamepadInfo {
	index: u8,
	kind: GamepadKind,
	capabilities: u16,
	supported_buttons: u32,
}

impl GamepadInfo {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u8>()    // index
			+ std::mem::size_of::<u8>()  // kind
			+ std::mem::size_of::<u16>() // capabilities
			+ std::mem::size_of::<u32>() // supported_buttons
		;

		if buffer.len() < EXPECTED_SIZE {
			log::warn!("Expected at least {EXPECTED_SIZE} bytes for GamepadInfo, got {} bytes.", buffer.len());
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			kind: GamepadKind::from_repr(buffer[1]).ok_or_else(|| log::warn!("Unknown gamepad kind: {}", buffer[1]))?,
			capabilities: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			supported_buttons: u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
		})
	}

	fn has_capability(&self, capability: &GamepadCapability) -> bool {
		(self.capabilities & *capability as u16) != 0
	}

	fn has_button(&self, button: &GamepadButton) -> bool {
		(self.supported_buttons & *button as u32) != 0
	}
}

#[derive(Debug)]
pub struct GamepadUpdate {
	pub index: u16,
	active_gamepad_mask: u16,
	button_flags: u32,
	left_trigger: u8,
	right_trigger: u8,
	left_stick: (i16, i16),
	right_stick: (i16, i16),
}

impl GamepadUpdate {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u16>()   // header
			+ std::mem::size_of::<u16>() // index
			+ std::mem::size_of::<u16>() // active gamepad mask
			+ std::mem::size_of::<u16>() // mid B
			+ std::mem::size_of::<u16>() // button flags
			+ std::mem::size_of::<u8>()  // left trigger
			+ std::mem::size_of::<u8>()  // right trigger
			+ std::mem::size_of::<i16>() // left stick x
			+ std::mem::size_of::<i16>() // left stick y
			+ std::mem::size_of::<i16>() // right stick x
			+ std::mem::size_of::<i16>() // right stick y
			+ std::mem::size_of::<i16>() // tail a
			+ std::mem::size_of::<i16>() // button flags 2
			+ std::mem::size_of::<i16>() // tail b
		;

		if buffer.len() < EXPECTED_SIZE {
			log::warn!("Expected at least {EXPECTED_SIZE} bytes for GamepadUpdate, got {} bytes.", buffer.len());
			return Err(());
		}

		Ok(Self {
			index: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			active_gamepad_mask: u16::from_le_bytes(buffer[4..6].try_into().unwrap()),
			button_flags: u16::from_le_bytes(buffer[8..10].try_into().unwrap()) as u32 | (u16::from_le_bytes(buffer[22..24].try_into().unwrap()) as u32) << 16,
			left_trigger: buffer[10],
			right_trigger: buffer[11],
			left_stick: (
				i16::from_le_bytes(buffer[12..14].try_into().unwrap()),
				i16::from_le_bytes(buffer[14..16].try_into().unwrap()),
			),
			right_stick: (
				i16::from_le_bytes(buffer[16..18].try_into().unwrap()),
				i16::from_le_bytes(buffer[18..20].try_into().unwrap())
			),
		})
	}
}

pub struct Gamepad {
	info: GamepadInfo,
	device: VirtualDevice,
	button_state: u32,
}

impl Gamepad {
	pub fn new(info: GamepadInfo) -> Result<Self, ()> {
		let mut buttons = AttributeSet::new();
		for button in GamepadButton::iter() {
			if info.has_button(&button) {
				buttons.insert(button.into());
			}
		}

		let device = VirtualDeviceBuilder::new()
			.map_err(|e| log::error!("Failed to initiate virtual gamepad: {e}"))?
			.input_id(InputId::new(evdev::BusType::BUS_USB, 0x45E, 0x28E, 0x110))
			.name(format!("Moonshine Gamepad {}", info.index).as_str())
			.with_keys(&buttons)
			.map_err(|e| log::error!("Failed to add keys to virtual gamepad: {e}"))?
			// Dpad.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_HAT0X,
				AbsInfo::new(0, -1, 1, 0, 0, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_HAT0Y,
				AbsInfo::new(0, -1, 1, 0, 0, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			// Left stick.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_X,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_Y,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			// Right stick.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_RX,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_RY,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			// Left trigger.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_Z,
				AbsInfo::new(0, 0, u8::MAX as i32, 0, 0, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			// Right trigger.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_RZ,
				AbsInfo::new(0, 0, u8::MAX as i32, 0, 0, 0)
			))
			.map_err(|e| log::error!("Failed to enable gamepad axis: {e}"))?
			// .with_ff(&AttributeSet::from_iter([
			// 	evdev::FFEffectType::FF_RUMBLE,
			// 	evdev::FFEffectType::FF_CONSTANT,
			// 	evdev::FFEffectType::FF_PERIODIC,
			// 	evdev::FFEffectType::FF_SINE,
			// 	evdev::FFEffectType::FF_RAMP,
			// 	evdev::FFEffectType::FF_GAIN,
			// ]))
			// .map_err(|e| log::error!("Failed to enable force feedback on virtual gamepad: {e}"))?
			.build()
			.map_err(|e| log::error!("Failed to create virtual gamepad: {e}"))?;

		Ok(Self { info, device, button_state: 0 })
	}

	fn button_changed(&self, button: &GamepadButton, new_state: u32) -> bool {
		(self.button_state & *button as u32) != (new_state & *button as u32)
	}

	pub fn update(&mut self, update: GamepadUpdate) -> Result<(), ()> {
		let mut events = Vec::new();

		// Check all buttons that have changed and emit their update.
		for button in GamepadButton::iter() {
			if self.button_changed(&button, update.button_flags) {
				log::info!("Sending update for button {:?}, state: {}", button, (update.button_flags & button as u32) != 0);
				events.push(evdev::InputEvent::new_now(
					evdev::EventType::KEY,
					Into::<Key>::into(button).code(),
					((update.button_flags & button as u32) != 0) as i32,
				));
			}
		}
		self.button_state = update.button_flags;

		// Send analog triggers.
		events.extend([
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, update.left_stick.0 as i32),
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, -update.left_stick.1 as i32),
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_RX.0, update.right_stick.0 as i32),
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_RY.0, -update.right_stick.1 as i32),
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_Z.0, update.left_trigger as i32),
			evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_RZ.0, update.right_trigger as i32),
		]);

		self.device.emit(&events)
			.map_err(|e| log::error!("Failed to send gamepad events: {e}"))
	}
}