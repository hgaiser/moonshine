use super::ControlMessageType;

#[derive(Debug)]
pub enum FeedbackCommand {
	Rumble(RumbleCommand),
	SetLed(SetLedCommand),
	EnableMotionEvent(EnableMotionEventCommand),
}

impl FeedbackCommand {
	pub fn as_packet(&self) -> Vec<u8> {
		match self {
			FeedbackCommand::Rumble(command) => command.as_packet().to_vec(),
			FeedbackCommand::SetLed(command) => command.as_packet().to_vec(),
			FeedbackCommand::EnableMotionEvent(command) => command.as_packet().to_vec(),
		}
	}
}

#[derive(Debug)]
pub struct RumbleCommand {
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
pub struct SetLedCommand {
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
pub struct EnableMotionEventCommand {
	pub id: u16,
	pub report_rate: u16,
	pub motion_type: u8,
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
		buffer[8] = self.motion_type;

		buffer
	}
}
