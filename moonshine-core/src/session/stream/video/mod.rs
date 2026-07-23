use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::{
	net::UdpSocket,
	sync::{broadcast, mpsc, watch, Notify},
};

use crate::session::compositor::frame::{ExportedFrame, HdrModeState};
use crate::session::manager::SessionShutdownReason;
use crate::session::SessionKeysReceiver;

mod packetizer;
mod pipeline;
mod shard_batch;
use pipeline::VideoPipeline;
use shard_batch::ShardBatch;

/// Configuration for the video stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoStreamConfig {
	/// Port to use for streaming video data.
	pub port: u16,

	/// What percentage of data packets should be parity packets.
	pub fec_percentage: u8,

	/// Whether to enable video stream encryption (AES-128-GCM).
	#[serde(default)]
	pub encrypt: bool,

	/// Whether to emit a WARN log when a single frame takes longer to encode and
	/// packetize than the frame budget.
	#[serde(default)]
	pub log_frame_spikes: bool,
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			fec_percentage: 20,
			encrypt: false,
			log_frame_spikes: false,
		}
	}
}

/// Per-frame encoding statistics emitted by the video pipeline.
///
/// Sent via `broadcast` channel, receivable through `SessionManager::bench_stats_receiver()`.
#[derive(Clone, Debug)]
pub struct FrameStats {
	/// Time the frame spent waiting in the compositor's output channel.
	pub channel_wait: std::time::Duration,
	/// Time spent importing the DMA-BUF into Vulkan.
	pub import: std::time::Duration,
	/// Time spent on GPU color conversion.
	pub convert: std::time::Duration,
	/// Time spent encoding the frame.
	pub encode: std::time::Duration,
	/// Time spent packetizing the encoded data.
	pub packetize: std::time::Duration,
	/// Time spent sending the packets over the channel.
	pub send: std::time::Duration,
	/// Total end-to-end latency for this frame.
	pub total: std::time::Duration,
	/// Number of bytes encoded for this frame.
	pub encoded_bytes: usize,
	/// Whether this frame is a key (IDR) frame.
	pub is_key_frame: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoFormat {
	#[default]
	H264,
	Hevc,
	Av1,
}

impl TryFrom<u32> for VideoFormat {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::H264),
			1 => Ok(Self::Hevc),
			2 => Ok(Self::Av1),
			_ => Err(()),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoDynamicRange {
	#[default]
	Sdr,
	Hdr,
}

impl TryFrom<u32> for VideoDynamicRange {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::Sdr),
			1 => Ok(Self::Hdr),
			_ => Err(()),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoChromaSampling {
	#[default]
	Yuv420,
	Yuv444,
}

impl TryFrom<u32> for VideoChromaSampling {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::Yuv420),
			1 => Ok(Self::Yuv444),
			_ => Err(()),
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct VideoStreamContext {
	/// Width of the video stream in pixels.
	pub width: u32,

	/// Height of the video stream in pixels.
	pub height: u32,

	/// Frames per second of the video stream.
	pub fps: u32,

	/// Size of each encoded packet in bytes.
	pub packet_size: usize,

	/// Target bitrate for the video stream in bits per second.
	pub bitrate: usize,

	/// Minimum number of FEC packets to include for each frame.
	pub minimum_fec_packets: u32,

	/// Whether to apply QoS markings to video stream packets.
	pub qos: bool,

	/// Video format to use for encoding the stream.
	pub video_format: VideoFormat,

	/// Dynamic range of the video stream.
	pub dynamic_range: VideoDynamicRange,

	/// Chroma sampling type for the video stream.
	pub chroma_sampling_type: VideoChromaSampling,

	/// Maximum number of reference frames for the video encoder.
	pub max_reference_frames: u32,

	/// Whether the client has enabled video encryption.
	pub encrypt_video: bool,
}

/// Handle returned by `VideoStream::start` that gates the pipeline and packet handler.
///
/// The pipeline and packet handler are spawned immediately but block on a `Notify`
/// until `trigger()` is called on `StartB`.
#[derive(Clone)]
pub(crate) struct VideoStreamHandle {
	notify: Arc<Notify>,
	idr_tx: broadcast::Sender<()>,
	/// Set on resume to arm a stream reset; the packet handler fires it once it has
	/// re-learned the reconnecting client's address (see `request_reset`).
	resume_pending: Arc<AtomicBool>,
}

impl VideoStreamHandle {
	/// Signal the video pipeline and packet handler to begin processing.
	pub fn trigger(&self) {
		self.notify.notify_waiters();
	}

	/// Request an IDR (key) frame from the encoder.
	pub fn request_idr_frame(&self) {
		let _ = self.idr_tx.send(());
	}

	/// Arm a stream reset for a resuming client.
	///
	/// Called when a client reconnects to an already-running session. The pipeline
	/// keeps incrementing `frame_number` for the lifetime of the session, but a fresh
	/// Moonlight session expects frame numbers to start at 1; without a reset it counts
	/// the jump as massive frame loss and reports a poor connection. The reset also forces
	/// an IDR so the resumed client has a decodable starting frame.
	///
	/// The reset is not fired immediately: the packet handler still holds the previous
	/// connection's address, and a reconnecting client almost always arrives on a new UDP
	/// source port. Firing now would spend the forced IDR on the stale address, the client
	/// would receive no decodable frame, and it would abort with a connection error
	/// (typically recovering only on a retry). Instead we arm a flag that the packet handler
	/// consumes once it has re-learned the client's address from its first PING, so the IDR
	/// lands where the client is actually listening.
	pub fn request_reset(&self) {
		self.resume_pending.store(true, Ordering::Relaxed);
	}

	/// Clone the start notify for external triggering (e.g. bench binary).
	pub fn clone_start_notify(&self) -> Arc<Notify> {
		self.notify.clone()
	}
}

pub(crate) struct VideoStream {
	socket: UdpSocket,
	frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
	hdr_metadata_tx: watch::Sender<HdrModeState>,
	stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
}

impl VideoStream {
	pub async fn new(
		config: VideoStreamConfig,
		address: String,
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
		_stop: ShutdownManager<SessionShutdownReason>,
		stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video stream.");

		let socket = UdpSocket::bind((address.as_str(), config.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		tracing::debug!(
			"Listening for video messages on {}",
			socket
				.local_addr()
				.map_err(|e| tracing::warn!("Failed to get local address associated with video socket: {e}"))?
		);

		Ok(Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
			stats_tx,
		})
	}

	#[allow(clippy::too_many_arguments)]
	pub fn start(
		self,
		config: VideoStreamConfig,
		context: VideoStreamContext,
		keys_rx: SessionKeysReceiver,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<VideoStreamHandle, ()> {
		let Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
			stats_tx,
		} = self;

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = socket.set_tos_v4(160);
		}

		// Gate for pipeline + packet handler.
		let start_notify = Arc::new(Notify::new());

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Stream-reset broadcast channel (client reconnect/resume). The packet handler
		// fires it once it has re-learned the reconnecting client's address.
		let (reset_tx, _reset_rx) = broadcast::channel(1);
		let resume_pending = Arc::new(AtomicBool::new(false));

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<ShardBatch>(128);

		// Spawn packet handler — gated behind start_notify.
		spawn_handle_video_packets(
			packet_rx,
			socket,
			start_notify.clone(),
			reset_tx.clone(),
			resume_pending.clone(),
			stop.clone(),
		);

		// Spawn pipeline thread — gated behind start_notify.
		VideoPipeline::new(
			frame_rx,
			config,
			context,
			keys_rx,
			packet_tx,
			idr_tx.subscribe(),
			reset_tx.subscribe(),
			stop.clone(),
			hdr_metadata_tx,
			start_notify.clone(),
			stats_tx,
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			notify: start_notify,
			idr_tx,
			resume_pending,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<ShardBatch>,
	socket: UdpSocket,
	start: Arc<Notify>,
	reset_tx: broadcast::Sender<()>,
	resume_pending: Arc<AtomicBool>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
) {
	tokio::spawn(async move {
		start.notified().await;

		let mut buf = [0; 1024];
		let mut client_address = None;

		// Trigger session shutdown if we exit unexpectedly.
		let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoPacketHandlerStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		while !stop_session_manager.is_shutdown_triggered() {
			tokio::select! {
				batch = packet_rx.recv() => {
					match batch {
						Some(batch) => {
							if let Some(client_address) = client_address {
								for shard in batch.shards() {
									if let Err(e) = socket.send_to(shard, client_address).await {
										tracing::warn!("Failed to send packet to client: {e}");
									}
								}
							}
						},
						None => {
							tracing::debug!("Video packet channel closed.");
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

						// A resume armed a stream reset (frame-counter reset + forced IDR). Fire it
						// now that we know where the reconnecting client is listening, so the forced
						// IDR is sent to the current address instead of the previous connection's.
						if resume_pending.swap(false, Ordering::Relaxed) {
							tracing::info!("Re-learned client address after resume; firing armed stream reset.");
							let _ = reset_tx.send(());
						}
					} else {
						tracing::warn!("Received unknown message on video stream of length {len}.");
					}
				},
			}
		}

		tracing::debug!("Video packet stream stopped.");
	});
}
