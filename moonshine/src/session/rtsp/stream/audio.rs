use tokio::net::UdpSocket;

use crate::config::Config;

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	pub packet_duration: u32,
}

pub(super) async fn run_audio_stream(
	config: Config,
	_context: AudioStreamContext,
) -> Result<(), ()> {
	let socket = UdpSocket::bind((config.address, config.stream.audio.port)).await
		.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

	log::info!(
		"Listening for audio messages on {}",
		socket.local_addr()
		.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
	);

	let mut buf = [0; 1024];
	for _ in 0.. {
		match socket.recv_from(&mut buf).await {
			Ok((len, addr)) => {
				if &buf[..len] == b"PING" {
					log::debug!("Received audio stream PING message from {addr}.");
				} else {
					log::warn!("Received unknown message on audio stream of length {len}.");
				}
			},
			Err(ref e) => {
				if e.kind() != std::io::ErrorKind::WouldBlock {
					log::error!("Failed to receive UDP message: {e}");
					return Err(());
				}
			}
		}
	}

	Ok(())
}
