use std::ffi::CStr;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use pulse::sample_spec::MAX_CHANNELS;
use pulseaudio::protocol as pulse;
use serde::{Deserialize, Serialize};
use strum_macros::Display;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::session::SessionKeysReceiver;
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::{StartSignal, StartWaiter};

use self::encoder::AudioEncoder;
use self::pulse_server::{OUTPUT_SAMPLE_RATE, PulseServer};

mod buffer;
mod encoder;
mod log;
mod pulse_server;

/// Configuration for the audio stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioStreamConfig {
	/// Port to use for streaming audio data.
	pub port: u16,
}

impl Default for AudioStreamConfig {
	fn default() -> Self {
		Self { port: 48000 }
	}
}

/// Number of audio channels requested by the client.
#[derive(Clone, Copy, Debug, Default, Display, PartialEq, Eq, PartialOrd)]
pub enum AudioChannels {
	#[default]
	Stereo = 2,
	Surround51 = 6,
	Surround71 = 8,
}

#[derive(Debug, Error)]
pub enum AudioChannelError {
	#[error("Unsupported number of channels: {0}")]
	Unsupported(u8),
	#[error("Requested `{0}` channels, but max is {MAX_CHANNELS}")]
	OutOfBounds(u8),
}

impl TryFrom<u8> for AudioChannels {
	type Error = AudioChannelError;

	fn try_from(val: u8) -> Result<Self, Self::Error> {
		match val {
			6 => Ok(Self::Surround51),
			8 => Ok(Self::Surround71),
			2 => Ok(Self::Stereo),
			_ if val > MAX_CHANNELS => Err(AudioChannelError::OutOfBounds(val)),
			_ => Err(AudioChannelError::Unsupported(val)),
		}
	}
}

impl AudioChannels {
	pub fn map(&self) -> pulse::ChannelMap {
		match self {
			AudioChannels::Surround51 => pulse::ChannelMap::new([
				pulse::ChannelPosition::FrontLeft,
				pulse::ChannelPosition::FrontRight,
				pulse::ChannelPosition::FrontCenter,
				pulse::ChannelPosition::Lfe,
				pulse::ChannelPosition::RearLeft,
				pulse::ChannelPosition::RearRight,
			]),
			AudioChannels::Surround71 => pulse::ChannelMap::new([
				pulse::ChannelPosition::FrontLeft,
				pulse::ChannelPosition::FrontRight,
				pulse::ChannelPosition::FrontCenter,
				pulse::ChannelPosition::Lfe,
				pulse::ChannelPosition::RearLeft,
				pulse::ChannelPosition::RearRight,
				pulse::ChannelPosition::SideLeft,
				pulse::ChannelPosition::SideRight,
			]),
			AudioChannels::Stereo => pulse::ChannelMap::stereo(),
		}
	}

	pub fn map_cstr(&self) -> &CStr {
		match self {
			AudioChannels::Surround51 => c"front-left,front-right,front-center,lfe,rear-left,rear-right",
			AudioChannels::Surround71 => {
				c"front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right"
			},
			AudioChannels::Stereo => c"front-left,front-right",
		}
	}

	pub fn port_as_cstr(&self) -> &CStr {
		match self {
			AudioChannels::Surround51 => c"Surround 5.1 Output",
			AudioChannels::Surround71 => c"Surround 7.1 Output",
			AudioChannels::Stereo => c"Stereo Output",
		}
	}

	pub fn count_as_cstr(&self) -> &CStr {
		match self {
			AudioChannels::Surround51 => c"6",
			AudioChannels::Surround71 => c"8",
			AudioChannels::Stereo => c"2",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkSampleRate {
	S48k = 48000,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkSpec {
	/// Format / Encoding of individual samples.
	pub format: pulse::SampleFormat,
	/// Number of independent channels.
	pub channels: AudioChannels,
	/// Number of samples per second (and per channel).
	pub sample_rate: SinkSampleRate,
}

impl SinkSpec {
	pub fn new(channels: AudioChannels) -> Self {
		Self {
			format: pulse::SampleFormat::Float32Le,
			channels,
			sample_rate: SinkSampleRate::S48k,
		}
	}

	pub fn as_pulse_spec(&self) -> pulse::SampleSpec {
		pulse::SampleSpec {
			format: self.format,
			channels: self.channels as u8,
			sample_rate: self.sample_rate as u32,
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
pub(crate) const OPUS_STEREO: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Stereo,
	streams: 1,
	coupled_streams: 1,
	mapping: [0, 1, 0, 0, 0, 0, 0, 0],
	bitrate: 96_000,
};

pub(crate) const OPUS_HIGH_STEREO: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Stereo,
	streams: 1,
	coupled_streams: 1,
	mapping: [0, 1, 0, 0, 0, 0, 0, 0],
	bitrate: 512_000,
};

pub(crate) const OPUS_SURROUND51: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround51,
	streams: 4,
	coupled_streams: 2,
	mapping: [0, 1, 4, 5, 2, 3, 0, 0],
	bitrate: 256_000,
};

pub(crate) const OPUS_HIGH_SURROUND51: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround51,
	streams: 6,
	coupled_streams: 0,
	mapping: [0, 1, 2, 3, 4, 5, 0, 0],
	bitrate: 1_536_000,
};

pub(crate) const OPUS_SURROUND71: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround71,
	streams: 5,
	coupled_streams: 3,
	mapping: [0, 1, 4, 5, 6, 7, 2, 3],
	bitrate: 450_000,
};

pub(crate) const OPUS_HIGH_SURROUND71: OpusStreamConfig = OpusStreamConfig {
	channels: AudioChannels::Surround71,
	streams: 8,
	coupled_streams: 0,
	mapping: [0, 1, 2, 3, 4, 5, 6, 7],
	bitrate: 2_048_000,
};

/// All standard configurations, ordered for RTSP DESCRIBE emission.
pub(crate) const ALL_AUDIO_CONFIGS: [&OpusStreamConfig; 6] = [
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

pub(crate) struct AudioStream {
	pulse_socket: UnixListener,
	pub pulse_socket_path: PathBuf,
	udp_socket: tokio::net::UdpSocket,
	stop: ShutdownManager<SessionShutdownReason>,
}

impl AudioStream {
	pub async fn new(
		config: AudioStreamConfig,
		address: String,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing audio stream.");

		let udp_socket = UdpSocket::bind((address, config.port))
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

	pub fn start(self, context: AudioStreamContext, keys_rx: SessionKeysReceiver) -> Result<StartSignal, ()> {
		log::stream_start(&context);

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = self.udp_socket.set_tos_v4(224);
		}

		// Gate for encoder and packet handler.
		let start = StartSignal::new();

		// Create packet channel and spawn handler — gated behind start.
		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(10);
		spawn_handle_audio_packets(packet_rx, self.udp_socket, start.waiter(), self.stop.clone());

		// Create frame channels for PulseServer and encoder communication.
		let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
		let (frame_recycle_tx, frame_recycle_rx) = crossbeam_channel::bounded(3);

		// Spawn PulseServer immediately (no gating — it just mixes audio, no network impact).
		PulseServer::spawn(
			self.pulse_socket,
			self.pulse_socket_path.clone(),
			context.audio_config.channels,
			context.packet_duration_ms,
			frame_tx,
			frame_recycle_rx,
			self.stop.clone(),
		)
		.map_err(|e| tracing::error!("Failed to create PulseServer: {e}"))?;

		// Spawn audio encoder — gated behind start.
		AudioEncoder::spawn(
			OUTPUT_SAMPLE_RATE,
			&context.audio_config.stream_config,
			frame_rx,
			frame_recycle_tx,
			keys_rx,
			context.encrypt_audio,
			packet_tx,
			self.stop.clone(),
			start.waiter(),
		)?;

		Ok(start)
	}
}

fn spawn_handle_audio_packets(
	mut packet_rx: mpsc::Receiver<Vec<u8>>,
	socket: UdpSocket,
	mut start: StartWaiter,
	stop: ShutdownManager<SessionShutdownReason>,
) {
	tokio::spawn(async move {
		if !start.wait(&stop).await {
			return;
		}

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
							if let Some(client_address) = client_address
								&& let Err(e) = socket.send_to(packet.as_slice(), client_address).await {
									tracing::warn!("Failed to send packet to client: {e}");
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

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn shutdown_before_start_releases_packet_handler() {
		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let (packet_tx, packet_rx) = mpsc::channel(1);
		let start = StartSignal::new();
		let stop = ShutdownManager::new();
		spawn_handle_audio_packets(packet_rx, socket, start.waiter(), stop.clone());

		stop.trigger_shutdown(SessionShutdownReason::ManagerShutdown).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), packet_tx.closed())
			.await
			.expect("packet handler kept its socket after shutdown");
	}
}
