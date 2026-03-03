use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::{
	config::Config,
	session::{manager::SessionShutdownReason, SessionKeys},
};

use self::encoder::AudioEncoder;
use self::pulse_server::{PulseServer, CAPTURE_SAMPLE_RATE};

mod buffer;
mod encoder;
mod pulse_server;

/// Opus multistream configuration for a specific channel layout.
#[derive(Clone, Debug)]
pub struct OpusStreamConfig {
	pub channels: u8,
	pub streams: u8,
	pub coupled_streams: u8,
	pub mapping: [u8; 8],
	pub bitrate: u32,
}

/// Pre-defined Opus stream configurations matching Sunshine's behavior.
pub const OPUS_STEREO: OpusStreamConfig = OpusStreamConfig {
	channels: 2,
	streams: 1,
	coupled_streams: 1,
	mapping: [0, 1, 0, 0, 0, 0, 0, 0],
	bitrate: 96_000,
};

pub const OPUS_HIGH_STEREO: OpusStreamConfig = OpusStreamConfig {
	channels: 2,
	streams: 1,
	coupled_streams: 1,
	mapping: [0, 1, 0, 0, 0, 0, 0, 0],
	bitrate: 512_000,
};

pub const OPUS_SURROUND51: OpusStreamConfig = OpusStreamConfig {
	channels: 6,
	streams: 4,
	coupled_streams: 2,
	mapping: [0, 1, 4, 5, 2, 3, 0, 0],
	bitrate: 256_000,
};

pub const OPUS_HIGH_SURROUND51: OpusStreamConfig = OpusStreamConfig {
	channels: 6,
	streams: 6,
	coupled_streams: 0,
	mapping: [0, 1, 2, 3, 4, 5, 0, 0],
	bitrate: 1_536_000,
};

pub const OPUS_SURROUND71: OpusStreamConfig = OpusStreamConfig {
	channels: 8,
	streams: 5,
	coupled_streams: 3,
	mapping: [0, 1, 4, 5, 6, 7, 2, 3],
	bitrate: 450_000,
};

pub const OPUS_HIGH_SURROUND71: OpusStreamConfig = OpusStreamConfig {
	channels: 8,
	streams: 8,
	coupled_streams: 0,
	mapping: [0, 1, 2, 3, 4, 5, 6, 7],
	bitrate: 2_048_000,
};

/// All standard configurations, ordered for RTSP DESCRIBE emission.
pub const ALL_STREAM_CONFIGS: [&OpusStreamConfig; 6] = [
	&OPUS_STEREO,
	&OPUS_HIGH_STEREO,
	&OPUS_SURROUND51,
	&OPUS_HIGH_SURROUND51,
	&OPUS_SURROUND71,
	&OPUS_HIGH_SURROUND71,
];

/// Audio configuration negotiated between client and server.
#[derive(Clone, Debug)]
pub struct AudioConfig {
	pub channels: u8,
	pub channel_mask: u32,
	pub high_quality: bool,
	pub stream_config: OpusStreamConfig,
}

impl Default for AudioConfig {
	fn default() -> Self {
		Self {
			channels: 2,
			channel_mask: 0x3,
			high_quality: true,
			stream_config: OPUS_HIGH_STEREO,
		}
	}
}

impl AudioConfig {
	/// Select the appropriate OpusStreamConfig based on channel count and quality.
	pub fn from_channels(channels: u8, channel_mask: u32, high_quality: bool) -> Self {
		let stream_config = match (channels, high_quality) {
			(6, false) => OPUS_SURROUND51,
			(6, true) => OPUS_HIGH_SURROUND51,
			(8, false) => OPUS_SURROUND71,
			(8, true) => OPUS_HIGH_SURROUND71,
			(_, false) => OPUS_STEREO,
			(_, true) => OPUS_HIGH_STEREO,
		};
		Self {
			channels: stream_config.channels,
			channel_mask,
			high_quality,
			stream_config,
		}
	}
}

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub packet_duration_ms: u32,
	pub qos: bool,
	pub audio_config: AudioConfig,
}

enum AudioStreamCommand {
	Start(SessionKeys),
	UpdateKeys(SessionKeys),
}

#[derive(Clone)]
pub struct AudioStream {
	command_tx: mpsc::Sender<AudioStreamCommand>,
}

impl AudioStream {
	pub async fn new(
		config: Config,
		context: AudioStreamContext,
		listener: std::os::unix::net::UnixListener,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing audio stream.");

		let socket = UdpSocket::bind((config.address, config.stream.audio.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// TODO: Check this value 224, what does it mean exactly?
			tracing::debug!("Enabling QoS on audio socket.");
			socket
				.set_tos_v4(224)
				.map_err(|e| tracing::warn!("Failed to set QoS on the audio socket: {e}"))?;
		}

		tracing::debug!(
			"Listening for audio messages on {}",
			socket
				.local_addr()
				.map_err(|e| tracing::warn!("Failed to get local address associated with control socket: {e}"))?
		);

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = AudioStreamInner {
			encoder: None,
			listener: Some(listener),
			packet_duration_ms: context.packet_duration_ms,
			audio_config: context.audio_config,
			pulse_server_close_tx: None,
			pulse_server_waker: None,
		};
		tokio::spawn(inner.run(socket, command_rx, stop_session_manager.clone()));

		Ok(AudioStream { command_tx })
	}

	pub async fn start(&self, keys: SessionKeys) -> Result<(), ()> {
		tracing::debug!("Starting audio stream.");

		self.command_tx
			.send(AudioStreamCommand::Start(keys))
			.await
			.map_err(|e| tracing::warn!("Failed to send Start command: {e}"))
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		tracing::debug!("Updating audio stream keys.");

		self.command_tx
			.send(AudioStreamCommand::UpdateKeys(keys))
			.await
			.map_err(|e| tracing::warn!("Failed to send UpdateKeys command: {e}"))
	}
}

struct AudioStreamInner {
	encoder: Option<AudioEncoder>,
	listener: Option<std::os::unix::net::UnixListener>,
	packet_duration_ms: u32,
	audio_config: AudioConfig,
	pulse_server_close_tx: Option<crossbeam_channel::Sender<()>>,
	pulse_server_waker: Option<mio::Waker>,
}

impl AudioStreamInner {
	async fn run(
		mut self,
		socket: UdpSocket,
		mut command_rx: mpsc::Receiver<AudioStreamCommand>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		// Trigger session shutdown when the audio stream stops.
		let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::AudioStreamStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(10);
		tokio::spawn(handle_audio_packets(packet_rx, socket, stop_session_manager.clone()));

		let mut started_streaming = false;
		while let Ok(Some(command)) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
			match command {
				AudioStreamCommand::Start(keys) => {
					if started_streaming {
						tracing::warn!("Can't start streaming twice.");
						continue;
					}

					let Some(listener) = self.listener.take() else {
						tracing::error!("No listener available for PulseServer.");
						break;
					};

					// Create frame channels for PulseServer ↔ Encoder communication.
					let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
					let (frame_recycle_tx, frame_recycle_rx) = crossbeam_channel::bounded(3);

					// Start PulseServer.
					let (mut server, close_tx, waker): (PulseServer, _, _) = match PulseServer::new(
						listener,
						self.audio_config.channels,
						self.packet_duration_ms,
						frame_tx,
						frame_recycle_rx,
					) {
						Ok(result) => result,
						Err(e) => {
							tracing::error!("Failed to create PulseServer: {e}");
							break;
						},
					};

					let server_stop = stop_session_manager.clone();
					std::thread::Builder::new()
						.name("pulse-server".to_string())
						.spawn(move || {
							if let Err(e) = server.run() {
								tracing::error!("PulseServer error: {e}");
								let _ = server_stop.trigger_shutdown(SessionShutdownReason::PulseServerStopped);
							}
						})
						.map_err(|e| tracing::error!("Failed to spawn pulse server thread: {e}"))
						.ok();

					self.pulse_server_close_tx = Some(close_tx);
					self.pulse_server_waker = Some(waker);

					let encoder = match AudioEncoder::new(
						CAPTURE_SAMPLE_RATE,
						&self.audio_config.stream_config,
						frame_rx,
						frame_recycle_tx,
						keys.clone(),
						packet_tx.clone(),
						stop_session_manager.clone(),
					) {
						Ok(encoder) => encoder,
						Err(()) => break,
					};

					self.encoder = Some(encoder);

					started_streaming = true;
				},

				AudioStreamCommand::UpdateKeys(keys) => {
					let Some(encoder) = &self.encoder else {
						tracing::warn!("Can't update session keys, there is no encoder to update.");
						continue;
					};

					let _ = encoder.update_keys(keys).await;
				},
			}
		}

		tracing::debug!("Audio stream stopped.");

		// Signal PulseServer to stop and wake its poll loop.
		if let Some(close_tx) = self.pulse_server_close_tx.take() {
			let _ = close_tx.send(());
		}
		if let Some(waker) = self.pulse_server_waker.take() {
			let _ = waker.wake();
		}
	}
}

async fn handle_audio_packets(
	mut packet_rx: mpsc::Receiver<Vec<u8>>,
	socket: UdpSocket,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
) {
	let mut buf = [0; 1024];
	let mut client_address = None;

	// Trigger session shutdown when the audio packet stream stops.
	let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::AudioPacketHandlerStopped);
	let _delay_stop = stop_session_manager.delay_shutdown_token();

	while !stop_session_manager.is_shutdown_triggered() {
		tokio::select! {
			packet = stop_session_manager.wrap_cancel(packet_rx.recv()) => {
				match packet {
					Ok(Some(packet)) => {
						if let Some(client_address) = client_address {
							if let Err(e) = socket.send_to(packet.as_slice(), client_address).await {
								tracing::warn!("Failed to send packet to client: {e}");
							}
						}
					},
					_ => {
						tracing::debug!("Audio packet channel closed.");
						break;
					},
				}
			},

			message = stop_session_manager.wrap_cancel(socket.recv_from(&mut buf)) => {
				let (len, address) = match message {
					Ok(Ok((len, address))) => (len, address),
					Ok(Err(e)) => {
						tracing::warn!("Failed to receive message: {e}");
						break;
					},
					Err(_) => break,
				};

				if &buf[..len] == b"PING" {
					tracing::trace!("Received video stream PING message from {address}.");
					client_address = Some(address);
				} else {
					tracing::warn!("Received unknown message on video stream of length {len}.");
				}
			},
		}
	}

	tracing::debug!("Audio packet stream stopped.");
}
