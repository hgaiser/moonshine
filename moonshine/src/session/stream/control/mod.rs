use async_shutdown::ShutdownManager;
use enigo::{Enigo, MouseControllable, KeyboardControllable};
use enet::{
	Address,
	BandwidthLimit,
	ChannelLimit,
	Enet,
	Event,
};
use openssl::symm::Cipher;
use tokio::sync::mpsc::{self, error::TryRecvError};

use crate::{session::{SessionContext, SessionKeys}, config::Config};

use self::input::InputEvent;

use super::{VideoStream, AudioStream};

mod input;

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
enum ControlMessage {
	Encrypted(EncryptedControlMessage),
	Ping,
	Termination,
	RumbleData,
	LossStats,
	FrameStats,
	InputData(InputEvent),
	InvalidateReferenceFrames,
	RequestIdrFrame,
	StartA,
	StartB,
}

impl ControlMessage {
	fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			log::warn!("Expected control message to have at least 4 bytes, got {}", buffer.len());
			return Err(());
		}

		let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
		if length as usize != buffer.len() - 4 {
			log::info!("Received incorrect packet length: expecting {length} bytes, but buffer says it should be {} bytes.", buffer.len() - 4);
			return Err(());
		}

		match u16::from_le_bytes(buffer[..2].try_into().unwrap()).try_into()? {
			ControlMessageType::Encrypted => {
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
			ControlMessageType::Ping => Ok(Self::Ping),
			ControlMessageType::Termination => Ok(Self::Termination),
			ControlMessageType::RumbleData => Ok(Self::RumbleData),
			ControlMessageType::LossStats => Ok(Self::LossStats),
			ControlMessageType::FrameStats => Ok(Self::FrameStats),
			ControlMessageType::InputData => {
				// Length of the input event, excluding the length itself.
				let length = u32::from_be_bytes(buffer[4..8].try_into().unwrap());
				if length as usize != buffer.len() - 8 {
					log::info!("Failed to interpret input event message: expected {length} bytes, but buffer has {} bytes left.", buffer.len() - 8);
					return Err(());
				}

				Ok(Self::InputData(InputEvent::from_bytes(&buffer[8..])?))
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
	_length: u16,
	sequence_number: u32,
	tag: [u8; 16],
	payload: Vec<u8>,
}

enum ControlStreamCommand {
	UpdateKeys(SessionKeys),
}

pub struct ControlStream {
	command_tx: mpsc::Sender<ControlStreamCommand>,
}

impl ControlStream {
	pub fn new(
		config: Config,
		video_stream: VideoStream,
		audio_stream: AudioStream,
		context: SessionContext,
		enet: Enet,
		stop_signal: ShutdownManager<()>,
	) -> Self {
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
						enet,
					)))
				)
			}
		});

		Self { command_tx }
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(ControlStreamCommand::UpdateKeys(keys)).await
			.map_err(|e| log::error!("Failed to send UpdateKeys command: {e}"))
	}
}

struct ControlStreamInner {
}

impl ControlStreamInner {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub async fn run(
		&self,
		config: Config,
		mut command_rx: mpsc::Receiver<ControlStreamCommand>,
		video_stream: VideoStream,
		audio_stream: AudioStream,
		mut context: SessionContext,
		enet: Enet,
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
			.map_err(|e| log::error!("Failed to create Enet host: {e}"))?;

		let mut enigo = Enigo::new();

		log::debug!("Listening for control messages on {:?}", host.address());

		let mut stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(config.stream_timeout);

		loop {
			// Check if we received a command.
			let command = command_rx.try_recv();
			match command {
				Ok(command) => {
					match command {
						ControlStreamCommand::UpdateKeys(keys) => {
							log::debug!("Updating session keys.");
							context.keys = keys;
						},
					}
				},
				Err(TryRecvError::Disconnected) => {
					log::debug!("Command channel closed.");
					break;
				},
				Err(TryRecvError::Empty) => { },
			}

			// Check if the timeout has passed.
			if std::time::Instant::now() > stop_deadline {
				log::info!("Stopping because we haven't received a ping for {} seconds.", config.stream_timeout);
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
							&context.keys.remote_input_key,
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
							Err(()) => continue,
						};

						log::trace!("Decrypted control message: {control_message:?}");
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
						ControlMessage::InputData(message) => {
							log::trace!("Received input event: {message:?}");
							match message {
								InputEvent::KeyDown(event) => {
									enigo.key_down(enigo::Key::Raw(event.key));
									log::info!("Key down event: {event:?}");
								},
								InputEvent::KeyUp(event) => {
									enigo.key_up(enigo::Key::Raw(event.key));
									log::info!("Key up event: {event:?}");
								},
								InputEvent::MouseMoveAbsolute(event) => {
									let display_size = enigo.main_display_size();
									enigo.mouse_move_to(
										event.x as i32 * display_size.0 / event.width as i32,
										event.y as i32 * display_size.1 / event.height as i32,
									);
								},
								InputEvent::MouseMoveRelative(event) => {
									enigo.mouse_move_relative(event.x as i32, event.y as i32);
								},
								InputEvent::MouseButtonDown(button) => {
									enigo.mouse_down(button.into());
								},
								InputEvent::MouseButtonUp(button) => {
									enigo.mouse_up(button.into());
								},
							}
						},
						skipped_message => {
							log::trace!("Skipped control message: {skipped_message:?}");
						},
					};
				}
				_ => (),
			}
		}

		log::debug!("Control stream closing.");
		Ok(())
	}
}
