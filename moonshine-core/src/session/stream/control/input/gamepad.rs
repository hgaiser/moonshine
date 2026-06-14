use std::time::Instant;

use inputtino::{
	BatteryState as InputtinoBatterState, DeviceDefinition, Joypad, JoypadMotionType, JoypadStickPosition, PS5Joypad,
	SwitchJoypad, XboxOneJoypad,
};
use serde::{Deserialize, Serialize};
use strum_macros::FromRepr;
use tokio::sync::mpsc;

use crate::session::stream::control::{
	feedback::{EnableMotionEventCommand, RumbleCommand, SetLedCommand, TriggerEffectCommand},
	FeedbackCommand,
};

use super::remap::{HoldToHome, HoldTransition};

/// Configuration for the hold-to-Home gamepad button remap.
///
/// When enabled, holding the Back/Select button for `hold_ms` emits the
/// Home/Guide button instead. A short tap (released early) still sends Back.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HomeButtonConfig {
	/// How long (in milliseconds) the Back button must be held before the
	/// Home/Guide button is emitted instead. While held, the Back button is
	/// withheld; a short tap (released before this duration) still emits Back.
	/// Set to 0 to disable the remap entirely (the default).
	pub hold_ms: u64,

	/// Duration (in milliseconds) of the tactile rumble pulse fired when
	/// hold-to-Home activates. Set to 0 to disable the rumble pulse.
	pub rumble_duration_ms: u64,

	/// Rumble intensity for the hold-to-Home activation pulse (0.0-1.0).
	/// 0.0 means no rumble; 1.0 is maximum intensity.
	pub rumble_intensity: f64,
}

impl Default for HomeButtonConfig {
	fn default() -> Self {
		Self {
			hold_ms: 0,
			rumble_duration_ms: 50,
			rumble_intensity: 0.5,
		}
	}
}

/// Configuration for gamepad input handling.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GamepadConfig {
	/// Configuration for the hold-to-Home button remap.
	pub home_button: HomeButtonConfig,
}

#[derive(Debug, FromRepr)]
#[repr(u8)]
pub(crate) enum GamepadKind {
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
pub(crate) struct GamepadInfo {
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

	pub fn default_for_index(index: u8) -> Self {
		Self {
			index,
			kind: GamepadKind::Unknown,
			capabilities: 0,
			_supported_buttons: 0,
		}
	}

	#[allow(dead_code)]
	fn has_capability(&self, capability: &GamepadCapability) -> bool {
		(self.capabilities & *capability as u16) != 0
	}
}

#[derive(Debug)]
pub(crate) struct GamepadTouch {
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
pub(crate) struct GamepadUpdate {
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
pub(crate) struct GamepadMotion {
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
pub(crate) struct GamepadBattery {
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

pub(crate) struct Gamepad {
	/// The underlying inputtino joypad, used to inject button presses, stick
	/// positions, triggers, touchpad events, and motion data.
	gamepad: inputtino::Joypad,

	/// Gamepad index assigned by the client (0-15).
	index: u8,

	/// Hold-to-Home button remap state machine for this gamepad.
	remap: HoldToHome,

	/// Channel to send feedback commands (rumble, LED, trigger effects, motion)
	/// back to the client.
	feedback_tx: mpsc::Sender<FeedbackCommand>,

	/// Rumble intensity for the hold-to-Home activation pulse (0.0-1.0).
	home_rumble_intensity: f64,

	/// Duration of the hold-to-Home activation rumble pulse.
	home_rumble_duration: std::time::Duration,

	/// Instant at which the hold-to-Home rumble pulse should be turned off,
	/// or `None` if no pulse is active.
	home_rumble_off_at: Option<Instant>,
}

impl Gamepad {
	pub async fn new(
		info: &GamepadInfo,
		feedback_tx: mpsc::Sender<FeedbackCommand>,
		config: &GamepadConfig,
	) -> Result<Self, ()> {
		let id = format!("00:11:22:33:{:02x}", info.index);
		let definition = match info.kind {
			GamepadKind::Unknown | GamepadKind::Xbox => DeviceDefinition::new(
				"Moonshine XOne controller",
				0x045e,
				0x02dd,
				0x0100,
				id.as_str(),
				id.as_str(),
			),
			GamepadKind::PlayStation => DeviceDefinition::new(
				"Moonshine PS5 controller",
				0x054C,
				0x0CE6,
				0x8111,
				id.as_str(),
				id.as_str(),
			),
			GamepadKind::Nintendo => DeviceDefinition::new(
				"Moonshine Switch controller",
				0x057e,
				0x2009,
				0x8111,
				id.as_str(),
				id.as_str(),
			),
		};

		let mut gamepad = match info.kind {
			GamepadKind::Unknown | GamepadKind::Xbox => Joypad::XboxOne(
				XboxOneJoypad::new(&definition).map_err(|e| tracing::warn!("Failed to create gamepad: {e}"))?,
			),
			GamepadKind::PlayStation => {
				let mut gamepad =
					PS5Joypad::new(&definition).map_err(|e| tracing::warn!("Failed to create gamepad: {e}"))?;

				gamepad.set_on_led({
					let feedback_tx = feedback_tx.clone();
					let index = info.index;
					move |r, g, b| {
						let _ = feedback_tx.blocking_send(FeedbackCommand::SetLed(SetLedCommand {
							id: index as u16,
							rgb: (r as u8, g as u8, b as u8),
						}));
					}
				});

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
					}
				});

				// Enable gyro and accelerometer events.
				let _ = feedback_tx
					.send(FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
						id: info.index as u16,
						report_rate: 100,
						motion_type: JoypadMotionType::ACCELERATION as u8,
					}))
					.await;
				let _ = feedback_tx
					.send(FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
						id: info.index as u16,
						report_rate: 100,
						motion_type: JoypadMotionType::GYROSCOPE as u8,
					}))
					.await;

				Joypad::PS5(gamepad)
			},
			GamepadKind::Nintendo => Joypad::Switch(
				SwitchJoypad::new(&definition).map_err(|e| tracing::warn!("Failed to create gamepad: {e}"))?,
			),
		};

		let feedback_tx_for_rumble = feedback_tx.clone();
		gamepad.set_on_rumble({
			let index = info.index;
			move |low_frequency, high_frequency| {
				let _ = feedback_tx_for_rumble.blocking_send(FeedbackCommand::Rumble(RumbleCommand {
					id: index as u16,
					low_frequency: low_frequency as u16,
					high_frequency: high_frequency as u16,
				}));
			}
		});

		Ok(Self {
			gamepad,
			index: info.index,
			remap: HoldToHome::new(config),
			feedback_tx,
			home_rumble_intensity: config.home_button.rumble_intensity,
			home_rumble_duration: std::time::Duration::from_millis(config.home_button.rumble_duration_ms),
			home_rumble_off_at: None,
		})
	}

	fn send_rumble(&self, low_frequency: u16, high_frequency: u16) {
		let _ = self.feedback_tx.try_send(FeedbackCommand::Rumble(RumbleCommand {
			id: self.index as u16,
			low_frequency,
			high_frequency,
		}));
	}

	fn check_rumble(&mut self, now: Instant) {
		if let Some(off_at) = self.home_rumble_off_at {
			if now >= off_at {
				self.send_rumble(0, 0);
				self.home_rumble_off_at = None;
			}
		}
	}

	pub fn update(&mut self, update: &GamepadUpdate) {
		let now = Instant::now();

		self.check_rumble(now);

		// Remap buttons (e.g. hold-to-Home) before they reach the device.
		let (button_flags, transition) = self.remap.apply(update.button_flags, now);
		self.gamepad.set_pressed(button_flags as i32);

		if transition == HoldTransition::HomeActivated
			&& self.home_rumble_off_at.is_none()
			&& self.home_rumble_intensity > 0.0
			&& !self.home_rumble_duration.is_zero()
		{
			let intensity = (self.home_rumble_intensity * u16::MAX as f64) as u16;
			self.send_rumble(intensity, intensity);
			self.home_rumble_off_at = Some(now + self.home_rumble_duration);
		}

		// Send analog triggers.
		self.gamepad
			.set_stick(JoypadStickPosition::LS, update.left_stick.0, update.left_stick.1);
		self.gamepad
			.set_stick(JoypadStickPosition::RS, update.right_stick.0, update.right_stick.1);
		self.gamepad
			.set_triggers(update.left_trigger as i16, update.right_trigger as i16);
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
			gamepad.set_motion(
				motion.motion_type,
				motion.x.to_radians(),
				motion.y.to_radians(),
				motion.z.to_radians(),
			);
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
				},
			};

			gamepad.set_battery(state, gamepad_battery.battery_percentage);
		}
	}
}
