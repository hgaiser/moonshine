use evdev::{
	uinput::{
		VirtualDevice,
		VirtualDeviceBuilder
	},
	AttributeSet,
	Key,
	UinputAbsSetup,
	AbsoluteAxisType,
	AbsInfo,
	InputId,
};
use strum::IntoEnumIterator;
use strum_macros::{FromRepr, EnumIter};

#[derive(Debug, FromRepr)]
#[repr(u8)]
enum GamepadKind {
	_Unknown = 0x00,
	_Xbox = 0x01,
	_PlayStation = 0x02,
	_Nintendo = 0x03,
}

#[derive(Copy, Clone, Debug)]
#[repr(u16)]
enum GamepadCapability {
	/// Reports values between 0x00 and 0xFF for trigger axes.
	_AnalogTriggers = 0x01,

	/// Can rumble.
	_Rumble = 0x02,

	/// Can rumble triggers.
	_TriggerRumble = 0x04,

	/// Reports touchpad events.
	_Touchpad = 0x08,

	/// Can report accelerometer events.
	_Acceleration = 0x10,

	/// Can report gyroscope events.
	_Gyro = 0x20,

	/// Reports battery state.
	_BatteryState = 0x40,

	// Can set RGB LED state.
	_RgbLed = 0x80,
}

#[derive(Copy, Clone, Debug, EnumIter, PartialEq)]
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
			GamepadButton::Select => Key::BTN_SELECT,
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
	// kind: GamepadKind,
	// capabilities: u16,
	// supported_buttons: u32,
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
			tracing::warn!("Expected at least {EXPECTED_SIZE} bytes for GamepadInfo, got {} bytes.", buffer.len());
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			// kind: GamepadKind::from_repr(buffer[1]).ok_or_else(|| tracing::warn!("Unknown gamepad kind: {}", buffer[1]))?,
			// capabilities: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			// supported_buttons: u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
		})
	}

	// fn has_capability(&self, capability: &GamepadCapability) -> bool {
	// 	(self.capabilities & *capability as u16) != 0
	// }

	// fn has_button(&self, button: &GamepadButton) -> bool {
	// 	(self.supported_buttons & *button as u32) != 0
	// }
}

#[derive(Debug)]
pub struct GamepadUpdate {
	pub index: u16,
	_active_gamepad_mask: u16,
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
			tracing::warn!("Expected at least {EXPECTED_SIZE} bytes for GamepadUpdate, got {} bytes.", buffer.len());
			return Err(());
		}

		Ok(Self {
			index: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			_active_gamepad_mask: u16::from_le_bytes(buffer[4..6].try_into().unwrap()),
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
	_info: GamepadInfo,
	device: VirtualDevice,
	button_state: u32,
}

impl Gamepad {
	pub fn new(info: GamepadInfo) -> Result<Self, ()> {
		// Ideally we use info.supported_buttons, but this gives unexpected results.
		// For example, the left and right joystick buttons would be mapped to SELECT / START for some reason..
		let buttons = AttributeSet::from_iter([
			evdev::Key::BTN_WEST,
			evdev::Key::BTN_EAST,
			evdev::Key::BTN_NORTH,
			evdev::Key::BTN_SOUTH,
			evdev::Key::BTN_THUMBL,
			evdev::Key::BTN_THUMBR,
			evdev::Key::BTN_TL,
			evdev::Key::BTN_TR,
			evdev::Key::BTN_TL2,
			evdev::Key::BTN_TR2,
			evdev::Key::BTN_START,
			evdev::Key::BTN_SELECT,
			evdev::Key::BTN_MODE,
		]);

		let device = VirtualDeviceBuilder::new()
			.map_err(|e| tracing::error!("Failed to initiate virtual gamepad: {e}"))?
			.input_id(InputId::new(evdev::BusType::BUS_BLUETOOTH, 0x54C, 0x5C4, 0x8100))
			.name(format!("Moonshine Gamepad {}", info.index).as_str())
			.with_keys(&buttons)
			.map_err(|e| tracing::error!("Failed to add keys to virtual gamepad: {e}"))?
			// Dpad.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_HAT0X,
				AbsInfo::new(0, -1, 1, 0, 0, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_HAT0Y,
				AbsInfo::new(0, -1, 1, 0, 0, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			// Left stick.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_X,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_Y,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			// Right stick.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_RX,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_RY,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			// Left trigger.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_Z,
				AbsInfo::new(0, 0, u8::MAX as i32, 0, 0, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			// Right trigger.
			.with_absolute_axis(&UinputAbsSetup::new(
				AbsoluteAxisType::ABS_RZ,
				AbsInfo::new(0, 0, u8::MAX as i32, 0, 0, 0)
			))
			.map_err(|e| tracing::error!("Failed to enable gamepad axis: {e}"))?
			// .with_ff(&AttributeSet::from_iter([
			// 	evdev::FFEffectType::FF_RUMBLE,
			// 	evdev::FFEffectType::FF_PERIODIC,
			// 	evdev::FFEffectType::FF_SQUARE,
			// 	evdev::FFEffectType::FF_TRIANGLE,
			// 	evdev::FFEffectType::FF_SINE,
			// 	evdev::FFEffectType::FF_GAIN,
			// ]))
			// .map_err(|e| tracing::error!("Failed to enable force feedback on virtual gamepad: {e}"))?
			// .with_ff_effects_max(16) // TODO: What should this value be?
			.build()
			.map_err(|e| tracing::error!("Failed to create virtual gamepad: {e}"))?;

		Ok(Self { _info: info, device, button_state: 0 })
	}

	fn button_changed(&self, button: &GamepadButton, new_state: u32) -> bool {
		(self.button_state & *button as u32) != (new_state & *button as u32)
	}

	pub fn update(&mut self, update: GamepadUpdate) -> Result<(), ()> {
		let mut events = Vec::new();

		// Check all buttons that have changed and emit their update.
		for button in GamepadButton::iter() {
			if self.button_changed(&button, update.button_flags) {
				tracing::trace!("Sending update for button {:?}, state: {}", button, (update.button_flags & button as u32) != 0);

				match button {
					GamepadButton::Down | GamepadButton::Up => {
						let state;
						if (update.button_flags & GamepadButton::Up as u32) != 0 {
							state = -1;
						} else if (update.button_flags & GamepadButton::Down as u32) != 0 {
							state = 1;
						} else {
							state = 0;
						}
						events.push(evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_HAT0Y.0, state));
					},
					GamepadButton::Left | GamepadButton::Right => {
						let state;
						if (update.button_flags & GamepadButton::Left as u32) != 0 {
							state = -1;
						} else if (update.button_flags & GamepadButton::Right as u32) != 0 {
							state = 1;
						} else {
							state = 0;
						}
						events.push(evdev::InputEvent::new_now(evdev::EventType::ABSOLUTE, AbsoluteAxisType::ABS_HAT0X.0, state));
					},
					_ => {
						events.push(evdev::InputEvent::new_now(
							evdev::EventType::KEY,
							Into::<Key>::into(button).code(),
							((update.button_flags & button as u32) != 0) as i32,
						));
					}
				}
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
			.map_err(|e| tracing::error!("Failed to send gamepad events: {e}"))
	}
}
