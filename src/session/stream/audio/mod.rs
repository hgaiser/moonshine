use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::{
	config::Config,
	session::{manager::SessionShutdownReason, SessionKeys},
};

use self::{capture::AudioCapture, encoder::AudioEncoder};

mod capture;
mod encoder;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub _packet_duration: u32,
	pub qos: bool,
	pub sink_name: Option<String>,
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
				.set_tos(224)
				.map_err(|e| tracing::error!("Failed to set QoS on the audio socket: {e}"))?;
		}

		tracing::debug!(
			"Listening for audio messages on {}",
			socket
				.local_addr()
				.map_err(|e| tracing::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = AudioStreamInner {
			capture: None,
			encoder: None,
			sink_name: context.sink_name,
		};
		tokio::spawn(inner.run(socket, command_rx, stop_session_manager.clone()));

		Ok(AudioStream { command_tx })
	}

	pub async fn start(&self, keys: SessionKeys) -> Result<(), ()> {
		tracing::debug!("Starting audio stream.");

		self.command_tx
			.send(AudioStreamCommand::Start(keys))
			.await
			.map_err(|e| tracing::error!("Failed to send Start command: {e}"))
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		tracing::info!("Updating audio stream keys.");

		self.command_tx
			.send(AudioStreamCommand::UpdateKeys(keys))
			.await
			.map_err(|e| tracing::error!("Failed to send UpdateKeys command: {e}"))
	}
}

struct AudioStreamInner {
	capture: Option<AudioCapture>,
	encoder: Option<AudioEncoder>,
	sink_name: Option<String>,
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

					let (audio_tx, audio_rx) = mpsc::channel(10);
					let capture =
						match AudioCapture::new(audio_tx, stop_session_manager.clone(), self.sink_name.clone()).await {
							Ok(capture) => capture,
							Err(()) => break,
						};

					let encoder = match AudioEncoder::new(
						capture.sample_rate(),
						capture.channels(),
						audio_rx,
						keys.clone(),
						packet_tx.clone(),
						stop_session_manager.clone(),
					) {
						Ok(encoder) => encoder,
						Err(()) => break,
					};

					self.capture = Some(capture);
					self.encoder = Some(encoder);

					started_streaming = true;
				},

				AudioStreamCommand::UpdateKeys(keys) => {
					let Some(encoder) = &self.encoder else {
						tracing::error!("Can't update session keys, there is no encoder to update.");
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
