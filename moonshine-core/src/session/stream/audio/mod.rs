use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use strum_macros::Display;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::session::authorization::MediaStream;
use crate::session::lifecycle::{StartLatch, StartWaiter, WorkerGuard};
use crate::session::manager::SessionShutdownReason;
use crate::session::{AuthorizationReceiver, SessionKeysReceiver};

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
/// The encoder and packet handler are registered and spawned immediately, then
/// wait on a persistent [`StartLatch`] until `trigger()`. PulseServer starts
/// immediately (it just mixes audio, no network impact).
#[derive(Clone)]
pub(crate) struct AudioStartHandle {
	start: StartLatch,
	packet_tx: mpsc::Sender<AudioPacketMessage>,
	encoder_reconfigure_tx: crossbeam_channel::Sender<AudioEncoderReconfigure>,
	pulse_reconfigure_tx: crossbeam_channel::Sender<PulseReconfigure>,
}

#[cfg(test)]
impl AudioStartHandle {
	/// A handle not connected to an encoder, for control-stream tests.
	pub(crate) fn for_test() -> Self {
		Self {
			start: StartLatch::new(),
			packet_tx: mpsc::channel(1).0,
			encoder_reconfigure_tx: crossbeam_channel::unbounded().0,
			pulse_reconfigure_tx: crossbeam_channel::unbounded().0,
		}
	}
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
	/// Idempotent: a duplicate `StartB` has no further effect.
	pub fn trigger(&self) {
		if !self.start.open() {
			tracing::debug!("Ignoring duplicate audio start signal");
		}
	}

	/// The stream's start latch, for external triggering (e.g. bench binary).
	pub(crate) fn start_latch(&self) -> StartLatch {
		self.start.clone()
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

	pub fn start(
		self,
		context: AudioStreamContext,
		keys_rx: SessionKeysReceiver,
		authorization_rx: AuthorizationReceiver,
	) -> Result<AudioStartHandle, ()> {
		// Apply QoS to UDP socket.
		if context.qos {
			let _ = self.udp_socket.set_tos_v4(224);
		}

		// Persistent gate for encoder and packet handler.
		let start = StartLatch::new();

		// Create packet channel and spawn handler — registered now, gated behind the latch.
		let (packet_tx, packet_rx) = mpsc::channel::<AudioPacketMessage>(16);
		let worker = WorkerGuard::register(&self.stop, SessionShutdownReason::AudioPacketHandlerStopped)?;
		spawn_handle_audio_packets(
			packet_rx,
			self.udp_socket,
			authorization_rx,
			start.waiter(),
			self.stop.clone(),
			worker,
		);

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

		// Spawn audio encoder — registered now, gated behind the latch.
		let (encoder_reconfigure_tx, encoder_reconfigure_rx) = crossbeam_channel::unbounded();
		AudioEncoder::spawn(
			CAPTURE_SAMPLE_RATE,
			context.clone(),
			frame_rx,
			frame_recycle_tx,
			keys_rx,
			packet_tx.clone(),
			self.stop.clone(),
			start.waiter(),
			encoder_reconfigure_rx,
		)?;

		Ok(AudioStartHandle {
			start,
			packet_tx,
			encoder_reconfigure_tx,
			pulse_reconfigure_tx,
		})
	}
}

/// `worker` was registered by the caller before spawning, so a stop before
/// `StartB` still waits for this task to drop its socket.
fn spawn_handle_audio_packets(
	packet_rx: mpsc::Receiver<AudioPacketMessage>,
	socket: UdpSocket,
	authorization: AuthorizationReceiver,
	start: StartWaiter,
	stop: ShutdownManager<SessionShutdownReason>,
	worker: WorkerGuard,
) {
	tokio::spawn(async move {
		// Declared first so it is released after the socket and channel below.
		let _worker = worker;
		let socket = socket;
		let mut packet_rx = packet_rx;
		if start.wait(&stop).await.is_err() {
			tracing::debug!("Audio packet handler stopped before start signal.");
			return;
		}

		let mut buf = [0; 1024];
		let mut client_address = None;

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

					// Only the current launch/resume generation may (re)discover the
					// destination; the source port may change (NAT, client sockets).
					if authorization.borrow().admits_media_ping(MediaStream::Audio, address, &buf[..len]) {
						tracing::trace!("Received audio stream PING message from {address}.");
						client_address = Some(address);
					} else {
						tracing::debug!(%address, len, "Ignoring unauthorized audio endpoint discovery datagram");
					}
				},
			}
		}

		tracing::debug!("Audio packet stream stopped.");
	});
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use tokio::sync::watch;

	use super::*;
	use crate::session::authorization::StreamAuthorization;

	async fn receives(socket: &UdpSocket, expected: &[u8]) -> bool {
		let mut buf = [0u8; 64];
		match tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf)).await {
			Ok(Ok((len, _))) => &buf[..len] == expected,
			_ => false,
		}
	}

	fn worker(stop: &ShutdownManager<SessionShutdownReason>) -> WorkerGuard {
		WorkerGuard::register(stop, SessionShutdownReason::AudioPacketHandlerStopped).unwrap()
	}

	/// STAB-001: audio has the same pre-start ownership contract as video.
	#[tokio::test]
	async fn stop_before_start_releases_the_audio_socket_before_completion() {
		for flavor_yield in [false, true] {
			let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
			let address = socket.local_addr().unwrap();
			let stop = ShutdownManager::new();
			let start = StartLatch::new();
			let (_tx, rx) = mpsc::channel(4);
			let authorization = StreamAuthorization::new(1, "127.0.0.1".parse().unwrap()).unwrap();
			let (_authorization_tx, authorization_rx) = watch::channel(authorization);
			spawn_handle_audio_packets(
				rx,
				socket,
				authorization_rx,
				start.waiter(),
				stop.clone(),
				worker(&stop),
			);
			if flavor_yield {
				tokio::task::yield_now().await;
			}
			stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
			tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
				.await
				.unwrap();
			UdpSocket::bind(address)
				.await
				.expect("completed shutdown must imply the audio port is free");
			start.open();
		}
	}

	/// STAB-001: video and audio share one `StartB`; both observe an opening
	/// that precedes their first poll.
	#[tokio::test(flavor = "current_thread")]
	async fn start_before_first_poll_is_not_lost() {
		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(4);
		let authorization = StreamAuthorization::new(1, "127.0.0.1".parse().unwrap()).unwrap();
		let (_authorization_tx, authorization_rx) = watch::channel(authorization.clone());
		spawn_handle_audio_packets(
			rx,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker(&stop),
		);
		start.open();
		start.open();
		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let mut ping = authorization.ping_payload(MediaStream::Audio).as_bytes().to_vec();
		ping.extend(1u32.to_be_bytes());
		let mut delivered = false;
		for _ in 0..20 {
			client.send_to(&ping, server).await.unwrap();
			tx.send(AudioPacketMessage::Packet(vec![0xdd; 32])).await.unwrap();
			if receives(&client, &[0xdd; 32]).await {
				delivered = true;
				break;
			}
		}
		assert!(delivered);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
	}

	/// Audio endpoint discovery accepts only the authorized client: a legacy
	/// PING from another host cannot redirect the stream.
	#[tokio::test]
	async fn endpoint_discovery_rejects_unauthorized_hosts() {
		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(4);
		let authorization = StreamAuthorization::new(1, "127.0.0.1".parse().unwrap()).unwrap();
		let (_authorization_tx, authorization_rx) = watch::channel(authorization.clone());
		spawn_handle_audio_packets(
			rx,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker(&stop),
		);
		start.open();

		let attacker = UdpSocket::bind("127.0.0.2:0").await.unwrap();
		attacker.send_to(b"PING", server).await.unwrap();
		tokio::time::sleep(Duration::from_millis(20)).await;
		tx.send(AudioPacketMessage::Packet(vec![0xaa; 32])).await.unwrap();
		assert!(!receives(&attacker, &[0xaa; 32]).await);

		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let mut ping = authorization.ping_payload(MediaStream::Audio).as_bytes().to_vec();
		ping.extend(1u32.to_be_bytes());
		client.send_to(&ping, server).await.unwrap();
		tokio::time::sleep(Duration::from_millis(20)).await;
		tx.send(AudioPacketMessage::Packet(vec![0xbb; 32])).await.unwrap();
		assert!(receives(&client, &[0xbb; 32]).await);

		attacker.send_to(&ping, server).await.unwrap();
		tokio::time::sleep(Duration::from_millis(20)).await;
		tx.send(AudioPacketMessage::Packet(vec![0xcc; 32])).await.unwrap();
		assert!(receives(&client, &[0xcc; 32]).await);
		assert!(!receives(&attacker, &[0xcc; 32]).await);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	}
}
