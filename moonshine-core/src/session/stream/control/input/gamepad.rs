use inputtino::{
	BatteryState as InputtinoBatterState, DeviceDefinition, Joypad, JoypadMotionType, JoypadStickPosition, PS5Joypad,
	SwitchJoypad, XboxOneJoypad,
};
use serde::{Deserialize, Serialize};
use strum_macros::FromRepr;
use tokio::sync::mpsc;

use crate::session::stream::control::{
	FeedbackCommand,
	feedback::{EnableMotionEventCommand, RumbleCommand, SetLedCommand, TriggerEffectCommand},
};

const SONY_VENDOR: u16 = 0x054c;
const DUALSENSE_PRODUCT: u16 = 0x0ce6;
const DUALSENSE_EDGE_PRODUCT: u16 = 0x0df2;

/// Intentional synthetic Guide shortcut. Physical Guide is already carried by
/// Moonlight's SPECIAL flag; no shortcut is needed for clients that send it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeTrigger {
	Disabled,
	HoldBack,
	BackStart,
}

/// Home/Guide policy. An omitted trigger retains legacy `hold_ms` semantics;
/// the default zero threshold leaves ordinary controller input untouched.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HomeButtonConfig {
	/// None means legacy: nonzero hold_ms selects HoldBack, zero disables it.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub trigger: Option<HomeTrigger>,
	/// Hold threshold; zero disables synthetic remapping for every policy.
	pub hold_ms: u64,
	/// Activation rumble duration; zero disables the pulse.
	pub rumble_duration_ms: u64,
	/// Activation rumble intensity (0.0-1.0).
	pub rumble_intensity: f64,
	/// Drop physical Guide, independently of synthetic shortcut activation.
	pub suppress_home: bool,
}

impl HomeButtonConfig {
	pub fn trigger(&self) -> HomeTrigger {
		if self.hold_ms == 0 {
			HomeTrigger::Disabled
		} else {
			self.trigger.unwrap_or(HomeTrigger::HoldBack)
		}
	}
}

impl Default for HomeButtonConfig {
	fn default() -> Self {
		Self {
			trigger: None,
			hold_ms: 0,
			rumble_duration_ms: 50,
			rumble_intensity: 0.5,
			suppress_home: false,
		}
	}
}

/// Configuration for gamepad input handling.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GamepadConfig {
	/// Configuration for the intentional Home shortcut.
	pub home_button: HomeButtonConfig,
	/// Virtual controller family; auto preserves native client features.
	pub emulation: GamepadEmulation,
}

#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GamepadEmulation {
	#[default]
	Auto,
	Xbox,
	Playstation,
	Nintendo,
}

impl GamepadEmulation {
	fn target(self, incoming: GamepadKind) -> GamepadKind {
		match self {
			Self::Auto => match incoming {
				GamepadKind::Unknown | GamepadKind::Steam => GamepadKind::Xbox,
				kind => kind,
			},
			Self::Xbox => GamepadKind::Xbox,
			Self::Playstation => GamepadKind::PlayStation,
			Self::Nintendo => GamepadKind::Nintendo,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, FromRepr)]
#[repr(u8)]
pub(crate) enum GamepadKind {
	Unknown = 0x00,
	Xbox = 0x01,
	PlayStation = 0x02,
	Nintendo = 0x03,
	Steam = 0x04,
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

	/// LI_CCAP_DUALSENSE_EDGE; subtype, never a new controller family.
	DualSenseEdge = 0x200,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GamepadInfo {
	pub index: u8,
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
			tracing::warn!(
				"Expected at least {EXPECTED_SIZE} bytes for GamepadInfo, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		Ok(Self {
			index: buffer[0],
			kind: GamepadKind::from_repr(buffer[1]).unwrap_or_else(|| {
				tracing::warn!("Unknown gamepad kind: {}; using compatibility device", buffer[1]);
				GamepadKind::Unknown
			}),
			capabilities: u16::from_le_bytes(buffer[2..4].try_into().unwrap()),
			supported_buttons: u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
		})
	}

	fn is_dualsense_edge(&self) -> bool {
		self.kind == GamepadKind::PlayStation && self.has_capability(&GamepadCapability::DualSenseEdge)
	}

	fn playstation_product(&self) -> u16 {
		if self.is_dualsense_edge() {
			DUALSENSE_EDGE_PRODUCT
		} else {
			DUALSENSE_PRODUCT
		}
	}

	pub fn default_for_index(index: u8) -> Self {
		Self {
			index,
			kind: GamepadKind::Unknown,
			capabilities: 0,
			supported_buttons: 0,
		}
	}

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

	pub fn button_flags(&self) -> u32 {
		self.button_flags
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
}

impl Gamepad {
	pub async fn new(
		info: &GamepadInfo,
		feedback_tx: mpsc::Sender<FeedbackCommand>,
		policy: GamepadEmulation,
	) -> Result<Self, ()> {
		let kind = policy.target(info.kind);
		tracing::debug!(index = info.index, incoming = ?info.kind, ?policy, virtual_kind = ?kind,
			edge = info.is_dualsense_edge(), supported_buttons = format_args!("{:#010x}", info.supported_buttons),
			capabilities = format_args!("{:#06x}", info.capabilities),
			"Creating virtual controller");
		let id = format!("00:11:22:33:44:{:02x}", info.index);
		let definition = match kind {
			GamepadKind::Unknown | GamepadKind::Steam | GamepadKind::Xbox => DeviceDefinition::new(
				"Moonshine XOne controller",
				0x045e,
				0x02dd,
				0x0100,
				id.as_str(),
				id.as_str(),
			),
			GamepadKind::PlayStation => DeviceDefinition::new(
				if info.is_dualsense_edge() {
					"Moonshine DualSense Edge controller"
				} else {
					"Moonshine PS5 controller"
				},
				SONY_VENDOR,
				info.playstation_product(),
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

		if kind == GamepadKind::PlayStation {
			tracing::debug!(
				vendor = format_args!("{SONY_VENDOR:#06x}"),
				product = format_args!("{:#06x}", info.playstation_product()),
				"Selected virtual PlayStation identity"
			);
		}

		let mut gamepad = match kind {
			GamepadKind::Unknown | GamepadKind::Steam | GamepadKind::Xbox => Joypad::XboxOne(
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

		Ok(Self { gamepad })
	}

	/// Apply button flags to the gamepad.
	pub fn set_pressed(&self, button_flags: u32) {
		self.gamepad.set_pressed(button_flags as i32);
	}

	/// Apply a gamepad update (sticks, triggers) to the device.
	pub fn apply_update(&self, update: &GamepadUpdate) {
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

#[cfg(test)]
mod compatibility_tests {
	use super::*;
	#[test]
	fn all_moonlight_controller_kinds_and_future_values_are_accepted() {
		for raw in [0, 1, 2, 3, 4, 255] {
			let info = GamepadInfo::from_bytes(&[15, raw, 0x7f, 0, 0, 0, 0, 0]).unwrap();
			assert_eq!(info.index, 15);
			if raw == 4 {
				assert_eq!(info.kind, GamepadKind::Steam);
			}
			if raw == 255 {
				assert_eq!(info.kind, GamepadKind::Unknown);
			}
		}
		assert!(GamepadInfo::from_bytes(&[0, 4]).is_err());
	}
	#[test]
	fn auto_preserves_native_families_and_forced_policy_overrides_every_kind() {
		for kind in [
			GamepadKind::Unknown,
			GamepadKind::Xbox,
			GamepadKind::PlayStation,
			GamepadKind::Nintendo,
			GamepadKind::Steam,
		] {
			assert_eq!(GamepadEmulation::Xbox.target(kind), GamepadKind::Xbox);
			assert_eq!(GamepadEmulation::Playstation.target(kind), GamepadKind::PlayStation);
			assert_eq!(GamepadEmulation::Nintendo.target(kind), GamepadKind::Nintendo);
		}
		assert_eq!(
			GamepadEmulation::Auto.target(GamepadKind::PlayStation),
			GamepadKind::PlayStation
		);
		assert_eq!(
			GamepadEmulation::Auto.target(GamepadKind::Nintendo),
			GamepadKind::Nintendo
		);
		assert_eq!(GamepadEmulation::Auto.target(GamepadKind::Steam), GamepadKind::Xbox);
		assert_eq!(GamepadEmulation::Auto.target(GamepadKind::Unknown), GamepadKind::Xbox);
	}
	#[test]
	fn edge_identity_requires_playstation_and_explicit_capability() {
		for kind in [0, 1, 2, 3, 4, 255] {
			for caps in [0u16, 0xff, 0x100, 0x200, 0xffff] {
				let mut arrival = [0u8; 8];
				arrival[1] = kind;
				arrival[2..4].copy_from_slice(&caps.to_le_bytes());
				arrival[4..8].copy_from_slice(&0x000f0000u32.to_le_bytes());
				let info = GamepadInfo::from_bytes(&arrival).unwrap();
				let edge = kind == 2 && caps & 0x200 != 0;
				assert_eq!(info.is_dualsense_edge(), edge);
				assert_eq!(info.playstation_product(), if edge { 0x0df2 } else { 0x0ce6 });
				assert_eq!(
					GamepadEmulation::Playstation.target(info.kind),
					GamepadKind::PlayStation
				);
			}
		}
		assert!(!GamepadInfo::default_for_index(0).is_dualsense_edge());
	}

	#[test]
	fn extended_buttons_round_trip_press_release_and_ordinary_buttons() {
		for flags in [0u32, 0x10000, 0x20000, 0x40000, 0x80000, 0x3f0000, 0x3ff3ff, 0] {
			let mut packet = [0u8; 26];
			packet[8..10].copy_from_slice(&(flags as u16).to_le_bytes());
			packet[22..24].copy_from_slice(&((flags >> 16) as u16).to_le_bytes());
			assert_eq!(GamepadUpdate::from_bytes(&packet).unwrap().button_flags(), flags);
		}
		assert!(GamepadUpdate::from_bytes(&[0; 25]).is_err());
	}

	#[test]
	fn edge_buttons_survive_home_remap_and_timer_without_stuck_release() {
		use super::super::remap::{BACK_FLAG, HoldToHome, SPECIAL_FLAG};
		use std::time::{Duration, Instant};
		let now = Instant::now();
		for paddle in [0x10000, 0x20000, 0x40000, 0x80000] {
			let mut config = GamepadConfig::default();
			let mut remap = HoldToHome::new(&config);
			assert_eq!(remap.apply(paddle | 0x1000, now).0, paddle | 0x1000);
			assert_eq!(remap.apply(0, now).0, 0);
			config.home_button.hold_ms = 500;
			config.home_button.suppress_home = true;
			let mut remap = HoldToHome::new(&config);
			assert_eq!(remap.apply(paddle | 0x1000 | BACK_FLAG, now).0, paddle | 0x1000);
			let deadline = now + Duration::from_millis(500);
			assert_eq!(remap.advance(deadline).0, paddle | 0x1000 | SPECIAL_FLAG);
			assert_eq!(remap.apply(0, deadline).0, 0);
			assert!(remap.next_deadline().is_none());
			// A release while the Home timer is pending must not reassert a paddle.
			let mut remap = HoldToHome::new(&config);
			remap.apply(paddle | BACK_FLAG, now);
			remap.apply(BACK_FLAG, now + Duration::from_millis(100));
			assert_eq!(remap.advance(deadline).0, SPECIAL_FLAG);
			assert_eq!(remap.apply(0, deadline).0, 0);
		}
	}

	#[test]
	fn home_policy_migration_is_explicit_and_round_trips() {
		for (text, expected) in [
			("", HomeTrigger::Disabled),
			("hold_ms = 750", HomeTrigger::HoldBack),
			("hold_ms = 0", HomeTrigger::Disabled),
			("trigger = \"disabled\"\nhold_ms = 750", HomeTrigger::Disabled),
			("trigger = \"hold_back\"\nhold_ms = 750", HomeTrigger::HoldBack),
			("trigger = \"back_start\"\nhold_ms = 750", HomeTrigger::BackStart),
		] {
			let config: HomeButtonConfig = toml::from_str(text).unwrap();
			assert_eq!(config.trigger(), expected);
			let encoded = toml::to_string(&config).unwrap();
			assert_eq!(
				toml::from_str::<HomeButtonConfig>(&encoded).unwrap().trigger(),
				expected
			);
		}
		assert!(toml::from_str::<HomeButtonConfig>("trigger = \"invalid\"").is_err());
	}

	#[test]
	fn configuration_defaults_and_invalid_policies() {
		let config: GamepadConfig = toml::from_str("").unwrap();
		assert!(matches!(config.emulation, GamepadEmulation::Auto));
		for value in ["auto", "xbox", "playstation", "nintendo"] {
			assert!(toml::from_str::<GamepadConfig>(&format!("emulation = \"{value}\"")).is_ok());
		}
		assert!(toml::from_str::<GamepadConfig>("emulation = \"invalid\"").is_err());
	}
}
