use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::session::SessionKeysReceiver;
use crate::session::compositor::frame::{ExportedFrame, HdrModeState};
use crate::session::manager::SessionShutdownReason;

mod gso_socket;
mod packetizer;
mod pipeline;
mod shard_batch;
use gso_socket::UdpGsoSocket;
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

	/// Whether the client asked for full-range (0-255) rather than
	/// limited-range (16-235) luma.
	pub full_range: bool,

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
	/// Set on resume to arm a stream reset; the packet handler fires it once it has
	/// re-learned the reconnecting client's address (see `request_reset`).
	resume_pending: Arc<AtomicBool>,
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
			resume_pending,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<ShardBatch>,
	socket: UdpGsoSocket,
	start: Arc<Notify>,
	reset_tx: broadcast::Sender<()>,
	resume_pending: Arc<AtomicBool>,
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

								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown.
								match stop_session_manager
									.wrap_cancel(socket.send_batch(&batch, addr))
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
