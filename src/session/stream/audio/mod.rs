use anyhow::{Context, Result};
use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::{config::Config, session::SessionKeys};

use self::{capture::AudioCapture, encoder::AudioEncoder};

mod capture;
mod encoder;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
    #[allow(dead_code)]
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

unsafe impl Send for AudioStreamInner {}

impl AudioStream {
	pub fn new(config: Config, context: AudioStreamContext, stop_signal: ShutdownManager<()>) -> Self {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = AudioStreamInner {
			capture: None,
			encoder: None,
		};
		tokio::spawn(stop_signal.wrap_cancel(
			stop_signal.wrap_trigger_shutdown((), inner.run(config, context, command_rx, stop_signal.clone())),
		));

		AudioStream { command_tx }
	}

	pub async fn start(&self, keys: SessionKeys) -> Result<()> {
		self.command_tx
			.send(AudioStreamCommand::Start(keys))
			.await
			.context("Failed to send Start command")
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<()> {
		self.command_tx
			.send(AudioStreamCommand::UpdateKeys(keys))
			.await
			.context("Failed to send UpdateKeys command")
	}
}

impl AudioStreamInner {
	async fn run(
		mut self,
		config: Config,
		audio_stream_context: AudioStreamContext,
		mut command_rx: mpsc::Receiver<AudioStreamCommand>,
		_stop_signal: ShutdownManager<()>,
	) -> Result<()> {
		let socket = UdpSocket::bind((config.address, config.stream.audio.port))
			.await
			.context("Failed to bind to UDP socket")?;

		if audio_stream_context.qos {
			// TODO: Check this value 224, what does it mean exactly?
			tracing::debug!("Enabling QoS on audio socket.");
			socket.set_tos(224).context("Failed to set QoS on the audio socket")?;
		}

		tracing::debug!(
			"Listening for audio messages on {}",
			socket
				.local_addr()
				.context("Failed to get local address associated with control socket")?
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
										tracing::warn!("Failed to send packet to client: {e}");
									}
								}
							},
							None => {
								tracing::debug!("Packet channel closed.");
								break;
							},
						}
					},

					message = socket.recv_from(&mut buf) => {
						let (len, address) = match message {
							Ok((len, address)) => (len, address),
							Err(e) => {
								tracing::warn!("Failed to receive message: {e}");
								break;
							},
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
		});

		while let Some(command) = command_rx.recv().await {
			match command {
				AudioStreamCommand::Start(keys) => {
					tracing::info!("Starting audio stream.");

					let (audio_tx, audio_rx) = mpsc::channel(10);
					let capture = match AudioCapture::new(audio_tx).await {
						Ok(capture) => capture,
						Err(e) => {
							tracing::error!("Error creating audio capture: {e}");
							continue;
						},
					};

					let encoder = match AudioEncoder::new(
						capture.sample_rate(),
						capture.channels(),
						audio_rx,
						keys.clone(),
						packet_tx.clone(),
					) {
						Ok(encoder) => encoder,
						Err(e) => {
							tracing::error!("Error creating audio encoder: {e}");
							continue;
						},
					};

					self.capture = Some(capture);
					self.encoder = Some(encoder);
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

		tracing::debug!("Command channel closed.");
		Ok(())
	}
}
