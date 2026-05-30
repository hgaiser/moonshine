use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use strum_macros::Display;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::{
	config::Config,
	session::{manager::SessionShutdownReason, SessionKeysReceiver},
};

use self::encoder::AudioEncoder;
use self::pulse_server::{PulseServer, CAPTURE_SAMPLE_RATE};

mod buffer;
mod encoder;
mod pulse_server;

/// Number of audio channels requested by the client.
#[derive(Clone, Copy, Debug, Default, Display, PartialEq, PartialOrd)]
pub enum AudioChannels {
	#[default]
	Stereo = 2,
	Surround51 = 6,
	Surround71 = 8,
}

impl From<u8> for AudioChannels {
	fn from(value: u8) -> Self {
		match value {
			6 => Self::Surround51,
			8 => Self::Surround71,
			_ => Self::Stereo,
		}
	}
}

/// Opus multistream configuration for a specific channel layout.
#[derive(Clone, Debug)]
pub struct OpusStreamConfig {
	pub channels: AudioChannels,
	pub streams: u8,
	pub coupled_streams: u8,
	pub mapping: [u8; 8],
	pub bitrate: u32,
}

/// Pre-defined Opus stream configurations matching Sunshine's behavior.
pub const OPUS_STEREO: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Stereo,
	streams: 1,
	coupled_streams: 1,
	mapping: [0, 1, 0, 0, 0, 0, 0, 0],
	bitrate: 96_000,
};

pub const OPUS_HIGH_STEREO: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Stereo,
	streams: 1,
	coupled_streams: 1,
	mapping: [0, 1, 0, 0, 0, 0, 0, 0],
	bitrate: 512_000,
};

pub const OPUS_SURROUND51: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround51,
	streams: 4,
	coupled_streams: 2,
	mapping: [0, 1, 4, 5, 2, 3, 0, 0],
	bitrate: 256_000,
};

pub const OPUS_HIGH_SURROUND51: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround51,
	streams: 6,
	coupled_streams: 0,
	mapping: [0, 1, 2, 3, 4, 5, 0, 0],
	bitrate: 1_536_000,
};

pub const OPUS_SURROUND71: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround71,
	streams: 5,
	coupled_streams: 3,
	mapping: [0, 1, 4, 5, 6, 7, 2, 3],
	bitrate: 450_000,
};

pub const OPUS_HIGH_SURROUND71: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround71,
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
	pub channels: AudioChannels,
	pub channel_mask: u32,
	pub high_quality: bool,
	pub stream_config: OpusStreamConfig,
}

impl Default for AudioConfig {
	fn default() -> Self {
		Self {
			channels: AudioChannels::default(),
			channel_mask: 0x3,
			high_quality: true,
			stream_config: OPUS_HIGH_STEREO,
		}
	}
}

impl AudioConfig {
	/// Select the appropriate OpusStreamConfig based on channel count and quality.
	pub fn from_channels(channels: AudioChannels, channel_mask: u32, high_quality: bool) -> Self {
		let stream_config = match (channels, high_quality) {
			(AudioChannels::Surround51, false) => OPUS_SURROUND51,
			(AudioChannels::Surround51, true) => OPUS_HIGH_SURROUND51,
			(AudioChannels::Surround71, false) => OPUS_SURROUND71,
			(AudioChannels::Surround71, true) => OPUS_HIGH_SURROUND71,
			(_, false) => OPUS_STEREO,
			(_, true) => OPUS_HIGH_STEREO,
		};
		Self {
			channels,
			channel_mask,
			high_quality,
			stream_config,
		}
	}
}

#[derive(Clone, Default)]
pub struct AudioStreamContext {
	/// Duration of each audio packet in milliseconds, typically 20ms for Opus.
	pub packet_duration_ms: u32,
	/// Whether to enable QoS on the audio socket.
	pub qos: bool,
	/// Negotiated audio configuration for the stream.
	pub audio_config: AudioConfig,
	/// Whether the client has enabled audio encryption.
	pub encrypt_audio: bool,
}

/// Handle returned by `AudioStream::start` that gates the encoder and packet handler.
///
/// The encoder and packet handler are spawned immediately but block on a `Notify`
/// until `trigger()` is called. PulseServer starts immediately (it just mixes audio,
/// no network impact).
pub struct AudioStartHandle {
	notify: Arc<Notify>,
}

impl AudioStartHandle {
	/// Signal the encoder and packet handler to begin processing.
	pub fn trigger(&self) {
		self.notify.notify_waiters();
	}
}

pub struct AudioStream {
	pulse_socket: UnixListener,
	pub pulse_socket_path: PathBuf,
	udp_socket: tokio::net::UdpSocket,
	stop: ShutdownManager<SessionShutdownReason>,
}

impl AudioStream {
	pub async fn new(config: Config, stop: ShutdownManager<SessionShutdownReason>) -> Result<Self, ()> {
		tracing::debug!("Initializing audio stream.");

		let udp_socket = UdpSocket::bind((config.address, config.stream.audio.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		// Create the socket directory for the PulseAudio server.
		let pulse_socket_dir = dirs::runtime_dir()
			.ok_or_else(|| tracing::error!("Failed to get runtime directory for PulseAudio socket"))?
			.join("moonshine/pulse");
		std::fs::create_dir_all(&pulse_socket_dir)
			.map_err(|e| tracing::error!("Failed to create pulse socket directory: {e}"))?;
		let pulse_socket_path = pulse_socket_dir.join("native");

		// Remove any stale socket file from a previous session.
		let _ = std::fs::remove_file(&pulse_socket_path);

		// Bind the PulseAudio socket before launching the application so that
		// the app can connect as soon as it starts.
		let pulse_socket = UnixListener::bind(&pulse_socket_path)
			.map_err(|e| tracing::error!("Failed to bind PulseAudio socket: {e}"))?;

		tracing::debug!("Listening for audio messages on {}", pulse_socket_path.display());

		Ok(AudioStream {
			pulse_socket,
			pulse_socket_path,
			udp_socket,
			stop,
		})
	}

	pub fn start(self, context: AudioStreamContext, keys_rx: SessionKeysReceiver) -> Result<AudioStartHandle, ()> {
		// Apply QoS to UDP socket.
		if context.qos {
			let _ = self.udp_socket.set_tos_v4(224);
		}

		// Create the notify gate for encoder and packet handler.
		let start_notify = Arc::new(Notify::new());

		// Create packet channel and spawn handler — gated behind start_notify.
		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(10);
		spawn_handle_audio_packets(packet_rx, self.udp_socket, start_notify.clone(), self.stop.clone());

		// Create frame channels for PulseServer and encoder communication.
		let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
		let (frame_recycle_tx, frame_recycle_rx) = crossbeam_channel::bounded(3);

		// Spawn PulseServer immediately (no gating — it just mixes audio, no network impact).
		PulseServer::spawn(
			self.pulse_socket,
			self.pulse_socket_path.clone(),
			context.audio_config.channels as u8,
			context.packet_duration_ms,
			frame_tx,
			frame_recycle_rx,
			self.stop.clone(),
		)
		.map_err(|e| tracing::error!("Failed to create PulseServer: {e}"))?;

		// Spawn audio encoder — gated behind start_notify.
		AudioEncoder::spawn(
			CAPTURE_SAMPLE_RATE,
			&context.audio_config.stream_config,
			frame_rx,
			frame_recycle_tx,
			keys_rx,
			context.encrypt_audio,
			packet_tx,
			self.stop.clone(),
			start_notify.clone(),
		)?;

		Ok(AudioStartHandle { notify: start_notify })
	}
}

fn spawn_handle_audio_packets(
	mut packet_rx: mpsc::Receiver<Vec<u8>>,
	socket: UdpSocket,
	start: Arc<Notify>,
	stop: ShutdownManager<SessionShutdownReason>,
) {
	tokio::spawn(async move {
		start.notified().await;

		let mut buf = [0; 1024];
		let mut client_address = None;

		// Trigger session shutdown when the audio packet stream stops.
		let _stop_token = stop.trigger_shutdown_token(SessionShutdownReason::AudioPacketHandlerStopped);
		let _delay_stop = stop.delay_shutdown_token();

		while !stop.is_shutdown_triggered() {
			tokio::select! {
				packet = stop.wrap_cancel(packet_rx.recv()) => {
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

				message = stop.wrap_cancel(socket.recv_from(&mut buf)) => {
					let (len, address) = match message {
						Ok(Ok((len, address))) => (len, address),
						Ok(Err(e)) => {
							tracing::warn!("Failed to receive message: {e}");
							break;
						},
						Err(_) => break,
					};

					if &buf[..len] == b"PING" {
						tracing::trace!("Received audio stream PING message from {address}.");
						client_address = Some(address);
					} else {
						tracing::warn!("Received unknown message on audio stream of length {len}.");
					}
				},
			}
		}

		tracing::debug!("Audio packet stream stopped.");
	});
}
