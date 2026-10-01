use std::sync::Arc;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::session::SessionKeysReceiver;
use crate::session::compositor::frame::{ExportedFrame, HdrModeState};
use crate::session::manager::SessionShutdownReason;

mod diagnostics;
pub(crate) mod fec;
mod format;
mod gso_socket;
mod packetizer;
mod pipeline;
pub(crate) mod pyrowave;
mod shard_batch;
pub use fec::FecMode;
use fec::FrameFecStatus;
pub use format::{
	BitDepth, ChromaFormat, ColorPrimaries, ColorRange, MatrixCoefficients, NegotiatedVideoFormat, TransferFunction,
	VideoChromaSampling, VideoCodec, VideoDynamicRange, VideoFormat,
};
use gso_socket::{UdpGsoSocket, duration_micros_u64};
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

	/// Whether FEC is disabled, fixed, or driven by client feedback.
	pub fec_mode: FecMode,

	/// Lower bound for automatic FEC.
	pub fec_min_percentage: u8,

	/// Upper bound for automatic FEC.
	pub fec_max_percentage: u8,

	/// Whether to enable video stream encryption (AES-128-GCM).
	#[serde(default)]
	pub encrypt: bool,

	/// Whether to emit a WARN log when a single frame takes longer to encode and
	/// packetize than the frame budget.
	#[serde(default)]
	pub log_frame_spikes: bool,

	/// Upper bound for the client-requested video packet size, in bytes.
	///
	/// Moonlight's default packet size (1392 bytes) can exceed the path MTU
	/// over VPNs and tunnels, causing fragmented, dropped video. When non-zero,
	/// the client's `x-nv-video[0].packetSize` is clamped to this value; a
	/// smaller client request is honored. `0` disables the cap.
	///
	/// This is the stream's packet size, not a raw interface MTU: the on-wire
	/// UDP payload is `max_packet_size + 16` bytes. For example, to fit a
	/// 1420-byte WireGuard MTU over IPv4, use 1376 (1420 minus the 20-byte IP
	/// and 8-byte UDP headers and the 16-byte stream overhead).
	#[serde(default)]
	pub max_packet_size: usize,
}

/// Smallest accepted packet size cap. Lower values would leave almost no room
/// for payload after the 16-byte NV video header, so they are ignored.
const MIN_PACKET_SIZE: usize = 200;

impl VideoStreamConfig {
	/// Clamp a client-requested video packet size to `max_packet_size`.
	///
	/// A client only asks for what it believes it can receive, so a request
	/// smaller than the cap is honored; the cap only lowers oversized requests.
	/// A cap of `0` disables the limit.
	pub(crate) fn clamp_packet_size(&self, requested: usize) -> usize {
		if self.max_packet_size == 0 {
			return requested;
		}
		if self.max_packet_size < MIN_PACKET_SIZE {
			tracing::warn!(
				"max_packet_size {} is below the minimum of {MIN_PACKET_SIZE}, ignoring the cap.",
				self.max_packet_size
			);
			return requested;
		}
		requested.min(self.max_packet_size)
	}
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			fec_percentage: 20,
			// Fixed preserves the behavior of configurations written before the
			// explicit policy fields existed. Users opt into feedback with `auto`.
			fec_mode: FecMode::Fixed,
			fec_min_percentage: 0,
			fec_max_percentage: 25,
			encrypt: false,
			log_frame_spikes: false,
			max_packet_size: 0,
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
	/// Time from packetization through packet-channel handoff. PyroWave waits
	/// through the final UDP socket submission; conventional codecs currently
	/// stop this measurement when the packet channel accepts the batch.
	pub send: std::time::Duration,
	/// Total end-to-end latency for this frame.
	pub total: std::time::Duration,
	/// Number of bytes encoded for this frame.
	pub encoded_bytes: usize,
	/// Approximate transmitted bytes including FEC, packet headers, and encryption prefix.
	pub wire_bytes: usize,
	/// Number of UDP video shards emitted for this frame.
	pub packet_count: usize,
	/// Stale compositor frames discarded before this frame was encoded.
	pub stale_frames_dropped: u32,
	/// Whether this frame is a key (IDR) frame.
	pub is_key_frame: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
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

	/// Fully negotiated codec, chroma, bit depth, and color representation.
	pub format: NegotiatedVideoFormat,

	/// Maximum number of reference frames for the video encoder.
	pub max_reference_frames: u32,

	/// Whether the client has enabled video encryption.
	pub encrypt_video: bool,
}

impl VideoStreamContext {
	/// Names of negotiated properties whose change requires a new stream epoch.
	///
	/// Keep this list next to the context definition so newly-added negotiated
	/// fields cannot silently fall through the reconnect fast path.
	pub(crate) fn changed_fields(&self, requested: &Self) -> Vec<&'static str> {
		let mut changed = Vec::new();
		macro_rules! changed {
			($field:ident, $name:literal) => {
				if self.$field != requested.$field {
					changed.push($name);
				}
			};
		}
		changed!(width, "width");
		changed!(height, "height");
		changed!(fps, "fps");
		changed!(packet_size, "packet size");
		changed!(bitrate, "bitrate");
		changed!(minimum_fec_packets, "minimum FEC packets");
		changed!(qos, "QoS");
		changed!(max_reference_frames, "max reference frames");
		changed!(encrypt_video, "video encryption");
		if self.format.codec != requested.format.codec {
			changed.push("codec");
		}
		if self.format.chroma != requested.format.chroma {
			changed.push("chroma sampling");
		}
		if self.format.bit_depth != requested.format.bit_depth {
			changed.push("bit depth");
		}
		if self.format.hdr != requested.format.hdr {
			changed.push("dynamic range");
		}
		if self.format.primaries != requested.format.primaries {
			changed.push("color primaries");
		}
		if self.format.transfer != requested.format.transfer {
			changed.push("transfer function");
		}
		if self.format.matrix != requested.format.matrix {
			changed.push("matrix coefficients");
		}
		if self.format.range != requested.format.range {
			changed.push("color range");
		}
		let known_format_change = self.format.codec != requested.format.codec
			|| self.format.chroma != requested.format.chroma
			|| self.format.bit_depth != requested.format.bit_depth
			|| self.format.hdr != requested.format.hdr
			|| self.format.primaries != requested.format.primaries
			|| self.format.transfer != requested.format.transfer
			|| self.format.matrix != requested.format.matrix
			|| self.format.range != requested.format.range;
		if self.format != requested.format && !known_format_change {
			changed.push("other video format property");
		}
		changed
	}
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
	fec_feedback_tx: watch::Sender<FrameFecStatus>,
	packet_tx: mpsc::Sender<VideoPacketMessage>,
	reconfigure_tx: std::sync::mpsc::Sender<VideoReconfigureCommand>,
}

pub(super) struct VideoReconfigureCommand {
	pub context: VideoStreamContext,
	pub applied: tokio::sync::oneshot::Sender<Result<(), ()>>,
}

pub(super) enum VideoPacketMessage {
	Batch(ShardBatch),
	Pause(tokio::sync::oneshot::Sender<()>),
	BeginEpoch {
		context: VideoStreamContext,
		ready: tokio::sync::oneshot::Sender<()>,
	},
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

	/// Stop delivering packets while a changed reconnect is negotiated.
	pub async fn pause_for_reconfigure(&self) -> Result<(), ()> {
		let (ready, waiting) = tokio::sync::oneshot::channel();
		self.packet_tx
			.send(VideoPacketMessage::Pause(ready))
			.await
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())
	}

	/// Replace the encoder/packetizer epoch and wait until it is ready.
	pub async fn reconfigure(&self, context: VideoStreamContext) -> Result<(), ()> {
		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.reconfigure_tx
			.send(VideoReconfigureCommand { context, applied })
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())?
	}

	pub(crate) fn report_fec_status(&self, mut status: FrameFecStatus) {
		tracing::trace!(
			frame_index = status.frame_index,
			highest_sequence = status.highest_received_sequence_number,
			next_contiguous_sequence = status.next_contiguous_sequence_number,
			missing_before_highest = status.missing_packets_before_highest,
			data_packets = status.total_data_packets,
			parity_packets = status.total_parity_packets,
			received_data_packets = status.received_data_packets,
			received_parity_packets = status.received_parity_packets,
			fec_percentage = status.fec_percentage,
			block_index = status.block_index,
			block_count = status.block_count,
			"Received Moonlight frame FEC status"
		);
		let serial = self.fec_feedback_tx.borrow().serial.wrapping_add(1).max(1);
		status.serial = serial;
		self.fec_feedback_tx.send_replace(status);
	}

	/// Clone the start notify for external triggering (e.g. bench binary).
	pub fn clone_start_notify(&self) -> Arc<Notify> {
		self.notify.clone()
	}
}

pub(crate) struct VideoStream {
	socket: UdpGsoSocket,
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

		let socket = UdpGsoSocket::new(&address, config.port).await?;

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

		// Reference frame invalidation broadcast channel. Sized for a small burst
		// of loss reports; the encode loop drains all pending each iteration.
		let (invalidate_tx, _invalidate_rx) = broadcast::channel(16);

		// Stream-reset broadcast channel (client reconnect/resume).
		let (reset_tx, _reset_rx) = broadcast::channel(1);
		let (fec_feedback_tx, fec_feedback_rx) = watch::channel(FrameFecStatus::default());

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<VideoPacketMessage>(128);
		diagnostics::spawn_watchdog(stop.clone(), packet_tx.downgrade());
		let (reconfigure_tx, reconfigure_rx) = std::sync::mpsc::channel();
		let pacing_bitrate =
			(context.format.codec == VideoCodec::PyroWave).then(|| u64::try_from(context.bitrate).unwrap_or(u64::MAX));

		// Spawn packet handler — gated behind start_notify.
		spawn_handle_video_packets(
			packet_rx,
			socket,
			start_notify.clone(),
			stop.clone(),
			pacing_bitrate,
			context.fps,
		);

		// Spawn pipeline thread — gated behind start_notify.
		VideoPipeline::new(
			frame_rx,
			config,
			context,
			keys_rx,
			packet_tx.clone(),
			idr_tx.clone(),
			idr_tx.subscribe(),
			invalidate_tx.subscribe(),
			reset_tx.subscribe(),
			stop.clone(),
			hdr_metadata_tx,
			start_notify.clone(),
			stats_tx,
			fec_feedback_rx,
			reconfigure_rx,
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			notify: start_notify,
			idr_tx,
			invalidate_tx,
			reset_tx,
			fec_feedback_tx,
			packet_tx,
			reconfigure_tx,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<VideoPacketMessage>,
	socket: UdpGsoSocket,
	start: Arc<Notify>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	mut pacing_bitrate: Option<u64>,
	mut fps: u32,
) {
	tokio::spawn(async move {
		start.notified().await;

		let mut buf = [0; 1024];
		let mut client_address = None;
		// Rate-limits the GSO-fallback warning.
		let mut last_send_warn: Option<std::time::Instant> = None;
		let mut transport_window = diagnostics::TransportWindow::new();

		// Trigger session shutdown if we exit unexpectedly.
		let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoPacketHandlerStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		while !stop_session_manager.is_shutdown_triggered() {
			tokio::select! {
				message = stop_session_manager.wrap_cancel(packet_rx.recv()) => {
					match message {
						Ok(Some(VideoPacketMessage::Pause(ready))) => {
							client_address = None;
							let _ = ready.send(());
						},
						Ok(Some(VideoPacketMessage::BeginEpoch { context, ready })) => {
							pacing_bitrate = (context.format.codec == VideoCodec::PyroWave)
								.then(|| u64::try_from(context.bitrate).unwrap_or(u64::MAX));
							fps = context.fps;
							let tos = if context.qos { 160 } else { 0 };
							let _ = socket.set_tos_v4(tos);
							let _ = ready.send(());
						},
						Ok(Some(VideoPacketMessage::Batch(mut batch))) => {
							if let Some(addr) = client_address {
								if batch.shard_count() == 0 {
									continue;
								}

								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown.
								match stop_session_manager
									.wrap_cancel(socket.send_batch(&batch, addr, pacing_bitrate))
									.await
								{
									Ok(send_stats) => {
										transport_window.record(&send_stats, packet_rx.len());
										if send_stats.fallback_chunks > 0
											&& last_send_warn
												.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
										{
											tracing::warn!(
												"GSO send failed for {} chunk(s), sent per-shard instead",
												send_stats.fallback_chunks
											);
											last_send_warn = Some(std::time::Instant::now());
										}
										if tracing::enabled!(tracing::Level::TRACE) {
											let effective_wire_bitrate = if send_stats.elapsed.is_zero() {
												0
											} else {
												(u128::from(send_stats.wire_bytes)
													.saturating_mul(8)
													.saturating_mul(1_000_000)
													/ send_stats.elapsed.as_micros().max(1))
													.min(u128::from(u64::MAX)) as u64
											};
											tracing::trace!(
												frame_number = batch.frame_number(),
												fps,
												shard_size = batch.shard_size(),
												encoded_bytes = batch.encoded_size(),
												wire_bytes = send_stats.wire_bytes,
												data_shards = batch.data_shards(),
												parity_shards = batch.parity_shards(),
												total_shards = batch.shard_count(),
												fec_blocks = batch.fec_blocks(),
												gso_segments_per_send = send_stats.gso_segments_per_send,
												gso_sends = send_stats.gso_sends,
												final_chunk_segments = send_stats.final_chunk_segments,
												final_chunk_bytes = send_stats.final_chunk_bytes,
												per_shard_sends = send_stats.per_shard_sends,
												would_block_events = send_stats.would_block_events,
												gso_fallback_chunks = send_stats.fallback_chunks,
												send_us = duration_micros_u64(send_stats.elapsed),
												pacing_window_us = duration_micros_u64(send_stats.pacing_duration),
												scheduled_pacing_us = duration_micros_u64(send_stats.scheduled_pacing_duration),
												initial_pacing_lateness_us = duration_micros_u64(send_stats.initial_pacing_lateness),
												max_pacing_lateness_us = duration_micros_u64(send_stats.max_pacing_lateness),
												pacing_rebased = send_stats.pacing_rebased,
												backpressure_rebases = send_stats.backpressure_rebases,
												effective_wire_bitrate,
												"Video frame transport"
											);
										}
									},
									Err(_) => break,
								}
							}
							batch.notify_sent();
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

#[cfg(test)]
mod tests {
	use super::*;

	fn config(max_packet_size: usize) -> VideoStreamConfig {
		VideoStreamConfig {
			max_packet_size,
			..Default::default()
		}
	}

	#[test]
	fn no_cap_honors_requested() {
		assert_eq!(config(0).clamp_packet_size(1392), 1392);
	}

	#[test]
	fn smaller_client_request_is_honored() {
		assert_eq!(config(1200).clamp_packet_size(1024), 1024);
	}

	#[test]
	fn larger_client_request_is_capped() {
		assert_eq!(config(1200).clamp_packet_size(1392), 1200);
	}

	#[test]
	fn undersized_cap_is_ignored() {
		assert_eq!(config(50).clamp_packet_size(1392), 1392);
	}
}
