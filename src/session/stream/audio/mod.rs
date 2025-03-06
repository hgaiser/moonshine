use async_shutdown::{ShutdownManager, TriggerShutdownToken};
use tokio::{net::UdpSocket, sync::{mpsc, oneshot}};

use crate::{config::Config, session::SessionKeys};

use self::{capture::AudioCapture, encoder::AudioEncoder};

mod capture;
mod encoder;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub _packet_duration: u32,
	pub qos: bool,
}

enum AudioStreamCommand {
	Start(SessionKeys),
	Stop(oneshot::Sender<()>),
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
		stop_signal: ShutdownManager<()>,
	) -> Result<Self, ()> {
		tracing::info!("Starting audio stream.");

		let socket = UdpSocket::bind((config.address, config.stream.audio.port)).await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// TODO: Check this value 224, what does it mean exactly?
			tracing::debug!("Enabling QoS on audio socket.");
			socket.set_tos(224)
				.map_err(|e| tracing::error!("Failed to set QoS on the audio socket: {e}"))?;
		}

		tracing::debug!(
			"Listening for audio messages on {}",
			socket.local_addr()
				.map_err(|e| tracing::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = AudioStreamInner { capture: None, encoder: None };
		tokio::spawn(stop_signal.wrap_cancel(stop_signal.wrap_trigger_shutdown((), inner.run(
			socket,
			command_rx,
			stop_signal.trigger_shutdown_token(()),
		))));

		Ok(AudioStream { command_tx })
	}

	pub async fn start(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(AudioStreamCommand::Start(keys)).await
			.map_err(|e| tracing::error!("Failed to send Start command: {e}"))
	}

	pub async fn stop(&self) -> Result<(), ()> {
		tracing::info!("Stopping audio stream.");
		let (result_tx, result_rx) = oneshot::channel();
		self.command_tx.send(AudioStreamCommand::Stop(result_tx))
			.await
			.map_err(|e| tracing::error!("Failed to send Stop command: {e}"))?;
		result_rx.await
			.map_err(|e| tracing::error!("Failed to wait for result from Stop command: {e}"))
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(AudioStreamCommand::UpdateKeys(keys)).await
			.map_err(|e| tracing::error!("Failed to send UpdateKeys command: {e}"))
	}
}

struct AudioStreamInner {
	capture: Option<AudioCapture>,
	encoder: Option<AudioEncoder>,
}

unsafe impl Send for AudioStreamInner { }

impl AudioStreamInner {
	async fn run(
		mut self,
		socket: UdpSocket,
		mut command_rx: mpsc::Receiver<AudioStreamCommand>,
		stop_token: TriggerShutdownToken<()>,
	) {
		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(10);
		tokio::spawn(handle_audio_packets(packet_rx, socket));

		while let Some(command) = command_rx.recv().await {
			match command {
				AudioStreamCommand::Start(keys) => {
					let (audio_tx, audio_rx) = mpsc::channel(10);
					let capture = match AudioCapture::new(audio_tx, stop_token.clone()).await {
						Ok(capture) => capture,
						Err(()) => break,
					};

					let encoder = match AudioEncoder::new(
						capture.sample_rate(),
						capture.channels(),
						audio_rx,
						keys.clone(),
						packet_tx.clone(),
						stop_token.clone(),
					) {
						Ok(encoder) => encoder,
						Err(()) => break,
					};

					self.capture = Some(capture);
					self.encoder = Some(encoder);
				},

				AudioStreamCommand::Stop(result_tx) => {
					if let Some(capture) = self.capture.take() {
						let _ = capture.stop().await;
					}
					if let Some(encoder) = self.encoder.take() {
						let _ = encoder.stop().await;
					}
					let _ = result_tx.send(());
					stop_token.forget();
					break;
				}

				AudioStreamCommand::UpdateKeys(keys) => {
					let Some(encoder) = &self.encoder else {
						tracing::error!("Can't update session keys, there is no encoder to update.");
						continue;
					};

					let _ = encoder.update_keys(keys).await;
				},
			}
		}

		tracing::info!("Audio stream stopped.");
	}
}

async fn handle_audio_packets(mut packet_rx: mpsc::Receiver<Vec<u8>>, socket: UdpSocket) {
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
						tracing::debug!("Audio packet channel closed.");
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

	tracing::info!("Audio packet stream stopped.");
}
