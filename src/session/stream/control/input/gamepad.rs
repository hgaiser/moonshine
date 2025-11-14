use inputtino::{
	BatteryState as InputtinoBatterState,
	DeviceDefinition,
	Joypad,
	JoypadMotionType,
	JoypadStickPosition,
	PS5Joypad,
	SwitchJoypad,
	XboxOneJoypad
};
use strum_macros::FromRepr;
use tokio::sync::mpsc;

use crate::session::stream::control::{feedback::{EnableMotionEventCommand, RumbleCommand, SetLedCommand, TriggerEffectCommand}, FeedbackCommand};

#[derive(Debug, FromRepr)]
#[repr(u8)]
pub enum GamepadKind {
	Unknown = 0x00,
	Xbox = 0x01,
	PlayStation = 0x02,
	Nintendo = 0x03,
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

#[derive(Debug)]
pub struct GamepadInfo {
	pub index: u8,
	kind: GamepadKind,
	capabilities: u16,
	_supported_buttons: u32,
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
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for GamepadInfo, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			kind: GamepadKind::from_repr(buffer[1])
				.ok_or_else(|| tracing::warn!("Unknown gamepad kind: {}", buffer[1]))?,
			capabilities: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			_supported_buttons: u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
		})
	}

	#[allow(dead_code)]
	fn has_capability(&self, capability: &GamepadCapability) -> bool {
		(self.capabilities & *capability as u16) != 0
	}
}

#[derive(Debug)]
pub struct GamepadTouch {
	pub index: u8,
	_event_type: u8,
	// zero: [u8; 2], // Alignment/reserved
	pointer_id: u32,
	pub x: f32,
	pub y: f32,
	pub pressure: f32,
}

impl GamepadTouch {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u8>()    // index
			+ std::mem::size_of::<u8>()  // event_type
			+ std::mem::size_of::<u16>() // zero
			+ std::mem::size_of::<u32>() // pointer_id
			+ std::mem::size_of::<f32>() // x
			+ std::mem::size_of::<f32>() // y
			+ std::mem::size_of::<f32>() // pressure
		;

		if buffer.len() < EXPECTED_SIZE {
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for GamepadTouch, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			_event_type: buffer[1],
			// zero: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			pointer_id: u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
			x: f32::from_le_bytes(buffer[8..12].try_into().unwrap()).clamp(0.0, 1.0),
			y: f32::from_le_bytes(buffer[12..16].try_into().unwrap()).clamp(0.0, 1.0),
			pressure: f32::from_le_bytes(buffer[16..20].try_into().unwrap()).clamp(0.0, 1.0),
		})
	}
}

#[derive(Debug)]
pub struct GamepadUpdate {
	pub index: u16,
	pub active_gamepad_mask: u16,
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
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for GamepadUpdate, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			index: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			active_gamepad_mask: u16::from_le_bytes(buffer[4..6].try_into().unwrap()),
			button_flags: u16::from_le_bytes(buffer[8..10].try_into().unwrap()) as u32
				| (u16::from_le_bytes(buffer[22..24].try_into().unwrap()) as u32) << 16,
			left_trigger: buffer[10],
			right_trigger: buffer[11],
			left_stick: (
				i16::from_le_bytes(buffer[12..14].try_into().unwrap()),
				i16::from_le_bytes(buffer[14..16].try_into().unwrap()),
			),
			right_stick: (
				i16::from_le_bytes(buffer[16..18].try_into().unwrap()),
				i16::from_le_bytes(buffer[18..20].try_into().unwrap()),
			),
		})
	}
}

#[derive(Debug)]
pub struct GamepadMotion {
	pub index: u8,
	motion_type: JoypadMotionType,
	// zero: [u8; 2], // Alignment/reserved
	x: f32,
	y: f32,
	z: f32,
}

impl GamepadMotion {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u8>() // index
			+ std::mem::size_of::<u8>() // motion type
			+ std::mem::size_of::<u16>() // alignment/reserved
			+ std::mem::size_of::<f32>() // x
			+ std::mem::size_of::<f32>() // y
			+ std::mem::size_of::<f32>() // z
		;

		if buffer.len() < EXPECTED_SIZE {
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for GamepadMotion, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			motion_type: match buffer[1] {
				1 => JoypadMotionType::ACCELERATION,
				2 => JoypadMotionType::GYROSCOPE,
				_ => {
					tracing::warn!("Unknown gamepad motion type: {}", buffer[1]);
					return Err(());
				},
			},
			// zero: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			x: f32::from_le_bytes(buffer[4..8].try_into().unwrap()),
			y: f32::from_le_bytes(buffer[8..12].try_into().unwrap()),
			z: f32::from_le_bytes(buffer[12..16].try_into().unwrap()),
		})
	}
}

#[derive(Debug, FromRepr)]
#[repr(u8)]
enum BatteryState {
	Unknown = 0x00,
	NotPresent = 0x01,
	Discharging = 0x02,
	Charging = 0x03,
	NotCharging = 0x04,
	Full = 0x05,
	PercentageUnknown = 0xFF,
}

#[derive(Debug)]
pub struct GamepadBattery {
	pub index: u8,
	battery_state: BatteryState,
	battery_percentage: u8,
}

impl GamepadBattery {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		const EXPECTED_SIZE: usize =
			std::mem::size_of::<u8>() // index
			+ std::mem::size_of::<u8>() // battery state
			+ std::mem::size_of::<u8>() // battery percentage
			+ std::mem::size_of::<u8>() // padding
		;

		if buffer.len() < EXPECTED_SIZE {
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for GamepadBattery, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			battery_state: BatteryState::from_repr(buffer[1])
				.ok_or_else(|| tracing::warn!("Unknown battery state: {}", buffer[1]))?,
			battery_percentage: buffer[2],
		})
	}
}

pub struct Gamepad {
	gamepad: inputtino::Joypad,
}

impl Gamepad {
	pub async fn new(info: &GamepadInfo, feedback_tx: mpsc::Sender<FeedbackCommand>) -> Result<Self, ()> {
		let definition = match info.kind {
			GamepadKind::Unknown | GamepadKind::Xbox => DeviceDefinition::new(
				"Moonshine XOne controller",
				0x045e,
				0x02dd,
				0x0100,
				"00:11:22:33:44",
				"00:11:22:33:44",
			),
			GamepadKind::PlayStation => DeviceDefinition::new(
				"Moonshine PS5 controller",
				0x054C,
				0x0CE6,
				0x8111,
				"00:11:22:33:44",
				"00:11:22:33:44",
			),
			GamepadKind::Nintendo => DeviceDefinition::new(
				"Moonshine Switch controller",
				0x057e,
				0x2009,
				0x8111,
				"00:11:22:33:44",
				"00:11:22:33:44",
			),
		};

		let mut gamepad = match info.kind {
			GamepadKind::Unknown | GamepadKind::Xbox => Joypad::XboxOne(
				XboxOneJoypad::new(&definition).map_err(|e| tracing::error!("Failed to create gamepad: {e}"))?,
			),
			GamepadKind::PlayStation => {
				let mut gamepad = PS5Joypad::new(&definition)
					.map_err(|e| tracing::error!("Failed to create gamepad: {e}"))?;

				gamepad.set_on_led({
					let feedback_tx = feedback_tx.clone();
					let index = info.index;
					move |r, g, b| {
						let _ = feedback_tx.blocking_send(FeedbackCommand::SetLed(SetLedCommand {
							id: index as u16,
							rgb: (r as u8, g as u8, b as u8),
						}));
					}}
				);

				gamepad.set_on_trigger_effect({
					let feedback_tx = feedback_tx.clone();
					let index = info.index;
					move |trigger_event_flags, type_left, type_right, left, right| {
						let left: &[u8; 10] = if let Ok(left) = left.try_into() {
							left
						} else {
							tracing::warn!("Couldn't convert left trigger effect.");
							return;
						};

						let right: &[u8; 10] = if let Ok(right) = right.try_into() {
							right
						} else {
							tracing::warn!("Couldn't convert right trigger effect.");
							return;
						};

						// tracing::info!("Trigger effect: {:?} {:?} {:?} {:?}", type_left, type_right, left, right);

						let _ = feedback_tx.blocking_send(FeedbackCommand::TriggerEffect(TriggerEffectCommand {
							id: index as u16,
							trigger_event_flags,
							type_left,
							type_right,
							left: left.to_owned(),
							right: right.to_owned(),
						}));
					}}
				);

				// Enable gyro and accelerometer events.
				let _ = feedback_tx.send(FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
					id: info.index as u16,
					report_rate: 100,
					motion_type: JoypadMotionType::ACCELERATION as u8,
				})).await;
				let _ = feedback_tx.send(FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
					id: info.index as u16,
					report_rate: 100,
					motion_type: JoypadMotionType::GYROSCOPE as u8,
				})).await;

				Joypad::PS5(gamepad)
			}
			GamepadKind::Nintendo => Joypad::Switch(
				SwitchJoypad::new(&definition).map_err(|e| tracing::error!("Failed to create gamepad: {e}"))?,
			),
		};

		gamepad.set_on_rumble({
			let index = info.index;
			move |low_frequency, high_frequency| {
				let _ = feedback_tx.blocking_send(FeedbackCommand::Rumble(RumbleCommand {
					id: index as u16,
					low_frequency: low_frequency as u16,
					high_frequency: high_frequency as u16,
				}));
			}
		});

		Ok(Self { gamepad })
	}

	pub fn update(&mut self, update: &GamepadUpdate) {
		// Send button state.
		self.gamepad.set_pressed(update.button_flags as i32);

		// Send analog triggers.
		self.gamepad.set_stick(JoypadStickPosition::LS, update.left_stick.0, update.left_stick.1);
		self.gamepad.set_stick(JoypadStickPosition::RS, update.right_stick.0, update.right_stick.1);
		self.gamepad.set_triggers(update.left_trigger as i16, update.right_trigger as i16);
	}

	pub fn touch(&mut self, touch: &GamepadTouch) {
		if let Joypad::PS5(gamepad) = &self.gamepad {
			if touch.pressure > 0.5 {
				gamepad.place_finger(
					touch.pointer_id,
					(touch.x * PS5Joypad::TOUCHPAD_WIDTH as f32) as u16,
					(touch.y * PS5Joypad::TOUCHPAD_HEIGHT as f32) as u16,
				);
			} else {
				gamepad.release_finger(touch.pointer_id);
			}
		}
	}

	pub fn set_motion(&self, motion: &GamepadMotion) {
		if let Joypad::PS5(gamepad) = &self.gamepad {
			gamepad.set_motion(motion.motion_type, motion.x.to_radians(), motion.y.to_radians(), motion.z.to_radians());
		}
	}

	pub fn set_battery(&self, gamepad_battery: &GamepadBattery) {
		if let Joypad::PS5(gamepad) = &self.gamepad {
			let state = match gamepad_battery.battery_state {
				BatteryState::Discharging => InputtinoBatterState::BATTERY_DISCHARGING,
				BatteryState::Charging => InputtinoBatterState::BATTERY_CHARGHING,
				BatteryState::Full => InputtinoBatterState::BATTERY_FULL,
				BatteryState::NotPresent => return,
				BatteryState::NotCharging => return,
				BatteryState::Unknown => return,
				_ => {
					tracing::warn!("Unknown battery state: {:?}", gamepad_battery.battery_state);
					return;
				}
			};

			gamepad.set_battery(state, gamepad_battery.battery_percentage);
		}
	}
}
