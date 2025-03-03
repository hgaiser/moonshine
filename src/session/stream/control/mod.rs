use std::net::{SocketAddr, UdpSocket};

use async_shutdown::ShutdownManager;
use openssl::{cipher, symm};
use rusty_enet as enet;
use tokio::sync::mpsc::{self, error::TryRecvError};

use crate::{config::Config, crypto::encrypt, session::{SessionContext, SessionKeys}};
use self::{input::InputHandler, feedback::FeedbackCommand};
use super::{VideoStream, AudioStream};

mod input;
mod feedback;

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
	FrameStats = 0x0204,
	InputData = 0x0206,
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
enum ControlMessage<'a> {
	Encrypted(EncryptedControlMessage),
	Ping,
	Termination,
	RumbleData,
	LossStats,
	FrameStats,
	InputData(&'a [u8]),
	InvalidateReferenceFrames,
	RequestIdrFrame,
	StartA,
	StartB,
}

impl<'a> ControlMessage<'a> {
	fn from_bytes(buffer: &'a [u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			tracing::warn!("Expected control message to have at least 4 bytes, got {}", buffer.len());
			return Err(());
		}

		let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
		if length as usize != buffer.len() - 4 {
			tracing::info!("Received incorrect packet length: expecting {length} bytes, but buffer says it should be {} bytes.", buffer.len() - 4);
			return Err(());
		}

		match u16::from_le_bytes(buffer[..2].try_into().unwrap()).try_into()? {
			ControlMessageType::Encrypted => {
				if buffer.len() < MINIMUM_ENCRYPTED_LENGTH {
					tracing::info!("Expected encrypted control message of at least {MINIMUM_ENCRYPTED_LENGTH} bytes, got buffer of {} bytes.", buffer.len());
					return Err(());
				}

				let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
				if (length as usize) < MINIMUM_ENCRYPTED_LENGTH {
					tracing::info!("Expected encrypted control message of at least {MINIMUM_ENCRYPTED_LENGTH} bytes, got reported length of {length} bytes.");
					return Err(());
				}

				let sequence_number = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
				Ok(Self::Encrypted(EncryptedControlMessage {
					length,
					sequence_number,
					tag: buffer[8..8 + ENCRYPTION_TAG_LENGTH].try_into()
						.map_err(|e| tracing::warn!("Failed to get tag from encrypted control message: {e}"))?,
					payload: buffer[8 + ENCRYPTION_TAG_LENGTH..].to_vec(),
				}))
			},
			ControlMessageType::Ping => Ok(Self::Ping),
			ControlMessageType::Termination => Ok(Self::Termination),
			ControlMessageType::RumbleData => Ok(Self::RumbleData),
			ControlMessageType::LossStats => Ok(Self::LossStats),
			ControlMessageType::FrameStats => Ok(Self::FrameStats),
			ControlMessageType::InputData => {
				// Length of the input event, excluding the length itself.
				let length = u32::from_be_bytes(buffer[4..8].try_into().unwrap());
				if length as usize != buffer.len() - 8 {
					tracing::info!("Failed to interpret input event message: expected {length} bytes, but buffer has {} bytes left.", buffer.len() - 8);
					return Err(());
				}

				Ok(Self::InputData(&buffer[8..]))
			},
			ControlMessageType::InvalidateReferenceFrames => Ok(Self::InvalidateReferenceFrames),
			ControlMessageType::RequestIdrFrame => Ok(Self::RequestIdrFrame),
			ControlMessageType::StartA => Ok(Self::StartA),
			ControlMessageType::StartB => Ok(Self::StartB),
		}
	}
}

#[derive(Debug)]
struct EncryptedControlMessage {
	length: u16,
	sequence_number: u32,
	tag: [u8; 16],
	payload: Vec<u8>,
}

impl EncryptedControlMessage {
	fn as_bytes(&self) -> Vec<u8> {
		let mut buffer = Vec::with_capacity(self.length as usize);

		buffer.extend((ControlMessageType::Encrypted as u16).to_le_bytes());
		buffer.extend(self.length.to_le_bytes());
		buffer.extend(self.sequence_number.to_le_bytes());
		buffer.extend(self.tag);
		buffer.extend(&self.payload);

		buffer
	}
}

fn encode_control(key: &[u8], sequence_number: u32, payload: &[u8]) -> Result<Vec<u8>, ()> {
	let mut initialization_vector = [0u8; 12];
	initialization_vector[0..4].copy_from_slice(&sequence_number.to_le_bytes());
	initialization_vector[10] = b'H';
	initialization_vector[11] = b'C';

	let cipher = cipher::Cipher::aes_128_gcm();

	if key.len() != cipher.key_length() {
		tracing::warn!("Key length has {} bytes, but expected {} bytes.", key.len(), cipher.key_length());
		return Err(());
	}

	let mut tag = [0u8; 16];
	let payload = encrypt(cipher, payload, Some(key), Some(&initialization_vector), Some(&mut tag), true)
		.map_err(|e| tracing::warn!("Failed to encrypt control data: {e}"))?;

	if payload.is_empty() {
		tracing::warn!("Failed to encrypt control data.");
		return Err(());
	}

	let message = EncryptedControlMessage {
		length:
			std::mem::size_of::<u32>() as u16 // Sequence number.
			 + ENCRYPTION_TAG_LENGTH as u16   // Tag.
			 + payload.len() as u16,          // Payload.
		sequence_number,
		tag,
		payload,
	};

	Ok(message.as_bytes())
}

enum ControlStreamCommand {
	UpdateKeys(SessionKeys),
}

pub struct ControlStream {
	command_tx: mpsc::Sender<ControlStreamCommand>,
}

impl ControlStream {
	#[allow(clippy::result_unit_err)]
	pub fn new(
		config: Config,
		video_stream: VideoStream,
		audio_stream: AudioStream,
		context: SessionContext,
		stop_signal: ShutdownManager<()>,
	) -> Result<Self, ()> {
		let input_handler = InputHandler::new()?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = ControlStreamInner { };
		tokio::task::spawn_blocking({
			move || {
				tokio::runtime::Handle::current().block_on(
					stop_signal.wrap_cancel(stop_signal.wrap_trigger_shutdown((), inner.run(
						config,
						command_rx,
						video_stream,
						audio_stream,
						context,
						input_handler,
					)))
				)
			}
		});

		Ok(Self { command_tx })
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(ControlStreamCommand::UpdateKeys(keys)).await
			.map_err(|e| tracing::error!("Failed to send UpdateKeys command: {e}"))
	}
}

struct ControlStreamInner { }

impl ControlStreamInner {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub async fn run(
		&self,
		config: Config,
		mut command_rx: mpsc::Receiver<ControlStreamCommand>,
		video_stream: VideoStream,
		audio_stream: AudioStream,
		mut context: SessionContext,
		input_handler: InputHandler,
	) -> Result<(), ()> {
		let socket_address = SocketAddr::new(
			config.address.parse()
				.map_err(|e| tracing::error!("Failed to parse address ({}): {e}", config.address))?,
			config.stream.control.port,
		);
		let socket = UdpSocket::bind(socket_address)
			.map_err(|e| tracing::error!("Failed to bind to socket address ({}): {e}", socket_address))?;
		let mut host = rusty_enet::Host::new(
			socket,
			enet::HostSettings {
				peer_limit: 1,
				channel_limit: 1,
				..Default::default()
			}
		)
			.map_err(|e| tracing::error!("Failed to create control host: {e}"))?;

		tracing::debug!("Listening for control messages on {:?}", socket_address);

		let mut stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(config.stream_timeout);

		// Create a channel over which we can receive feedback messages to send to the connected client.
		let (feedback_tx, mut feedback_rx) = mpsc::channel(10);

		// Sequence number of feedback messages.
		let mut sequence_number = 0u32;

		loop {
			// Check if we received a command.
			let command = command_rx.try_recv();
			match command {
				Ok(command) => {
					match command {
						ControlStreamCommand::UpdateKeys(keys) => {
							tracing::debug!("Updating session keys.");
							context.keys = keys;
						},
					}
				},
				Err(TryRecvError::Disconnected) => {
					tracing::debug!("Command channel closed.");
					break;
				},
				Err(TryRecvError::Empty) => { },
			}

			// Check if the timeout has passed.
			if std::time::Instant::now() > stop_deadline {
				tracing::info!("Stopping because we haven't received a ping for {} seconds.", config.stream_timeout);
				break;
			}

			// Check for feedback messages.
			if let Ok(FeedbackCommand::Rumble(command)) = feedback_rx.try_recv() {
				tracing::debug!("Sending rumble command: {command:?}");
				let payload = command.as_packet();
				let packet = encode_control(&context.keys.remote_input_key, sequence_number, &payload);

				if let Ok(packet) = packet {
					for peer in host.connected_peers_mut() {
						let _ = peer.send(0, &enet::Packet::reliable(&packet))
							.map_err(|e| tracing::warn!("Failed to send rumble to peer: {e}"));
					}
				}

				sequence_number += 1;
			}

			match host.service().map_err(|e| tracing::error!("Failure in enet host: {e}"))? {
				Some(enet::Event::Connect { .. }) => {},
				Some(enet::Event::Disconnect { .. }) => {},
				Some(enet::Event::Receive {
					packet,
					..
				}) => {
					let mut control_message = ControlMessage::from_bytes(packet.data())?;
					tracing::trace!("Received control message: {control_message:?}");

					// First check for encrypted control messages and decrypt them.
					let decrypted;
					if let ControlMessage::Encrypted(message) = control_message {
						let mut initialization_vector = [0u8; 12];
						initialization_vector[0..4].copy_from_slice(&message.sequence_number.to_le_bytes());
						initialization_vector[10] = b'C';
						initialization_vector[11] = b'C';

						let decrypted_result = openssl::symm::decrypt_aead(
							symm::Cipher::aes_128_gcm(),
							&context.keys.remote_input_key,
							Some(&initialization_vector),
							&[],
							&message.payload,
							&message.tag,
						);

						decrypted = match decrypted_result {
							Ok(decrypted) => decrypted,
							Err(e) => {
								tracing::error!("Failed to decrypt control message: {:?}", e.errors());
								continue;
							}
						};

						control_message = match ControlMessage::from_bytes(&decrypted) {
							Ok(decrypted_message) => decrypted_message,
							Err(()) => continue,
						};

						tracing::trace!("Decrypted control message: {control_message:?}");
					}

					match control_message {
						ControlMessage::Encrypted(_) => unreachable!("Encrypted control messages should be decrypted already."),
						ControlMessage::RequestIdrFrame | ControlMessage::InvalidateReferenceFrames => {
							video_stream.request_idr_frame().await?;
						},
						ControlMessage::StartB => {
							audio_stream.start(context.keys.clone()).await?;
							video_stream.start().await?;
						},
						ControlMessage::Ping => {
							stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(config.stream_timeout);
						},
						ControlMessage::InputData(event) => {
							let _ = input_handler.handle_raw_input(event, feedback_tx.clone()).await;
						},
						skipped_message => {
							tracing::trace!("Skipped control message: {skipped_message:?}");
						},
					};
				}
				_ => (),
			}

			std::thread::sleep(std::time::Duration::from_millis(1));
		}

		tracing::debug!("Control stream closing.");
		Ok(())
	}
}
