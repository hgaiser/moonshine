use enet::{
	Address,
	BandwidthLimit,
	ChannelLimit,
	Enet,
	Event,
	Host,
};
use tokio::sync::mpsc;

use super::video_stream::VideoCommand;

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
			log::error!("Expected control message to have at least two bytes, got {}", buffer.len());
			return Err(());
		}

		match u16::from_le_bytes(buffer[..2].try_into().unwrap()).try_into() {
			Ok(ControlMessageType::Encrypted) => {
				let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
				let sequence_number = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
				Ok(Self::Encrypted(EncryptedControlMessage {
					_length: length,
					sequence_number,
					payload: buffer[8..].to_vec(),
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
	payload: Vec<u8>,
}

pub(super) struct ControlStream {
	host: Host<()>,
}

impl ControlStream {
	pub(super) fn new(address: &str, port: u16) -> Result<Self, ()> {
		let enet = Enet::new()
			.map_err(|e| log::error!("Failed to initialize Enet session: {e}"))?;

		let local_addr = Address::new(
			address.parse()
				.map_err(|e| log::error!("Failed to parse address: {e}"))?,
			port,
		);
		let host = enet
			.create_host::<()>(
				Some(&local_addr),
				10,
				ChannelLimit::Maximum,
				BandwidthLimit::Unlimited,
				BandwidthLimit::Unlimited,
			)
			.unwrap();

		Ok(Self { host })
	}

	pub(super) async fn run(
		mut self,
		video_command_tx: mpsc::Sender<VideoCommand>,
	) -> Result<(), ()> {
		log::info!("Listening for control messages on {:?}", self.host.address());

		loop {
			match self.host.service(1000)
				.map_err(|e| log::error!("Failure in enet host: {e}"))? {
				Some(Event::Connect(_)) => {}, //println!("new connection!"),
				Some(Event::Disconnect(..)) => {}, //println!("disconnect!"),
				Some(Event::Receive {
					ref packet,
					..
				}) => {
					let control_message = ControlMessage::from_bytes(packet.data())?;
					log::info!("Received control message: {control_message:?}");

					match control_message {
						ControlMessage::InvalidateReferenceFrames | ControlMessage::RequestIdrFrame => {
							video_command_tx.send(VideoCommand::RequestIdrFrame).await
								.map_err(|e| log::error!("Failed to send video command: {e}"))?;
						},
						ControlMessage::StartB => {
							video_command_tx.send(VideoCommand::StartStreaming).await
								.map_err(|e| log::error!("Failed to send video command: {e}"))?;
						},
						skipped_message => {
							log::trace!("Skipped control message: {skipped_message:?}");
						},
					};
					// println!(
					// 	"got packet on channel {}, content: '{:?}'",
					// 	channel_id,
					// 	std::str::from_utf8(packet.data())
					// );
				}
				_ => (),
			}
		}
	}
}
