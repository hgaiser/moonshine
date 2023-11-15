use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::{config::Config, session::SessionKeys};

use self::{capture::AudioCapture, encoder::AudioEncoder};

mod capture;
mod encoder;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub packet_duration: u32,
	pub qos: bool,
}

enum AudioStreamCommand {
	Start(SessionKeys),
	UpdateKeys(SessionKeys),
}

#[derive(Clone)]
pub struct AudioStream {
	command_tx: mpsc::Sender<AudioStreamCommand>,
}

struct AudioStreamInner {
	capture: Option<AudioCapture>,
	encoder: Option<AudioEncoder>,
}

unsafe impl Send for AudioStreamInner { }

impl AudioStream {
	pub fn new(
		config: Config,
		context: AudioStreamContext,
		stop_signal: ShutdownManager<()>,
	) -> Self {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = AudioStreamInner { capture: None, encoder: None };
		tokio::spawn(stop_signal.wrap_cancel(stop_signal.wrap_trigger_shutdown((), inner.run(
			config,
			context,
			command_rx,
			stop_signal.clone(),
		))));

		AudioStream { command_tx }
	}

	pub async fn start(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(AudioStreamCommand::Start(keys)).await
			.map_err(|e| log::error!("Failed to send Start command: {e}"))
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(AudioStreamCommand::UpdateKeys(keys)).await
			.map_err(|e| log::error!("Failed to send UpdateKeys command: {e}"))
	}
}

impl AudioStreamInner {
	async fn run(
		mut self,
		config: Config,
		audio_stream_context: AudioStreamContext,
		mut command_rx: mpsc::Receiver<AudioStreamCommand>,
		_stop_signal: ShutdownManager<()>,
	) -> Result<(), ()> {
		let socket = UdpSocket::bind((config.address, config.stream.audio.port)).await
			.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

		if audio_stream_context.qos {
			// TODO: Check this value 224, what does it mean exactly?
			log::debug!("Enabling QoS on audio socket.");
			socket.set_tos(224)
				.map_err(|e| log::error!("Failed to set QoS on the audio socket: {e}"))?;
		}

		log::debug!(
			"Listening for audio messages on {}",
			socket.local_addr()
			.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let (packet_tx, mut packet_rx) = mpsc::channel::<Vec<u8>>(1024);
		tokio::spawn(async move {
			let mut buf = [0; 1024];
			let mut client_address = None;

			loop {
				tokio::select! {
					packet = packet_rx.recv() => {
						match packet {
							Some(packet) => {
								if let Some(client_address) = client_address {
									if let Err(e) = socket.send_to(packet.as_slice(), client_address).await {
										log::warn!("Failed to send packet to client: {e}");
									}
								}
							},
							None => {
								log::debug!("Packet channel closed.");
								break;
							},
						}
					},

					message = socket.recv_from(&mut buf) => {
						let (len, address) = match message {
							Ok((len, address)) => (len, address),
							Err(e) => {
								log::warn!("Failed to receive message: {e}");
								break;
							},
						};

						if &buf[..len] == b"PING" {
							log::trace!("Received video stream PING message from {address}.");
							client_address = Some(address);
						} else {
							log::warn!("Received unknown message on video stream of length {len}.");
						}
					},
				}
			}
		});

		while let Some(command) = command_rx.recv().await {
			#[allow(clippy::single_match)] // There will be more in the future.
			match command {
				AudioStreamCommand::Start(keys) => {
					log::info!("Starting audio stream.");

					let (audio_tx, audio_rx) = mpsc::channel(10);
					let capture = match AudioCapture::new(audio_tx) {
						Ok(capture) => capture,
						Err(()) => continue,
					};

					let encoder = match AudioEncoder::new(
						capture.stream_config(),
						audio_rx,
						keys.clone(),
						packet_tx.clone()
					) {
						Ok(encoder) => encoder,
						Err(()) => continue,
					};

					self.capture = Some(capture);
					self.encoder = Some(encoder);
				},

				AudioStreamCommand::UpdateKeys(keys) => {
					let Some(encoder) = &self.encoder else {
						log::error!("Can't update session keys, there is no encoder to update.");
						continue;
					};

					let _ = encoder.update_keys(keys).await;
				},
			}
		}

		log::debug!("Command channel closed.");
		Ok(())
	}

}
