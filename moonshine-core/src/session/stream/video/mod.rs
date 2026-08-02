use std::sync::Arc;

use async_shutdown::ShutdownManager;
use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use serde::{Deserialize, Serialize};
use tokio::{
	net::UdpSocket,
	sync::{Notify, broadcast, mpsc, watch},
};

use crate::session::SessionKeysReceiver;
use crate::session::compositor::frame::{ExportedFrame, HdrModeState};
use crate::session::manager::SessionShutdownReason;

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
	/// Time spent submitting the frame to the asynchronous encoder.
	pub submit: std::time::Duration,
	/// Time between submit completion and the packet consumer awaiting the encode future.
	pub consumer_queue: std::time::Duration,
	/// Time from submit completion until the asynchronous encode/readback future has resolved.
	pub encode_wait: std::time::Duration,
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
	/// Reference frame invalidation requests, carrying the inclusive
	/// `[first, last]` client frame-index range the client could not decode.
	invalidate_tx: broadcast::Sender<(u32, u32)>,
	reset_tx: broadcast::Sender<()>,
}

impl VideoStreamHandle {
	/// Signal the video pipeline and packet handler to begin processing.
	pub fn trigger(&self) {
		// Call notify_one() twice instead of notify_waiters() because
		// Notify only wakes tasks already .awaiting; notify_waiters()
		// is a no-op if no task is waiting yet.  notify_one() stores
		// a permit so the next notified().await completes immediately.
		self.notify.notify_one();
		self.notify.notify_one();
	}

	/// Request an IDR (key) frame from the encoder.
	pub fn request_idr_frame(&self) {
		let _ = self.idr_tx.send(());
	}

	/// Request reference frame invalidation for the inclusive client frame-index
	/// range `[first, last]` the client reported it could not decode.
	///
	/// The encoder drops the affected references and recovers by predicting from
	/// a surviving reference where possible, falling back to an IDR only when no
	/// reference survives — much cheaper than always re-sending a keyframe.
	pub fn invalidate_reference_frames(&self, first: u32, last: u32) {
		let _ = self.invalidate_tx.send((first, last));
	}

	/// Reset the stream's frame/sequence counters for a resuming client.
	///
	/// Called when a client reconnects to an already-running session. The pipeline
	/// keeps incrementing `frame_number` for the lifetime of the session, but a fresh
	/// Moonlight session expects frame numbers to start at 1; without a reset it counts
	/// the jump as massive frame loss and reports a poor connection. This also forces an
	/// IDR so the resumed client has a decodable starting frame.
	pub fn request_reset(&self) {
		let _ = self.reset_tx.send(());
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

		// Initialize quinn-udp state for GSO support.
		let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
			.map_err(|e| tracing::error!("Failed to initialize UDP socket state: {e}"))?;
		let gso_enabled = udp_state.max_gso_segments() > 1;
		if gso_enabled {
			tracing::info!("GSO enabled, max segments: {}", udp_state.max_gso_segments());
		} else {
			tracing::info!("GSO not available, using per-shard sends");
		}

		// Gate for pipeline + packet handler.
		let start_notify = Arc::new(Notify::new());

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Reference frame invalidation broadcast channel. Sized for a small burst
		// of loss reports; the encode loop drains all pending each iteration.
		let (invalidate_tx, _invalidate_rx) = broadcast::channel(16);

		// Stream-reset broadcast channel (client reconnect/resume).
		let (reset_tx, _reset_rx) = broadcast::channel(1);

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<ShardBatch>(128);

		// Spawn packet handler — gated behind start_notify.
		spawn_handle_video_packets(packet_rx, socket, udp_state, start_notify.clone(), stop.clone());

		// Spawn pipeline thread — gated behind start_notify.
		VideoPipeline::new(
			frame_rx,
			config,
			context,
			keys_rx,
			packet_tx,
			idr_tx.subscribe(),
			invalidate_tx.subscribe(),
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
			invalidate_tx,
			reset_tx,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<ShardBatch>,
	socket: UdpSocket,
	udp_state: UdpSocketState,
	start: Arc<Notify>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
) {
	tokio::spawn(async move {
		start.notified().await;

		let mut buf = [0; 1024];
		let mut client_address = None;
		// Rate-limits the GSO-fallback warning.
		let mut last_send_warn: Option<std::time::Instant> = None;

		// Trigger session shutdown if we exit unexpectedly.
		let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoPacketHandlerStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		while !stop_session_manager.is_shutdown_triggered() {
			tokio::select! {
				batch = stop_session_manager.wrap_cancel(packet_rx.recv()) => {
					match batch {
						Ok(Some(batch)) => {
							if let Some(addr) = client_address {
								if batch.shard_count() == 0 {
									continue;
								}

								// Try GSO first if available (check dynamically —
								// quinn-udp may disable GSO after a send failure).
								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown.
								let use_gso = udp_state.max_gso_segments() > 1;
								if use_gso {
									match stop_session_manager
										.wrap_cancel(send_batch_gso(&socket, &udp_state, &batch, addr))
										.await
									{
										Ok(failed_chunks) => {
											if failed_chunks > 0
												&& last_send_warn
													.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
											{
												tracing::warn!(
													"GSO send failed for {failed_chunks} chunk(s), sent per-shard instead"
												);
												last_send_warn = Some(std::time::Instant::now());
											}
										},
										Err(_) => break,
									}
								} else if stop_session_manager
									.wrap_cancel(send_shards(&socket, &batch, addr))
									.await
									.is_err()
								{
									break;
								}
							}
						},
						Ok(None) => {
							tracing::debug!("Video packet channel closed.");
							break;
						},
						Err(_) => break,
					}
				},

				message = stop_session_manager.wrap_cancel(socket.recv_from(&mut buf)) => {
					let (len, address) = match message {
						Ok(Ok((len, address))) => (len, address),
						Ok(Err(e)) => {
							tracing::warn!("Failed to receive message: {e}");
							break;
						},
						Err(_) => break,
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

		tracing::debug!("Video packet stream stopped.");
	});
}

/// Maximum payload of one UDP datagram (65535 minus IPv4/UDP headers).
/// A GSO batch is still one datagram: the kernel rejects anything larger
/// with EMSGSIZE before segmenting. IPv6 allows 20 more bytes; the IPv4
/// value is safe for both.
const MAX_UDP_PAYLOAD: usize = 65507;

/// Number of shards per GSO send: the kernel's segment-count cap or the
/// datagram-size cap, whichever binds first. Never zero, even for a shard
/// larger than a datagram (such a chunk fails at send time and falls back
/// to per-shard sends, which fail visibly instead of silently).
fn gso_segments_per_send(max_gso_segments: usize, shard_size: usize) -> usize {
	max_gso_segments
		.min(MAX_UDP_PAYLOAD.checked_div(shard_size).unwrap_or(0))
		.max(1)
}

/// Send a batch as GSO chunks sized to the kernel's segment and datagram
/// caps; a frame's batch routinely exceeds both. Uses `try_send` because
/// `send` masks every error except `WouldBlock` as success, silently
/// discarding the batch. Returns the number of chunks that fell back to
/// per-shard sends, so the caller can rate-limit the warning.
async fn send_batch_gso(
	socket: &UdpSocket,
	udp_state: &UdpSocketState,
	batch: &ShardBatch,
	addr: std::net::SocketAddr,
) -> u32 {
	let shard_size = batch.shard_size();
	if shard_size == 0 {
		return 0;
	}
	let segments_per_send = gso_segments_per_send(udp_state.max_gso_segments(), shard_size);

	let mut failed_chunks = 0u32;
	for chunk in batch.as_bytes().chunks(segments_per_send * shard_size) {
		let transmit = Transmit {
			destination: addr,
			ecn: None,
			contents: chunk,
			segment_size: Some(shard_size),
			src_ip: None,
		};
		let result = loop {
			match udp_state.try_send(UdpSockRef::from(socket), &transmit) {
				// A frame burst can outrun the socket buffer; wait for it to
				// drain and resend the same chunk instead of degrading.
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					socket.writable().await.ok();
				},
				other => break other,
			}
		};
		if let Err(e) = result {
			failed_chunks += 1;
			tracing::debug!("GSO send failed ({e}), falling back to per-shard sends for this chunk");
			send_shard_bytes(socket, chunk, shard_size, addr).await;
		}
	}
	failed_chunks
}

/// Send all shards in a batch as individual UDP packets.
async fn send_shards(socket: &UdpSocket, batch: &ShardBatch, addr: std::net::SocketAddr) {
	send_shard_bytes(socket, batch.as_bytes(), batch.shard_size(), addr).await;
}

/// Send a contiguous buffer of equal-sized shards as individual UDP packets.
async fn send_shard_bytes(socket: &UdpSocket, bytes: &[u8], shard_size: usize, addr: std::net::SocketAddr) {
	if shard_size == 0 {
		return;
	}
	for shard in bytes.chunks(shard_size) {
		if let Err(e) = socket.send_to(shard, addr).await {
			tracing::warn!("Failed to send packet to client: {e}");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gso_segment_count_cap_binds() {
		// Payload cap (65507 / 100 = 655) is looser than the segment cap.
		assert_eq!(gso_segments_per_send(8, 100), 8);
	}

	#[test]
	fn gso_payload_cap_binds() {
		// 65507 / 2000 = 32, tighter than the kernel's 64-segment cap.
		assert_eq!(gso_segments_per_send(64, 2000), 32);
	}

	#[test]
	fn gso_boundary_shard_sizes() {
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD), 1);
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD - 1), 1);
		// A shard that cannot fit a datagram at all still yields 1, not 0.
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD + 1), 1);
		assert_eq!(gso_segments_per_send(64, 0), 1);
	}

	#[test]
	fn gso_chunks_respect_kernel_limits() {
		// Shard counts chosen to force a partial final chunk.
		for (shard_count, shard_size) in [(250usize, 1404usize), (47, 1404), (100, 733)] {
			let segments = gso_segments_per_send(64, shard_size);
			let data = vec![0u8; shard_count * shard_size];
			for chunk in data.chunks(segments * shard_size) {
				assert_eq!(chunk.len() % shard_size, 0, "chunk must hold whole shards");
				assert!(chunk.len() <= MAX_UDP_PAYLOAD, "chunk must fit one datagram");
				assert!(chunk.len() / shard_size <= 64, "chunk must respect segment cap");
			}
		}
	}
}
