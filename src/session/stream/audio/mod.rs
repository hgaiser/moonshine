use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::{
	config::Config,
	session::{manager::SessionShutdownReason, SessionKeys},
};

use self::encoder::AudioEncoder;
use self::pulse_server::{PulseServer, CAPTURE_CHANNEL_COUNT, CAPTURE_SAMPLE_RATE};

mod buffer;
mod encoder;
mod pulse_server;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub _packet_duration: u32,
	pub qos: bool,
	pub socket_path: Option<PathBuf>,
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
			socket_path: context.socket_path,
			pulse_server_close_tx: None,
			_pulse_server_waker: None,
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
	socket_path: Option<PathBuf>,
	pulse_server_close_tx: Option<crossbeam_channel::Sender<()>>,
	_pulse_server_waker: Option<mio::Waker>,
}

unsafe impl Send for AudioStreamInner {}

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

					let Some(ref socket_path) = self.socket_path else {
						tracing::error!("No socket path configured for PulseServer.");
						break;
					};

					// Create frame channels for PulseServer ↔ Encoder communication.
					let (frame_tx, frame_rx) = crossbeam_channel::unbounded();
					let (frame_recycle_tx, frame_recycle_rx) = crossbeam_channel::unbounded();

					// Start PulseServer.
					let (mut server, close_tx, waker): (PulseServer, _, _) =
						match PulseServer::new(socket_path, frame_tx, frame_recycle_rx) {
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
								let _ = server_stop.trigger_shutdown(SessionShutdownReason::AudioCaptureStopped);
							}
						})
						.map_err(|e| tracing::error!("Failed to spawn pulse server thread: {e}"))
						.ok();

					self.pulse_server_close_tx = Some(close_tx);
					self._pulse_server_waker = Some(waker);

					let encoder = match AudioEncoder::new(
						CAPTURE_SAMPLE_RATE,
						CAPTURE_CHANNEL_COUNT as u8,
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
