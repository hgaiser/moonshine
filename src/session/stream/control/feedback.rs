use std::sync::Mutex;

use arrayvec::ArrayVec;
use inputtino::JoypadMotionType;

use super::MAX_GAMEPADS;
use crate::session::stream::control::ControlMessageType;

/// Latest unsent feedback for each controller. Producers never wait for the
/// control loop to make room, and no network work is done while holding the lock.
#[derive(Default)]
pub(super) struct Feedback(Mutex<[Option<PendingFeedback>; MAX_GAMEPADS]>);

impl Feedback {
	pub(super) fn rumble(&self, command: RumbleCommand) {
		self.update(command.id, |pending| pending.rumble = Some(command));
	}

	pub(super) fn led(&self, command: SetLedCommand) {
		self.update(command.id, |pending| pending.led = Some(command));
	}

	pub(super) fn motion(&self, command: EnableMotionEventCommand) {
		self.update(command.id, |pending| match command.motion_type {
			JoypadMotionType::ACCELERATION => pending.accel = Some(command),
			JoypadMotionType::GYROSCOPE => pending.gyro = Some(command),
		});
	}

	pub(super) fn trigger_effect(&self, mut command: TriggerEffectCommand) {
		self.update(command.id, |pending| {
			if let Some(old) = pending.trigger.take() {
				command.keep_unchanged_triggers(old);
			}
			pending.trigger = Some(command);
		});
	}

	fn update(&self, id: u16, update: impl FnOnce(&mut PendingFeedback)) {
		let mut pending = self.0.lock().unwrap();
		update(pending[usize::from(id)].get_or_insert_with(PendingFeedback::default));
	}

	pub(super) fn take(&self) -> ArrayVec<PendingFeedback, MAX_GAMEPADS> {
		self.0.lock().unwrap().iter_mut().filter_map(Option::take).collect()
	}
}

#[derive(Debug, Default)]
pub(super) struct PendingFeedback {
	rumble: Option<RumbleCommand>,
	led: Option<SetLedCommand>,
	accel: Option<EnableMotionEventCommand>,
	gyro: Option<EnableMotionEventCommand>,
	trigger: Option<TriggerEffectCommand>,
}

impl PendingFeedback {
	pub(super) fn send(self, mut send: impl FnMut(&[u8])) {
		if let Some(command) = self.rumble {
			send(&command.as_packet());
		}
		if let Some(command) = self.led {
			send(&command.as_packet());
		}
		for command in [self.accel, self.gyro].into_iter().flatten() {
			send(&command.as_packet());
		}
		if let Some(command) = self.trigger {
			send(&command.as_packet());
		}
	}
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RumbleCommand {
	pub id: u16,
	pub low_frequency: u16,
	pub high_frequency: u16,
}

impl RumbleCommand {
	const HEADER_LENGTH: usize =
		std::mem::size_of::<u16>() // Feedback type.
		+ std::mem::size_of::<u16>() // Payload length.
	;
	const PAYLOAD_LENGTH: usize =
		std::mem::size_of::<u32>() // Padding.
		+ std::mem::size_of::<u16>() // ID of the gamepad.
		+ std::mem::size_of::<u16>() // Low frequency.
		+ std::mem::size_of::<u16>() // High frequency.
	;

	pub fn as_packet(&self) -> [u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH] {
		let mut buffer = [0u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH];

		buffer[0..2].copy_from_slice(&(ControlMessageType::RumbleData as u16).to_le_bytes());
		buffer[2..4].copy_from_slice(&(Self::PAYLOAD_LENGTH as u16).to_le_bytes());
		// buffer[4..8].copy_from_slice(&[0, 0, 0, 0]); // Padding.
		buffer[8..10].copy_from_slice(&self.id.to_le_bytes());
		buffer[10..12].copy_from_slice(&self.low_frequency.to_le_bytes());
		buffer[12..14].copy_from_slice(&self.high_frequency.to_le_bytes());

		buffer
	}
}

#[derive(Debug)]
pub(crate) struct SetLedCommand {
	pub id: u16,
	pub rgb: (u8, u8, u8),
}

impl SetLedCommand {
	const HEADER_LENGTH: usize =
		std::mem::size_of::<u16>() // Feedback type.
		+ std::mem::size_of::<u16>() // Payload length.
	;
	const PAYLOAD_LENGTH: usize =
		std::mem::size_of::<u16>() // ID of the gamepad.
		+ 3 * std::mem::size_of::<u8>() // RGB values.
	;

	pub fn as_packet(&self) -> [u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH] {
		let mut buffer = [0u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH];

		buffer[0..2].copy_from_slice(&(ControlMessageType::SetRgbLed as u16).to_le_bytes());
		buffer[2..4].copy_from_slice(&(Self::PAYLOAD_LENGTH as u16).to_le_bytes());
		buffer[4..6].copy_from_slice(&self.id.to_le_bytes());
		buffer[6] = self.rgb.0;
		buffer[7] = self.rgb.1;
		buffer[8] = self.rgb.2;

		buffer
	}
}

#[derive(Debug)]
pub(crate) struct EnableMotionEventCommand {
	pub id: u16,
	pub report_rate: u16,
	pub motion_type: JoypadMotionType,
}

impl EnableMotionEventCommand {
	const HEADER_LENGTH: usize =
		std::mem::size_of::<u16>() // Feedback type.
		+ std::mem::size_of::<u16>() // Payload length.
	;
	const PAYLOAD_LENGTH: usize =
		std::mem::size_of::<u16>() // ID of the gamepad.
		+ std::mem::size_of::<u16>() // Report rate.
		+ std::mem::size_of::<u8>() // Motion type.
	;

	pub fn as_packet(&self) -> [u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH] {
		let mut buffer = [0u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH];

		buffer[0..2].copy_from_slice(&(ControlMessageType::SetMotionEvent as u16).to_le_bytes());
		buffer[2..4].copy_from_slice(&(Self::PAYLOAD_LENGTH as u16).to_le_bytes());
		buffer[4..6].copy_from_slice(&self.id.to_le_bytes());
		buffer[6..8].copy_from_slice(&self.report_rate.to_le_bytes());
		buffer[8] = self.motion_type as u8;

		buffer
	}
}

#[derive(Debug)]
pub(crate) struct TriggerEffectCommand {
	pub id: u16,
	pub trigger_event_flags: u8,
	pub type_left: u8,
	pub type_right: u8,
	pub left: [u8; 10],
	pub right: [u8; 10],
}

impl TriggerEffectCommand {
	const HEADER_LENGTH: usize = std::mem::size_of::<u16>() // Feedback type.
		+ std::mem::size_of::<u16>(); // Payload length.

	const PAYLOAD_LENGTH: usize = std::mem::size_of::<u16>() // ID of the gamepad.
		+ std::mem::size_of::<u8>() // Trigger event flags.
		+ std::mem::size_of::<u8>() // Type left.
		+ std::mem::size_of::<u8>() // Type right.
		+ 10 * std::mem::size_of::<u8>() // Left trigger effect (10 bytes).
		+ 10 * std::mem::size_of::<u8>(); // Right trigger effect (10 bytes).

	fn keep_unchanged_triggers(&mut self, old: Self) {
		// DualSense valid_flag0 bits, as forwarded by inputtino.
		const RIGHT: u8 = 0x04;
		const LEFT: u8 = 0x08;
		let unchanged = old.trigger_event_flags & !self.trigger_event_flags;
		if unchanged & LEFT != 0 {
			self.type_left = old.type_left;
			self.left = old.left;
		}
		if unchanged & RIGHT != 0 {
			self.type_right = old.type_right;
			self.right = old.right;
		}
		self.trigger_event_flags |= unchanged;
	}

	pub fn as_packet(&self) -> [u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH] {
		let mut buffer = [0u8; Self::HEADER_LENGTH + Self::PAYLOAD_LENGTH];

		// Write the header.
		buffer[0..2].copy_from_slice(&(ControlMessageType::SetTriggerEffect as u16).to_le_bytes());
		buffer[2..4].copy_from_slice(&(Self::PAYLOAD_LENGTH as u16).to_le_bytes());

		// Write the payload.
		buffer[4..6].copy_from_slice(&self.id.to_le_bytes());
		buffer[6] = self.trigger_event_flags;
		buffer[7] = self.type_left;
		buffer[8] = self.type_right;
		buffer[9..19].copy_from_slice(&self.left);
		buffer[19..29].copy_from_slice(&self.right);

		buffer
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn drain_packets(feedback: &Feedback) -> Vec<Vec<u8>> {
		let mut packets = Vec::new();
		for pending in feedback.take() {
			pending.send(|packet| packets.push(packet.to_vec()));
		}
		packets
	}

	#[test]
	fn latest_rumble_state_survives_bursts_without_replaying_old_effects() {
		let feedback = Feedback::default();
		for value in 1..=500 {
			feedback.rumble(RumbleCommand {
				id: 0,
				low_frequency: value,
				high_frequency: value,
			});
			feedback.rumble(RumbleCommand {
				id: 1,
				low_frequency: 0,
				high_frequency: value,
			});
		}
		let stop = RumbleCommand {
			id: 0,
			low_frequency: 0,
			high_frequency: 0,
		};
		feedback.rumble(stop);

		let other_pad = RumbleCommand {
			id: 1,
			low_frequency: 0,
			high_frequency: 500,
		};
		assert_eq!(drain_packets(&feedback), [stop.as_packet(), other_pad.as_packet()]);
		assert!(drain_packets(&feedback).is_empty());
	}

	#[test]
	fn partial_trigger_updates_keep_both_latest_effects() {
		let feedback = Feedback::default();
		for (flags, left, right) in [(0x0c, 1, 2), (0x08, 3, 0), (0x04, 0, 4)] {
			feedback.trigger_effect(TriggerEffectCommand {
				id: 0,
				trigger_event_flags: flags,
				type_left: left,
				type_right: right,
				left: [left; 10],
				right: [right; 10],
			});
		}

		let expected = TriggerEffectCommand {
			id: 0,
			trigger_event_flags: 0x0c,
			type_left: 3,
			type_right: 4,
			left: [3; 10],
			right: [4; 10],
		};
		assert_eq!(drain_packets(&feedback), [expected.as_packet()]);
	}

	#[test]
	fn enabling_one_sensor_does_not_replace_the_other() {
		let feedback = Feedback::default();
		for motion_type in [JoypadMotionType::ACCELERATION, JoypadMotionType::GYROSCOPE] {
			feedback.motion(EnableMotionEventCommand {
				id: 0,
				report_rate: 100,
				motion_type,
			});
		}
		let packets = drain_packets(&feedback);
		assert_eq!(packets.iter().map(|packet| packet[8]).collect::<Vec<_>>(), [1, 2]);
	}
}
