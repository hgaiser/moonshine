use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};

use crate::session::SessionKeysReceiver;
use crate::session::compositor::CompositorFrames;
use crate::session::compositor::frame::HdrModeState;
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::{StartSignal, StartWaiter};

mod gso_socket;
mod packetizer;
mod pipeline;
mod shard_batch;

pub use crate::session::compositor::frame::FrameColorSpace;
use gso_socket::UdpGsoSocket;
pub(crate) use pipeline::KcmpProbe;
use pipeline::{MAX_FRAMES_IN_FLIGHT, PacketBatch, VideoPipeline};

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

	/// Size of the video socket's send buffer, in bytes.
	///
	/// Data in the send buffer has already left the pipeline, so it isn't
	/// covered by its frame dropping: when the host's own uplink is slower
	/// than the bitrate, up to this much video can queue in the kernel. The
	/// buffer only fills when that uplink is the bottleneck.
	///
	/// `0` sizes it to fit a keyframe burst (about eight average frames at the
	/// stream's bitrate and frame rate), keeping a larger host default. A
	/// non-zero value sets the size exactly, even below the host default: a
	/// smaller buffer lowers the worst-case queueing, at the cost of keyframes
	/// waiting for the socket to drain. The kernel caps it at
	/// `net.core.wmem_max`.
	#[serde(default)]
	pub send_buffer_size: usize,

	/// How the encoder holds the stream to its bitrate.
	#[serde(default)]
	pub rate_control: RateControlMode,
}

/// How the encoder holds the stream to its bitrate. Either way, no frame goes
/// over the per-frame budget (bitrate / frame rate).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateControlMode {
	/// Constant bitrate. Some drivers (RADV) pad frames that need fewer bits
	/// with filler data up to the budget.
	#[default]
	Cbr,
	/// Variable bitrate capped at the bitrate: frames that need fewer bits are
	/// sent at their size, without filler. Needs a driver that supports VBR
	/// encoding.
	Vbr,
}

/// Smallest accepted packet size. Lower values would leave almost no room for
/// payload after the 16-byte NV video header, so a lower cap is ignored and a
/// smaller client request is rejected.
const MIN_PACKET_SIZE: usize = 200;
/// Largest accepted packet size, so one stream packet fits an IPv6 UDP datagram:
/// 65_535 - 40 (IPv6) - 8 (UDP) - 16 (stream overhead). The packetizer sizes its
/// buffers from the client's value, so an unbounded request could exhaust memory.
const MAX_PACKET_SIZE: usize = 65_471;

impl VideoStreamConfig {
	/// Whether a negotiated packet size is safe to packetize with.
	pub(crate) fn valid_packet_size(size: usize) -> bool {
		(MIN_PACKET_SIZE..=MAX_PACKET_SIZE).contains(&size)
	}

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
			encrypt: false,
			log_frame_spikes: false,
			max_packet_size: 0,
			send_buffer_size: 0,
			rate_control: RateControlMode::default(),
		}
	}
}

/// Average frames a keyframe burst spans, FEC included, for sizing the
/// socket's send buffer when `send_buffer_size` is automatic.
const KEYFRAME_BURST_FRAMES: usize = 8;

/// Send buffer size that fits a keyframe burst at `bitrate` (bits per second)
/// and `fps`.
fn keyframe_burst_bytes(bitrate: usize, fps: u32) -> usize {
	let fps = usize::try_from(fps.max(1)).unwrap_or(usize::MAX);
	bitrate / 8 / fps * KEYFRAME_BURST_FRAMES
}

/// Statistics line format a `frame` line follows. Version 1 is [`FrameStats`]'s fields in
/// declaration order with their own names, every duration as whole microseconds, absent durations
/// omitted.
pub const STATS_VERSION: u8 = 1;

/// Writes a duration as whole microseconds, the unit every duration in this format uses.
fn serialize_micros<S: serde::Serializer>(duration: &std::time::Duration, serializer: S) -> Result<S::Ok, S::Error> {
	serializer.serialize_u64(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX))
}

/// Writes an optional duration as whole microseconds. Pair with `skip_serializing_if` so absent
/// durations leave the record entirely.
fn serialize_micros_opt<S: serde::Serializer>(
	duration: &Option<std::time::Duration>,
	serializer: S,
) -> Result<S::Ok, S::Error> {
	match duration {
		Some(duration) => serialize_micros(duration, serializer),
		None => serializer.serialize_none(),
	}
}

/// Writes a point in time as microseconds since the Unix epoch.
fn serialize_unix_micros<S: serde::Serializer>(time: &std::time::SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
	let since_epoch = time.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
	serialize_micros(&since_epoch, serializer)
}

/// Per-frame encoding statistics emitted by the video pipeline.
///
/// Sent in `StreamEvent::Frame`.
///
/// This struct's serde representation is the statistics socket's wire format: fields are written
/// with their own names in declaration order, durations as whole microseconds, and an absent duration is
/// omitted. A field added here is published on the socket by the same commit, so a field that must
/// stay internal needs `#[serde(skip)]`. `src/stats_socket.rs` locks the resulting format.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FrameStats {
	/// The frame's number on the wire (its frame index), which the client sees too, so the two
	/// sides' records of a frame can be matched.
	pub frame_number: u32,
	/// When the frame was captured, by the wall clock.
	#[serde(serialize_with = "serialize_unix_micros")]
	pub captured_at: std::time::SystemTime,
	/// The color space of the captured content; `None` for a keyframe re-encoded without a new
	/// capture.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub content: Option<FrameColorSpace>,
	/// Time the frame spent waiting in the compositor's output channel.
	#[serde(serialize_with = "serialize_micros")]
	pub channel_wait: std::time::Duration,
	/// Time spent importing the DMA-BUF into Vulkan.
	#[serde(serialize_with = "serialize_micros")]
	pub import: std::time::Duration,
	/// Time spent on GPU color conversion.
	#[serde(serialize_with = "serialize_micros")]
	pub convert: std::time::Duration,
	/// Time spent submitting the frame to the asynchronous encoder.
	#[serde(serialize_with = "serialize_micros")]
	pub submit: std::time::Duration,
	/// Time between submit completion and the packet consumer awaiting the encode future.
	#[serde(serialize_with = "serialize_micros")]
	pub consumer_queue: std::time::Duration,
	/// Elapsed wall time from submit completion until the consumer obtains the encoded packet.
	/// Includes time queued before the consumer polls the encode future.
	#[serde(serialize_with = "serialize_micros")]
	pub encode_wait: std::time::Duration,
	/// GPU video encode command duration, when the video queue supports timestamps.
	/// This excludes conversion, queueing, and bitstream readback, and is part of `encode_wait`.
	#[serde(serialize_with = "serialize_micros_opt", skip_serializing_if = "Option::is_none")]
	pub gpu_encode: Option<std::time::Duration>,
	/// Pixelforge's wall time from encode submission until bitstream readback finishes.
	/// Available when the video queue supports timestamps. This includes GPU
	/// scheduling and readback, but not the consumer's post-completion scheduling, and is part of
	/// `encode_wait`.
	#[serde(serialize_with = "serialize_micros_opt", skip_serializing_if = "Option::is_none")]
	pub bitstream_ready: Option<std::time::Duration>,
	/// Time spent packetizing the encoded data.
	#[serde(serialize_with = "serialize_micros")]
	pub packetize: std::time::Duration,
	/// Time spent sending the packets over the channel.
	#[serde(serialize_with = "serialize_micros")]
	pub send: std::time::Duration,
	/// Total end-to-end latency for this frame.
	#[serde(serialize_with = "serialize_micros")]
	pub total: std::time::Duration,
	/// Number of bytes encoded for this frame.
	pub encoded_bytes: usize,
	/// Whether this frame is a key (IDR) frame.
	pub is_key_frame: bool,
}

/// A video stream's start, frames and end, in that order.
///
/// Receivable through `SessionManager::bench_stats_receiver()`.
#[derive(Clone, Debug)]
pub enum StreamEvent {
	/// The encoder for a new stream is ready.
	Started(StreamInfo),
	/// A frame was encoded and sent.
	Frame(FrameStats),
	/// The stream ended, after its last frame.
	Stopped,
}

/// Where the video pipeline sends its [`StreamEvent`]s. It also keeps the stream in progress, so
/// a reader that joins partway through can be told what it missed.
#[derive(Clone)]
pub struct StatsSender {
	events: broadcast::Sender<StreamEvent>,
	/// The stream in progress. Locked while a stream's start or end is broadcast, and while a
	/// reader reads it and subscribes, so a reader sees each start and end exactly once: in this,
	/// or as an event.
	current: Arc<std::sync::Mutex<Option<StreamInfo>>>,
}

impl StatsSender {
	pub(crate) fn new(capacity: usize) -> Self {
		Self {
			events: broadcast::channel(capacity).0,
			current: Default::default(),
		}
	}

	/// Every event from now on.
	pub(crate) fn subscribe(&self) -> broadcast::Receiver<StreamEvent> {
		self.events.subscribe()
	}

	/// The stream in progress, if any, and every event after it.
	pub(crate) fn join(&self) -> (Option<StreamInfo>, broadcast::Receiver<StreamEvent>) {
		let current = self.lock_current();
		(*current, self.events.subscribe())
	}

	pub(crate) fn started(&self, info: StreamInfo) {
		let mut current = self.lock_current();
		*current = Some(info);
		let _ = self.events.send(StreamEvent::Started(info));
	}

	pub(crate) fn frame(&self, stats: FrameStats) {
		let _ = self.events.send(StreamEvent::Frame(stats));
	}

	pub(crate) fn stopped(&self) {
		let mut current = self.lock_current();
		*current = None;
		let _ = self.events.send(StreamEvent::Stopped);
	}

	fn lock_current(&self) -> std::sync::MutexGuard<'_, Option<StreamInfo>> {
		self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// The video stream as negotiated with the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct StreamInfo {
	/// Width in pixels.
	pub width: u32,
	/// Height in pixels.
	pub height: u32,
	/// Frames per second.
	pub fps: u32,
	/// Target bitrate in bits per second.
	pub bitrate: usize,
	#[serde(rename = "codec")]
	pub video_format: VideoFormat,
	pub dynamic_range: VideoDynamicRange,
	#[serde(rename = "chroma")]
	pub chroma_sampling_type: VideoChromaSampling,
}

impl From<&VideoStreamContext> for StreamInfo {
	fn from(context: &VideoStreamContext) -> Self {
		Self {
			width: context.width,
			height: context.height,
			fps: context.fps,
			bitrate: context.bitrate,
			video_format: context.video_format,
			dynamic_range: context.dynamic_range,
			chroma_sampling_type: context.chroma_sampling_type,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
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
/// The pipeline and packet handler are spawned immediately but wait for the start flag
/// until `trigger()` is called on `StartB`.
#[derive(Clone)]
pub(crate) struct VideoStreamHandle {
	start: StartSignal,
	idr_tx: broadcast::Sender<()>,
	/// Reference frame invalidation requests, carrying the inclusive
	/// `[first, last]` client frame-index range the client could not decode.
	invalidate_tx: broadcast::Sender<(u32, u32)>,
	/// Set by a resume; the packet handler fires the reset on the client's next PING.
	resume_pending: Arc<AtomicBool>,
}

impl VideoStreamHandle {
	/// Signal the video pipeline and packet handler to begin processing.
	pub fn trigger(&self) {
		self.start.trigger();
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
	///
	/// The reset waits for the client's next PING: a reconnecting client usually arrives
	/// from a new UDP port, and firing immediately would send the IDR to the old address.
	pub fn request_reset(&self) {
		self.resume_pending.store(true, Ordering::Relaxed);
	}

	/// The start signal, for external triggering (e.g. bench binary).
	pub fn start_signal(&self) -> StartSignal {
		self.start.clone()
	}
}

pub(crate) struct VideoStream {
	socket: UdpGsoSocket,
	frames: CompositorFrames,
	hdr_metadata_tx: watch::Sender<HdrModeState>,
	stats_tx: StatsSender,
}

impl VideoStream {
	pub async fn new(
		config: VideoStreamConfig,
		address: String,
		frames: CompositorFrames,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
		_stop: ShutdownManager<SessionShutdownReason>,
		stats_tx: StatsSender,
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
			frames,
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
			frames,
			hdr_metadata_tx,
			stats_tx,
		} = self;

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = socket.set_tos_v4(160);
		}

		// Room for a keyframe burst, which is several times the average frame,
		// unless configured otherwise.
		if config.send_buffer_size == 0 {
			socket.grow_send_buffer(keyframe_burst_bytes(context.bitrate, context.fps));
		} else {
			socket.set_send_buffer(config.send_buffer_size);
		}

		// Gate for pipeline + packet handler.
		let start = StartSignal::new();

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Reference frame invalidation broadcast channel. Sized for a small burst
		// of loss reports; the encode loop drains all pending each iteration.
		let (invalidate_tx, _invalidate_rx) = broadcast::channel(16);

		// Stream-reset broadcast channel (client reconnect/resume).
		let (reset_tx, _reset_rx) = broadcast::channel(1);
		let resume_pending = Arc::new(AtomicBool::new(false));

		// Packet channel. Each queued batch holds its frame's in-flight slot, so the
		// channel never holds more than the in-flight limit.
		let (packet_tx, packet_rx) = mpsc::channel::<PacketBatch>(MAX_FRAMES_IN_FLIGHT);

		// Spawn packet handler — gated behind start.
		spawn_handle_video_packets(
			packet_rx,
			socket,
			start.waiter(),
			reset_tx.clone(),
			resume_pending.clone(),
			stop.clone(),
		);

		// Spawn pipeline thread — gated behind start.
		VideoPipeline::new(
			frames,
			config,
			context,
			keys_rx,
			packet_tx,
			idr_tx.clone(),
			idr_tx.subscribe(),
			invalidate_tx.subscribe(),
			reset_tx.subscribe(),
			stop.clone(),
			hdr_metadata_tx,
			start.waiter(),
			stats_tx,
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			start,
			idr_tx,
			invalidate_tx,
			resume_pending,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<PacketBatch>,
	socket: UdpGsoSocket,
	mut start: StartWaiter,
	reset_tx: broadcast::Sender<u32>,
	resume_pending: Arc<AtomicBool>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
) {
	tokio::spawn(async move {
		if !start.wait(&stop_session_manager).await {
			return;
		}

		let mut buf = [0; 1024];
		let mut client_address = None;
		// Epoch of the last reset fired; batches packetized before it carry old counters.
		let mut reset_epoch = 0u32;
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
							// Taking the batch frees its frame's in-flight slot, so a slow
							// send doesn't hold back encoding; a stall still fills the
							// slots with the batches queued behind it.
							let (epoch, shards) = batch.into_parts();
							if let Some(addr) = client_address {
								// Frames from before a resume's reset would reach the new client
								// with old frame numbers, making it drop the reset frames as stale.
								if epoch < reset_epoch || shards.shard_count() == 0 {
									continue;
								}

								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown.
								match stop_session_manager
									.wrap_cancel(socket.send_batch(&shards, addr))
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

						// Fire a resume's reset now that frames reach the client's current address.
						if resume_pending.swap(false, Ordering::Relaxed) {
							tracing::info!("Resumed client address is {address}, resetting video stream.");
							reset_epoch += 1;
							let _ = reset_tx.send(reset_epoch);
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

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn shutdown_before_start_releases_packet_handler() {
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let (packet_tx, packet_rx) = mpsc::channel(1);
		let (reset_tx, _) = broadcast::channel(1);
		let start = StartSignal::new();
		let stop = ShutdownManager::new();
		spawn_handle_video_packets(
			packet_rx,
			socket,
			start.waiter(),
			reset_tx,
			Arc::new(AtomicBool::new(false)),
			stop.clone(),
		);

		stop.trigger_shutdown(SessionShutdownReason::ManagerShutdown).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), packet_tx.closed())
			.await
			.expect("packet handler kept its socket after shutdown");
	}

	#[tokio::test]
	async fn stalled_sender_fills_in_flight_slots() {
		use std::sync::atomic::AtomicUsize;

		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let (packet_tx, packet_rx) = mpsc::channel(MAX_FRAMES_IN_FLIGHT);
		let (reset_tx, _) = broadcast::channel(1);
		let start = StartSignal::new();
		let stop = ShutdownManager::new();
		// Until started, the sender doesn't drain the channel, like a stalled socket.
		spawn_handle_video_packets(
			packet_rx,
			socket,
			start.waiter(),
			reset_tx,
			Arc::new(AtomicBool::new(false)),
			stop.clone(),
		);

		let in_flight = Arc::new(AtomicUsize::new(0));
		for _ in 0..MAX_FRAMES_IN_FLIGHT {
			packet_tx.try_send(PacketBatch::holding_slot(&in_flight)).unwrap();
		}
		// Queued frames keep their slots, so the encoding thread's gate closes and
		// it drops captures instead of the queue growing.
		assert_eq!(in_flight.load(Ordering::Relaxed), MAX_FRAMES_IN_FLIGHT);
		assert_eq!(packet_tx.capacity(), 0);

		// Once the sender drains the queue, the slots free up.
		start.trigger();
		tokio::time::timeout(std::time::Duration::from_secs(1), async {
			while in_flight.load(Ordering::Relaxed) > 0 {
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("sender kept in-flight slots after taking their batches");
	}

	#[test]
	fn keyframe_burst_scales_with_frame_size() {
		// 80 Mbps at 60 fps: ~166 KB average frames.
		assert_eq!(keyframe_burst_bytes(80_000_000, 60), 166_666 * KEYFRAME_BURST_FRAMES);
		// Doubling the frame rate halves the frames, and the burst.
		assert_eq!(keyframe_burst_bytes(80_000_000, 120), 83_333 * KEYFRAME_BURST_FRAMES);
		// A zero frame rate doesn't divide by zero.
		assert_eq!(keyframe_burst_bytes(8_000, 0), 1_000 * KEYFRAME_BURST_FRAMES);
	}

	fn config(max_packet_size: usize) -> VideoStreamConfig {
		VideoStreamConfig {
			max_packet_size,
			..Default::default()
		}
	}

	#[test]
	fn rejects_out_of_range_packet_sizes() {
		assert!(!VideoStreamConfig::valid_packet_size(0));
		assert!(!VideoStreamConfig::valid_packet_size(16));
		assert!(!VideoStreamConfig::valid_packet_size(MIN_PACKET_SIZE - 1));
		assert!(VideoStreamConfig::valid_packet_size(MIN_PACKET_SIZE));
		assert!(VideoStreamConfig::valid_packet_size(1392));
		assert!(VideoStreamConfig::valid_packet_size(MAX_PACKET_SIZE));
		assert!(!VideoStreamConfig::valid_packet_size(MAX_PACKET_SIZE + 1));
		assert!(!VideoStreamConfig::valid_packet_size(usize::MAX));
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
