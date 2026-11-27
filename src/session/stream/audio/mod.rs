use std::ffi::CStr;
use std::fmt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_shutdown::ShutdownManager;
use pulseaudio::protocol as pulse;
use serde::{Deserialize, Serialize};
use strum_macros::Display;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use crate::session::SessionKeysReceiver;
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::{StartSignal, StartWaiter};

use self::encoder::AudioEncoder;
use self::pulse_server::PulseServer;

/// Where the audio stream sends its packets.
#[derive(Clone, Debug, Default)]
pub(crate) enum AudioSink {
	/// The GameStream client, at the address its PINGs come from: RTP packets with FEC.
	#[default]
	GameStream,
	/// A browser: the Opus packets alone. When this channel is full, packets are dropped; the
	/// encoder never waits for a browser.
	Browser(mpsc::Sender<Vec<u8>>),
}

/// Changes where a running audio stream sends its packets.
#[derive(Clone)]
pub(crate) struct AudioHandle {
	sink: Arc<watch::Sender<AudioSink>>,
	/// Set while the sink is a browser: the encoder then does not encrypt.
	plain: Arc<AtomicBool>,
}

impl AudioHandle {
	pub fn set_sink(&self, sink: AudioSink) {
		self.plain
			.store(matches!(sink, AudioSink::Browser(_)), Ordering::Relaxed);
		self.sink.send_replace(sink);
	}
}

/// Size of the RTP header in front of each audio packet.
const RTP_HEADER_SIZE: usize = 12;

/// The RTP payload type of Opus data packets; FEC packets have another type.
const OPUS_PAYLOAD_TYPE: u8 = 97;

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

/// The client asked for a channel count the sink does not support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChannelError(pub u8);

impl fmt::Display for AudioChannelError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Unsupported number of channels: {}", self.0)
	}
}

impl std::error::Error for AudioChannelError {}

impl TryFrom<u8> for AudioChannels {
	type Error = AudioChannelError;

	fn try_from(val: u8) -> Result<Self, Self::Error> {
		match val {
			2 => Ok(Self::Stereo),
			6 => Ok(Self::Surround51),
			8 => Ok(Self::Surround71),
			_ => Err(AudioChannelError(val)),
		}
	}
}

impl AudioChannels {
	/// Parse the channel count a client asked for, warning and falling back to stereo when the
	/// sink does not support it.
	pub fn from_wire_or_stereo(count: u8) -> Self {
		Self::try_from(count).unwrap_or_else(|err| {
			tracing::warn!("{err}, falling back to stereo");
			Self::Stereo
		})
	}

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

/// The rate the sink runs at, and the one the encoder expects. Streams at any other rate are
/// resampled to it.
pub(crate) const SINK_SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkSpec {
	/// Format / Encoding of individual samples.
	pub format: pulse::SampleFormat,
	/// Number of independent channels.
	pub channels: AudioChannels,
	/// Number of samples per second (and per channel).
	pub sample_rate: u32,
}

impl SinkSpec {
	pub fn new(channels: AudioChannels) -> Self {
		Self {
			format: pulse::SampleFormat::Float32Le,
			channels,
			sample_rate: SINK_SAMPLE_RATE,
		}
	}

	pub fn as_pulse_spec(&self) -> pulse::SampleSpec {
		pulse::SampleSpec {
			format: self.format,
			channels: self.channels as u8,
			sample_rate: self.sample_rate,
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
	/// `session_name` names the PulseAudio socket directory
	/// (`$XDG_RUNTIME_DIR/<session_name>/pulse`), so two session managers in
	/// one user session (Moonshine and `moonshine-bench`) do not share it.
	pub async fn new(
		config: AudioStreamConfig,
		address: String,
		session_name: &str,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing audio stream.");

		let udp_socket = UdpSocket::bind((address, config.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		// Create the socket directory for the PulseAudio server.
		let pulse_socket_dir = dirs::runtime_dir()
			.ok_or_else(|| tracing::error!("Failed to get runtime directory for PulseAudio socket"))?
			.join(session_name)
			.join("pulse");
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
	) -> Result<(StartSignal, AudioHandle), ()> {
		log::stream_start(&context);

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = self.udp_socket.set_tos_v4(224);
		}

		// Gate for encoder and packet handler.
		let start = StartSignal::new();

		// Create packet channel and spawn handler — gated behind start.
		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(10);
		let (sink_tx, sink_rx) = watch::channel(AudioSink::GameStream);
		let handle = AudioHandle {
			sink: Arc::new(sink_tx),
			plain: Arc::new(AtomicBool::new(false)),
		};
		spawn_handle_audio_packets(packet_rx, self.udp_socket, start.waiter(), self.stop.clone(), sink_rx);

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
			SINK_SAMPLE_RATE,
			context.packet_duration_ms,
			&context.audio_config.stream_config,
			frame_rx,
			frame_recycle_tx,
			keys_rx,
			context.encrypt_audio,
			handle.plain.clone(),
			packet_tx,
			self.stop.clone(),
			start.waiter(),
		)?;

		Ok((start, handle))
	}
}

fn spawn_handle_audio_packets(
	mut packet_rx: mpsc::Receiver<Vec<u8>>,
	socket: UdpSocket,
	mut start: StartWaiter,
	stop: ShutdownManager<SessionShutdownReason>,
	sink: watch::Receiver<AudioSink>,
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
							let browser = match &*sink.borrow() {
								AudioSink::Browser(browser) => Some(browser.clone()),
								AudioSink::GameStream => None,
							};
							if let Some(browser) = browser {
								// The browser gets the Opus data: no RTP header, no FEC packets.
								if packet.len() > RTP_HEADER_SIZE && packet[1] & 0x7f == OPUS_PAYLOAD_TYPE {
									let _ = browser.try_send(packet[RTP_HEADER_SIZE..].to_vec());
								}
							} else if let Some(client_address) = client_address
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

	/// Writes surround test packets for a browser channel-order check: for each surround
	/// configuration and each input channel (in the sink's order), Opus packets with a tone in that
	/// channel only. `SURROUND_VECTORS=<file> cargo test --lib surround_vectors`.
	#[test]
	fn surround_vectors() {
		let Ok(path) = std::env::var("SURROUND_VECTORS") else {
			return;
		};
		let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
		let mut out = Vec::new();
		for config in [
			OPUS_SURROUND51,
			OPUS_HIGH_SURROUND51,
			OPUS_SURROUND71,
			OPUS_HIGH_SURROUND71,
		] {
			let channels = config.channels as usize;
			for loud in 0..channels {
				let mut encoder = opus::MSEncoder::new(
					48_000,
					config.streams,
					config.coupled_streams,
					&config.mapping[..channels],
					opus::Application::LowDelay,
				)
				.unwrap();
				encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate as i32)).unwrap();
				let packets: Vec<String> = (0..30)
					.map(|packet| {
						let mut pcm = vec![0f32; 480 * channels];
						for i in 0..480 {
							let t = (packet * 480 + i) as f32 / 48_000.0;
							pcm[i * channels + loud] = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
						}
						hex(&encoder.encode_vec_float(&pcm, 4000).unwrap())
					})
					.collect();
				out.push(serde_json::json!({
					"channels": channels,
					"streams": config.streams,
					"coupled": config.coupled_streams,
					"mapping": &config.mapping[..channels],
					"loud": loud,
					"packets": packets,
				}));
			}
		}
		std::fs::write(path, serde_json::to_vec(&out).unwrap()).unwrap();
	}

	#[tokio::test]
	async fn shutdown_before_start_releases_packet_handler() {
		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let (packet_tx, packet_rx) = mpsc::channel(1);
		let start = StartSignal::new();
		let stop = ShutdownManager::new();
		spawn_handle_audio_packets(
			packet_rx,
			socket,
			start.waiter(),
			stop.clone(),
			watch::channel(AudioSink::GameStream).1,
		);

		stop.trigger_shutdown(SessionShutdownReason::ManagerShutdown).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), packet_tx.closed())
			.await
			.expect("packet handler kept its socket after shutdown");
	}
}
