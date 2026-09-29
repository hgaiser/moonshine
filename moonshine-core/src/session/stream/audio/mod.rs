use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use strum_macros::Display;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::session::SessionKeysReceiver;
use crate::session::manager::SessionShutdownReason;

use self::encoder::AudioEncoder;
use self::pulse_server::{CAPTURE_SAMPLE_RATE, PulseServer};

mod buffer;
mod encoder;
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
#[derive(Clone)]
pub(crate) struct AudioStartHandle {
	notify: Arc<Notify>,
	packet_tx: mpsc::Sender<AudioPacketMessage>,
	encoder_reconfigure_tx: crossbeam_channel::Sender<AudioEncoderReconfigure>,
	pulse_reconfigure_tx: crossbeam_channel::Sender<PulseReconfigure>,
}

pub(crate) struct AudioEncoderReconfigure {
	context: AudioStreamContext,
	applied: tokio::sync::oneshot::Sender<Result<(), ()>>,
}

pub(crate) struct PulseReconfigure {
	channels: u8,
	packet_duration_ms: u32,
	applied: tokio::sync::oneshot::Sender<Result<(), String>>,
}

pub(crate) enum AudioPacketMessage {
	Packet(Vec<u8>),
	Pause(tokio::sync::oneshot::Sender<()>),
	BeginEpoch {
		qos: bool,
		ready: tokio::sync::oneshot::Sender<()>,
	},
}

impl AudioStartHandle {
	/// Signal the encoder and packet handler to begin processing.
	pub fn trigger(&self) {
		// Call notify_one() twice instead of notify_waiters() because
		// Notify only wakes tasks already .awaiting; notify_waiters()
		// is a no-op if no task is waiting yet.  notify_one() stores
		// a permit so the next notified().await completes immediately.
		self.notify.notify_one();
		self.notify.notify_one();
	}

	/// Clone the start notify for external triggering (e.g. bench binary).
	pub fn clone_start_notify(&self) -> Arc<Notify> {
		self.notify.clone()
	}

	pub async fn pause_for_reconfigure(&self) -> Result<(), ()> {
		let (ready, waiting) = tokio::sync::oneshot::channel();
		self.packet_tx
			.send(AudioPacketMessage::Pause(ready))
			.await
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())
	}

	pub async fn reconfigure(&self, context: AudioStreamContext, reconfigure_capture: bool) -> Result<(), ()> {
		if reconfigure_capture {
			let (pulse_applied, pulse_waiting) = tokio::sync::oneshot::channel();
			self.pulse_reconfigure_tx
				.send(PulseReconfigure {
					channels: context.audio_config.channels as u8,
					packet_duration_ms: context.packet_duration_ms,
					applied: pulse_applied,
				})
				.map_err(|_| ())?;
			pulse_waiting
				.await
				.map_err(|_| ())?
				.map_err(|error| tracing::warn!(%error, "PulseAudio capture reconfiguration failed"))?;
		}

		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.encoder_reconfigure_tx
			.send(AudioEncoderReconfigure { context, applied })
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())?
	}
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

	pub fn start(self, context: AudioStreamContext, keys_rx: SessionKeysReceiver) -> Result<AudioStartHandle, ()> {
		// Apply QoS to UDP socket.
		if context.qos {
			let _ = self.udp_socket.set_tos_v4(224);
		}

		// Create the notify gate for encoder and packet handler.
		let start_notify = Arc::new(Notify::new());

		// Create packet channel and spawn handler — gated behind start_notify.
		let (packet_tx, packet_rx) = mpsc::channel::<AudioPacketMessage>(16);
		spawn_handle_audio_packets(packet_rx, self.udp_socket, start_notify.clone(), self.stop.clone());

		// Create frame channels for PulseServer and encoder communication.
		let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
		let (frame_recycle_tx, frame_recycle_rx) = crossbeam_channel::bounded(3);

		// Spawn PulseServer immediately (no gating — it just mixes audio, no network impact).
		let (pulse_reconfigure_tx, pulse_reconfigure_rx) = crossbeam_channel::unbounded();
		PulseServer::spawn(
			self.pulse_socket,
			self.pulse_socket_path.clone(),
			context.audio_config.channels as u8,
			context.packet_duration_ms,
			frame_tx,
			frame_recycle_rx,
			self.stop.clone(),
			pulse_reconfigure_rx,
		)
		.map_err(|e| tracing::error!("Failed to create PulseServer: {e}"))?;

		// Spawn audio encoder — gated behind start_notify.
		let (encoder_reconfigure_tx, encoder_reconfigure_rx) = crossbeam_channel::unbounded();
		AudioEncoder::spawn(
			CAPTURE_SAMPLE_RATE,
			context.clone(),
			frame_rx,
			frame_recycle_tx,
			keys_rx,
			packet_tx.clone(),
			self.stop.clone(),
			start_notify.clone(),
			encoder_reconfigure_rx,
		)?;

		Ok(AudioStartHandle {
			notify: start_notify,
			packet_tx,
			encoder_reconfigure_tx,
			pulse_reconfigure_tx,
		})
	}
}

fn spawn_handle_audio_packets(
	mut packet_rx: mpsc::Receiver<AudioPacketMessage>,
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
				message = stop.wrap_cancel(packet_rx.recv()) => {
					match message {
						Ok(Some(AudioPacketMessage::Pause(ready))) => {
							client_address = None;
							let _ = ready.send(());
						},
						Ok(Some(AudioPacketMessage::BeginEpoch { qos, ready })) => {
							let _ = socket.set_tos_v4(if qos { 224 } else { 0 });
							let _ = ready.send(());
						},
						Ok(Some(AudioPacketMessage::Packet(packet))) => {
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
