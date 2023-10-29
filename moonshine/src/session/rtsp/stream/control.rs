use async_shutdown::ShutdownManager;
use enet::{
	Address,
	BandwidthLimit,
	ChannelLimit,
	Enet,
	Event,
};
use openssl::symm::Cipher;

use crate::{session::{SessionContext, rtsp::stream::audio}, config::Config};

use super::{VideoStream, AudioStream};

const ENCRYPTION_TAG_LENGTH: usize = 16;
// Sequence number + tag + control message id
const MINIMUM_ENCRYPTED_LENGTH: usize = 4 + ENCRYPTION_TAG_LENGTH + 4;

#[repr(u16)]
enum ControlMessageType {
	Encrypted = 0x0001,
	Ping = 0x0200,
	Termination = 0x0100,
	RumbleData = 0x010b,
	LossStats = 0x0201,
	FrameStats = 0x204,
	InputData = 0x206,
	InvalidateReferenceFrames = 0x0301,
	RequestIdrFrame = 0x0302,
	StartA = 0x0305,
	StartB = 0x0307,
}

impl TryFrom<u16> for ControlMessageType {
	type Error = ();

	fn try_from(v: u16) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::Encrypted as u16 => Ok(Self::Encrypted),
			x if x == Self::Ping as u16 => Ok(Self::Ping),
			x if x == Self::Termination as u16 => Ok(Self::Termination),
			x if x == Self::RumbleData as u16 => Ok(Self::RumbleData),
			x if x == Self::LossStats as u16 => Ok(Self::LossStats),
			x if x == Self::FrameStats as u16 => Ok(Self::FrameStats),
			x if x == Self::InputData as u16 => Ok(Self::InputData),
			x if x == Self::InvalidateReferenceFrames as u16 => Ok(Self::InvalidateReferenceFrames),
			x if x == Self::RequestIdrFrame as u16 => Ok(Self::RequestIdrFrame),
			x if x == Self::StartA as u16 => Ok(Self::StartA),
			x if x == Self::StartB as u16 => Ok(Self::StartB),
			_ => Err(()),
		}
	}
}

#[derive(Debug)]
enum ControlMessage {
	Encrypted(EncryptedControlMessage),
	Ping,
	Termination,
	RumbleData,
	LossStats,
	FrameStats,
	InputData,
	InvalidateReferenceFrames,
	RequestIdrFrame,
	StartA,
	StartB,
}

impl ControlMessage {
	fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 2 {
			log::warn!("Expected control message to have at least two bytes, got {}", buffer.len());
			return Err(());
		}

		match u16::from_le_bytes(buffer[..2].try_into().unwrap()).try_into() {
			Ok(ControlMessageType::Encrypted) => {
				if buffer.len() < MINIMUM_ENCRYPTED_LENGTH {
					log::info!("Expected encrypted control message of at least {MINIMUM_ENCRYPTED_LENGTH} bytes, got buffer of {} bytes.", buffer.len());
					return Err(());
				}

				let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
				if (length as usize) < MINIMUM_ENCRYPTED_LENGTH {
					log::info!("Expected encrypted control message of at least {MINIMUM_ENCRYPTED_LENGTH} bytes, got reported length of {length} bytes.");
					return Err(());
				}

				let sequence_number = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
				Ok(Self::Encrypted(EncryptedControlMessage {
					_length: length,
					sequence_number,
					tag: buffer[8..8 + ENCRYPTION_TAG_LENGTH].try_into()
						.map_err(|e| log::warn!("Failed to get tag from encrypted control message: {e}"))?,
					payload: buffer[8 + ENCRYPTION_TAG_LENGTH..].to_vec(),
				}))
			},
			Ok(ControlMessageType::Ping) => Ok(Self::Ping),
			Ok(ControlMessageType::Termination) => Ok(Self::Termination),
			Ok(ControlMessageType::RumbleData) => Ok(Self::RumbleData),
			Ok(ControlMessageType::LossStats) => Ok(Self::LossStats),
			Ok(ControlMessageType::FrameStats) => Ok(Self::FrameStats),
			Ok(ControlMessageType::InputData) => Ok(Self::InputData),
			Ok(ControlMessageType::InvalidateReferenceFrames) => Ok(Self::InvalidateReferenceFrames),
			Ok(ControlMessageType::RequestIdrFrame) => Ok(Self::RequestIdrFrame),
			Ok(ControlMessageType::StartA) => Ok(Self::StartA),
			Ok(ControlMessageType::StartB) => Ok(Self::StartB),
			Err(()) => {
				Err(())
			},
		}
	}
}

#[derive(Debug)]
struct EncryptedControlMessage {
	_length: u16,
	sequence_number: u32,
	tag: [u8; 16],
	payload: Vec<u8>,
}

pub async fn run_control_stream(
	config: Config,
	video_stream: VideoStream,
	audio_stream: AudioStream,
	context: SessionContext,
	enet: Enet,
	stop_signal: ShutdownManager<()>,
) -> Result<(), ()> {
	let local_addr = Address::new(
		config.address.parse()
		.map_err(|e| log::error!("Failed to parse address: {e}"))?,
		config.stream.control.port,
	);
	let mut host = enet
		.create_host::<()>(
			Some(&local_addr),
			10,
			ChannelLimit::Maximum,
			BandwidthLimit::Unlimited,
			BandwidthLimit::Unlimited,
		)
		.unwrap();

	log::info!("Listening for control messages on {:?}", host.address());

	loop {
		if stop_signal.is_shutdown_triggered() {
			log::debug!("Stopping due to shutdown triggered.");
			break;
		}

		match host.service(1000).map_err(|e| log::error!("Failure in enet host: {e}"))? {
			Some(Event::Connect(_)) => {},
			Some(Event::Disconnect(..)) => {},
			Some(Event::Receive {
				ref packet,
				..
			}) => {
				let mut control_message = ControlMessage::from_bytes(packet.data())?;
				log::trace!("Received control message: {control_message:?}");

				// First check for encrypted control messages and decrypt them.
				if let ControlMessage::Encrypted(message) = control_message {
					let mut initialization_vector = [0u8; 16];
					initialization_vector[0] = message.sequence_number as u8;

					let decrypted = openssl::symm::decrypt_aead(
						Cipher::aes_128_gcm(),
						&context.remote_input_key,
						Some(&initialization_vector),
						&[],
						&message.payload,
						&message.tag,
					);

					let decrypted = match decrypted {
						Ok(decrypted) => decrypted,
						Err(e) => {
							log::error!("Failed to decrypt control message: {:?}", e.errors());
							continue;
						}
					};

					control_message = match ControlMessage::from_bytes(&decrypted) {
						Ok(decrypted_message) => decrypted_message,
						Err(()) => {
							log::warn!("Failed to parse decrypted control message.");
							continue;
						},
					};

					log::trace!("Decrypted control message: {control_message:?}");
				}

				match control_message {
					ControlMessage::Encrypted(_) => unreachable!("Encrypted control messages should be decrypted already."),
					ControlMessage::RequestIdrFrame | ControlMessage::InvalidateReferenceFrames => {
						video_stream.request_idr_frame().await?;
					},
					ControlMessage::StartB => {
						audio_stream.start().await?;
						video_stream.start().await?;
					},
					skipped_message => {
						log::trace!("Skipped control message: {skipped_message:?}");
					},
				};
			}
			_ => (),
		}
	}

	Ok(())
}
