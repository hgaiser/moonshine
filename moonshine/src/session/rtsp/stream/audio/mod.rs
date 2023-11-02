use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::{config::Config, session::rtsp::stream::audio::{capture::AudioCapture, encoder::AudioEncoder}};

mod capture;
mod encoder;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub packet_duration: u32,
	pub remote_input_key: Vec<u8>,
	pub remote_input_key_id: i64,
	pub qos: bool,
}

enum AudioStreamCommand {
	Start,
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
		tokio::spawn(inner.run(config, context, command_rx, stop_signal));

		AudioStream { command_tx }
	}

	pub async fn start(&self) -> Result<(), ()> {
		self.command_tx.send(AudioStreamCommand::Start).await
			.map_err(|e| log::error!("Failed to send Start command: {e}"))
	}
}

impl AudioStreamInner {
	async fn run(
		mut self,
		config: Config,
		context: AudioStreamContext,
		mut command_rx: mpsc::Receiver<AudioStreamCommand>,
		_stop_signal: ShutdownManager<()>,
	) -> Result<(), ()> {
		let socket = UdpSocket::bind((config.address, config.stream.audio.port)).await
			.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// TODO: Check this value 224, what does it mean exactly?
			log::debug!("Enabling QoS on audio socket.");
			socket.set_tos(224)
				.map_err(|e| log::error!("Failed to set QoS on the audio socket: {e}"))?;
		}

		log::info!(
			"Listening for audio messages on {}",
			socket.local_addr()
			.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let (packet_tx, mut packet_rx) = mpsc::channel::<Vec<u8>>(10);
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
								log::info!("Failed to receive packets from encoder, channel closed.");
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
				AudioStreamCommand::Start => {
					log::info!("Starting audio stream.");

					let (audio_tx, audio_rx) = mpsc::channel(10);
					let capture = match AudioCapture::new(audio_tx) {
						Ok(capture) => capture,
						Err(()) => continue,
					};

					let encoder = match AudioEncoder::new(
						capture.stream_config(),
						audio_rx,
						context.remote_input_key.clone(),
						context.remote_input_key_id,
						packet_tx.clone()
					) {
						Ok(encoder) => encoder,
						Err(()) => continue,
					};

					self.capture = Some(capture);
					self.encoder = Some(encoder);
				},
			}
		}

		log::info!("Command channel closed.");
		Ok(())
	}

}
