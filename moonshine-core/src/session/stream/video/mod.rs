pub mod pyrowave_protocol;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};

use crate::session::authorization::MediaStream;
use crate::session::compositor::frame::HdrModeState;
use crate::session::lifecycle::{StartLatch, StartWaiter, WorkerGuard};
use crate::session::manager::SessionShutdownReason;
use crate::session::{AuthorizationReceiver, SessionKeysReceiver};

mod diagnostics;
pub(crate) mod fec;
mod format;
mod gso_socket;
mod pacing_timer;
mod packetizer;
mod pipeline;
pub(crate) mod pyrowave;
pub use pyrowave::PyroWaveQueueMode;
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

	/// Emit periodic capture, pipeline, transport, and runtime statistics.
	/// Disabling this also skips diagnostic accumulation and process sampling;
	/// benchmark frame statistics and operational warnings remain available.
	pub log_stats: bool,
	/// GPU scheduling preference for cross-process PyroWave encoding; auto prefers graphics.
	pub pyrowave_queue: pyrowave::PyroWaveQueueMode,

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
	///
	/// The cap bounds the on-wire datagram at `max_packet_size + 16` bytes. An
	/// encrypted stream's packet size excludes its 32-byte per-shard prefix
	/// (Moonlight subtracts it before announcing), so the cap does as well.
	pub(crate) fn clamp_packet_size(&self, requested: usize, encrypted: bool) -> usize {
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
		let cap = if encrypted {
			self.max_packet_size - packetizer::ENC_PREFIX_SIZE
		} else {
			self.max_packet_size
		};
		requested.min(cap)
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
			log_stats: true,
			pyrowave_queue: pyrowave::PyroWaveQueueMode::Auto,
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
	/// Queue handoff/residence measured separately from actual socket work.
	pub enqueue: std::time::Duration,
	/// Actual socket work through final submission or failure.
	pub send: std::time::Duration,
	/// Total end-to-end latency for this frame.
	pub total: std::time::Duration,
	/// Number of bytes encoded for this frame.
	pub encoded_bytes: usize,
	/// Successfully submitted UDP payload bytes including FEC and encryption;
	/// kernel acceptance does not confirm receiver delivery.
	pub wire_bytes: usize,
	/// Number of UDP video shards successfully submitted to the kernel.
	pub packet_count: usize,
	/// Logical UDP attempts; readiness retries/fallback duplication are excluded.
	pub attempted_packet_count: usize,
	pub failed_packet_count: usize,
	pub discarded_packet_count: usize,
	/// Stale compositor frames discarded before this frame was encoded.
	pub stale_frames_dropped: u32,
	/// Whether this frame is a key (IDR) frame.
	pub is_key_frame: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoStreamContext {
	/// Explicit setup-time PyroWave dialect; conventional codecs use None.
	pub pyrowave_dialect: Option<pyrowave_protocol::PyroWaveDialect>,
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
	/// Check every value that timing, allocation, encoder and transport code
	/// consume. Callers validate before pausing or reconfiguring a live epoch.
	pub(crate) fn validate(&self) -> Result<(), String> {
		crate::session::negotiation::validate_display_mode(self.width, self.height, self.fps)?;
		self.format.validate().map_err(str::to_string)?;
		if self.packet_size < packetizer::MIN_VIDEO_PACKET_SIZE {
			return Err(format!(
				"video packet size {} is below the {}-byte protocol minimum",
				self.packet_size,
				packetizer::MIN_VIDEO_PACKET_SIZE
			));
		}
		match packetizer::wire_shard_size(self.packet_size, self.encrypt_video) {
			Some(shard) if shard <= gso_socket::MAX_UDP_PAYLOAD => {},
			_ => {
				return Err(format!(
					"video packet size {} does not fit one UDP datagram{}",
					self.packet_size,
					if self.encrypt_video { " with encryption" } else { "" }
				));
			},
		}
		if self.bitrate == 0 {
			return Err("video bitrate must be non-zero".to_string());
		}
		// Vulkan Video rate control takes a 32-bit bit rate; PyroWave budgets
		// with checked `usize` arithmetic and has no such limit.
		if self.format.codec != VideoCodec::PyroWave && u32::try_from(self.bitrate).is_err() {
			return Err(format!(
				"video bitrate {} bps exceeds the {} encoder's 32-bit rate-control range",
				self.bitrate, self.format.codec
			));
		}
		Ok(())
	}

	/// Names of negotiated properties whose change requires a new stream epoch.
	///
	/// Keep this list next to the context definition so newly-added negotiated
	/// fields cannot silently fall through the reconnect fast path.
	/// The virtual-output properties this stream requires from the compositor.
	pub(crate) fn output_mode(&self) -> crate::session::compositor::OutputMode {
		crate::session::compositor::OutputMode {
			width: self.width,
			height: self.height,
			refresh_rate: self.fps,
			hdr: self.format.hdr,
		}
	}

	pub(crate) fn changed_fields(&self, requested: &Self) -> Vec<&'static str> {
		let mut changed = Vec::new();
		macro_rules! changed {
			($field:ident, $name:literal) => {
				if self.$field != requested.$field {
					changed.push($name);
				}
			};
		}
		changed!(pyrowave_dialect, "PyroWave dialect");
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
/// The pipeline and packet handler are registered with the session and spawned
/// immediately, then wait on a persistent [`StartLatch`] until `StartB`.
#[derive(Clone)]
pub(crate) struct VideoStreamHandle {
	start: StartLatch,
	idr_tx: broadcast::Sender<()>,
	/// Reference frame invalidation requests, carrying the inclusive
	/// `[first, last]` client frame-index range the client could not decode.
	invalidate_tx: broadcast::Sender<(u32, u32)>,
	reset_tx: std::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), ()>>>,
	fec_feedback_tx: watch::Sender<FrameFecStatus>,
	packet_tx: mpsc::Sender<VideoPacketMessage>,
	pause_tx: watch::Sender<u64>,
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
	/// Idempotent: a duplicate `StartB` has no further effect.
	pub fn trigger(&self) {
		if !self.start.open() {
			tracing::debug!("Ignoring duplicate video start signal");
		}
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
	pub async fn request_reset(&self) -> Result<(), ()> {
		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.reset_tx.send(applied).map_err(|_| ())?;
		waiting.await.map_err(|_| ())?
	}

	/// Stop delivering packets until the encoder activates the next client epoch.
	pub async fn pause_for_reconfigure(&self) -> Result<(), ()> {
		// Interrupt socket waits before ordering the pause barrier behind old work.
		self.pause_tx
			.send_modify(|generation| *generation = generation.wrapping_add(1));
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

	/// The stream's start latch, for external triggering (e.g. bench binary).
	pub(crate) fn start_latch(&self) -> StartLatch {
		self.start.clone()
	}
}

/// Receivers behind a [`VideoStreamHandle::for_test`] handle.
#[cfg(test)]
pub(super) struct VideoHandleProbe {
	pub(super) idr_rx: broadcast::Receiver<()>,
	pub(super) packet_rx: mpsc::Receiver<VideoPacketMessage>,
}

#[cfg(test)]
impl VideoStreamHandle {
	/// A handle not connected to a pipeline, for control-stream tests.
	pub(super) fn for_test() -> (Self, VideoHandleProbe) {
		let (idr_tx, idr_rx) = broadcast::channel(16);
		let (packet_tx, packet_rx) = mpsc::channel(16);
		let handle = Self {
			pause_tx: watch::channel(0u64).0,
			start: StartLatch::new(),
			idr_tx,
			invalidate_tx: broadcast::channel(16).0,
			reset_tx: std::sync::mpsc::channel().0,
			fec_feedback_tx: watch::channel(FrameFecStatus::default()).0,
			packet_tx,
			reconfigure_tx: std::sync::mpsc::channel().0,
		};
		(handle, VideoHandleProbe { idr_rx, packet_rx })
	}
}

pub(crate) struct VideoStream {
	socket: UdpGsoSocket,
	frame_rx: crate::session::compositor::admission::CaptureReceiver,
	hdr_metadata_tx: watch::Sender<HdrModeState>,
	stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
}

impl VideoStream {
	pub async fn new(
		config: VideoStreamConfig,
		address: String,
		frame_rx: crate::session::compositor::admission::CaptureReceiver,
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
		authorization_rx: AuthorizationReceiver,
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

		// Persistent gate for pipeline + packet handler.
		let start = StartLatch::new();

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Reference frame invalidation broadcast channel. Sized for a small burst
		// of loss reports; the encode loop drains all pending each iteration.
		let (invalidate_tx, _invalidate_rx) = broadcast::channel(16);

		// Stream-reset broadcast channel (client reconnect/resume).
		let (reset_tx, reset_rx) = std::sync::mpsc::channel();
		let (fec_feedback_tx, fec_feedback_rx) = watch::channel(FrameFecStatus::default());

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<VideoPacketMessage>(128);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		if config.log_stats {
			diagnostics::spawn_watchdog(stop.clone(), packet_tx.downgrade());
		}
		let (reconfigure_tx, reconfigure_rx) = std::sync::mpsc::channel();
		let pacing_bitrate =
			(context.format.codec == VideoCodec::PyroWave).then(|| u64::try_from(context.bitrate).unwrap_or(u64::MAX));

		// Spawn packet handler — registered now, gated behind the start latch.
		let worker = WorkerGuard::register(&stop, SessionShutdownReason::VideoPacketHandlerStopped)?;
		spawn_handle_video_packets(
			packet_rx,
			pause_rx,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker,
			pacing_bitrate,
			context.fps,
			config.log_stats,
		);

		// Spawn pipeline thread — registered now, gated behind the start latch.
		VideoPipeline::new(
			frame_rx,
			config,
			context,
			keys_rx,
			packet_tx.clone(),
			idr_tx.clone(),
			idr_tx.subscribe(),
			invalidate_tx.subscribe(),
			reset_rx,
			stop.clone(),
			hdr_metadata_tx,
			start.waiter(),
			stats_tx,
			fec_feedback_rx,
			reconfigure_rx,
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			start,
			idr_tx,
			invalidate_tx,
			reset_tx,
			fec_feedback_tx,
			packet_tx,
			pause_tx,
			reconfigure_tx,
		})
	}
}

/// `worker` was registered by the caller before spawning, so a stop before
/// `StartB` still waits for this task to drop its socket.
#[allow(clippy::too_many_arguments)]
fn spawn_handle_video_packets(
	packet_rx: mpsc::Receiver<VideoPacketMessage>,
	mut pause_rx: watch::Receiver<u64>,
	socket: UdpGsoSocket,
	authorization: AuthorizationReceiver,
	start: StartWaiter,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	worker: WorkerGuard,
	mut pacing_bitrate: Option<u64>,
	mut fps: u32,
	log_stats: bool,
) {
	tokio::spawn(async move {
		// Declared first so it is released after the socket and channel below.
		let _worker = worker;
		let mut socket = socket;
		let mut packet_rx = packet_rx;
		if start.wait(&stop_session_manager).await.is_err() {
			tracing::debug!("Video packet handler stopped before start signal.");
			return;
		}

		let mut buf = [0; 1024];
		let mut client_address = None;
		let mut paused = false;
		// Rate-limits the GSO-fallback warning.
		let mut last_send_warn: Option<std::time::Instant> = None;
		let mut transport_window = diagnostics::TransportWindow::new(log_stats);

		while !stop_session_manager.is_shutdown_triggered() {
			tokio::select! {
				Ok(()) = pause_rx.changed() => {
					if !paused { client_address = None; }
					paused = true;
				},
				message = stop_session_manager.wrap_cancel(packet_rx.recv()) => {
					match message {
						Ok(Some(VideoPacketMessage::Pause(ready))) => {
							// The FIFO barrier can win before changed(). Consume
							// its urgent notification before acknowledging; otherwise
							// that old notification could pause the next BeginEpoch.
							pause_rx.borrow_and_update();
							if !paused {
								client_address = None;
							}
							// PING may discover the next endpoint, but only an ordered
							// BeginEpoch from the producer can enable delivery again.
							paused = true;
							let _ = ready.send(());
						},
						Ok(Some(VideoPacketMessage::BeginEpoch { context, ready })) => {
							pacing_bitrate = (context.format.codec == VideoCodec::PyroWave)
								.then(|| u64::try_from(context.bitrate).unwrap_or(u64::MAX));
							fps = context.fps;
							paused = false;
							let tos = if context.qos { 160 } else { 0 };
							let _ = socket.set_tos_v4(tos);
							let _ = ready.send(());
						},
						Ok(Some(VideoPacketMessage::Batch(mut batch))) => {
							if let Some(addr) = client_address.filter(|_| !paused) {
								if batch.shard_count() == 0 {
									continue;
								}

								batch.mark_send_started();
								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown.
								match tokio::select! {
									biased;
									Ok(()) = pause_rx.changed() => {
										paused = true;
										client_address = None;
										let completion = batch.finish(shard_batch::CompletionDisposition::Discarded);
										transport_window.record(&gso_socket::SendStats::released(completion), packet_rx.len());
										continue;
									},
									result = stop_session_manager.wrap_cancel(socket.send_batch(&mut batch, addr, pacing_bitrate)) => result,
								}
								{
									Ok(send_stats) => {
										transport_window.record(&send_stats, packet_rx.len());
										if (send_stats.fallback_chunks > 0 || send_stats.outcome.failed_datagrams > 0)
											&& last_send_warn
												.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
										{
											tracing::warn!(
												last_error = ?send_stats.outcome.last_error,
												"Video transport: {} fallback chunks, {} failed datagrams",
												send_stats.fallback_chunks, send_stats.outcome.failed_datagrams
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
									Err(_) => {
										let completion = batch.finish(shard_batch::CompletionDisposition::Cancelled);
										transport_window.record(&gso_socket::SendStats::released(completion), packet_rx.len());
										break;
									},
								}
							}
							if client_address.is_some() && !paused { batch.notify_sent(); } else { let completion = batch.finish(shard_batch::CompletionDisposition::Discarded); transport_window.record(&gso_socket::SendStats::released(completion), packet_rx.len()); }
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

					// Only the current launch/resume generation may (re)discover the
					// destination; the source port may change (NAT, client sockets).
					if authorization.borrow().admits_media_ping(MediaStream::Video, address, &buf[..len]) {
						tracing::trace!("Received video stream PING message from {address}.");
						client_address = Some(address);
					} else {
						tracing::debug!(%address, len, "Ignoring unauthorized video endpoint discovery datagram");
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
	use crate::session::stream::test_support::SocketId;

	#[tokio::test]
	async fn pause_and_stop_interrupt_blocked_network_work_and_release_all_credits() {
		use std::sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		};
		use std::time::Duration;
		for pause in [false, true] {
			let mut socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
			socket.force_no_gso_for_test();
			socket.faults.stall_after = Some(1);
			let server = socket.local_addr().unwrap();
			let stop = ShutdownManager::new();
			let (mut handle, _probe) = VideoStreamHandle::for_test();
			let (tx, rx) = mpsc::channel(16);
			handle.packet_tx = tx.clone();
			let (pause_tx, pause_rx) = watch::channel(0u64);
			handle.pause_tx = pause_tx;
			let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
			spawn_handle_video_packets(
				rx,
				pause_rx,
				socket,
				authorization_rx,
				handle.start.waiter(),
				stop.clone(),
				worker(&stop),
				None,
				120,
				false,
			);
			handle.trigger();
			let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
			client.send_to(b"PING", server).await.unwrap();
			tokio::time::sleep(Duration::from_millis(10)).await;
			let credits = Arc::new(AtomicUsize::new(3));
			let mut completions = Vec::new();
			for _ in 0..3 {
				let mut batch = shard_batch::ShardBuf::new(3, 64, 0).into_batch();
				batch.hold_until_release(shard_batch::NetworkCredit(credits.clone()));
				let (sent, completed) = std::sync::mpsc::sync_channel(1);
				batch.set_send_completion(sent);
				completions.push(completed);
				tx.send(VideoPacketMessage::Batch(batch)).await.unwrap();
			}
			// The first datagram was submitted; the second is permanently blocked.
			tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut [0; 64]))
				.await
				.unwrap()
				.unwrap();
			assert_eq!(credits.load(Ordering::Relaxed), 3);
			if pause {
				tokio::time::timeout(Duration::from_secs(1), handle.pause_for_reconfigure())
					.await
					.unwrap()
					.unwrap();
				let (ready, waiting) = tokio::sync::oneshot::channel();
				tx.send(VideoPacketMessage::BeginEpoch {
					context: VideoStreamContext {
						fps: 120,
						bitrate: 750_000_000,
						..Default::default()
					},
					ready,
				})
				.await
				.unwrap();
				waiting.await.unwrap();
			} else {
				let _ = stop.trigger_shutdown(SessionShutdownReason::VideoPacketHandlerStopped);
			}
			if pause {
				let _ = stop.trigger_shutdown(SessionShutdownReason::VideoPacketHandlerStopped);
			}
			tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
				.await
				.unwrap();
			assert_eq!(credits.load(Ordering::Relaxed), 0);
			let completion = completions[0].try_recv().unwrap();
			assert_eq!(completion.outcome.submitted_datagrams, 1);
			assert_eq!(
				completion.disposition,
				if pause {
					shard_batch::CompletionDisposition::Discarded
				} else {
					shard_batch::CompletionDisposition::Cancelled
				}
			);
			for c in &completions[1..] {
				assert_eq!(c.try_recv().unwrap().outcome.submitted_datagrams, 0);
			}
			assert!(
				tokio::time::timeout(Duration::from_millis(20), client.recv_from(&mut [0; 64]))
					.await
					.is_err()
			);
		}
	}

	#[tokio::test(flavor = "current_thread")]
	async fn queued_pause_notification_cannot_pause_the_following_epoch() {
		use std::time::Duration;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			pause_rx,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		start.open();
		for _ in 0..32 {
			// Queue both ordered barriers and the urgent notification before
			// yielding. Either notification or FIFO reception may win the select.
			let (paused, pause_ack) = tokio::sync::oneshot::channel();
			let (ready, epoch_ack) = tokio::sync::oneshot::channel();
			pause_tx.send_modify(|g| *g += 1);
			tx.send(VideoPacketMessage::Pause(paused)).await.unwrap();
			tx.send(VideoPacketMessage::BeginEpoch {
				context: VideoStreamContext::default(),
				ready,
			})
			.await
			.unwrap();
			pause_ack.await.unwrap();
			epoch_ack.await.unwrap();
			// Any unread urgent notification is now ready to run. It must not
			// disable the newly acknowledged epoch.
			tokio::time::sleep(Duration::from_millis(1)).await;
			client.send_to(b"PING", server).await.unwrap();
			tokio::time::sleep(Duration::from_millis(2)).await;
			let mut fresh = shard_batch::ShardBuf::new(1, 64, 0);
			fresh.shard_mut(0).fill(0x11);
			tx.send(VideoPacketMessage::Batch(fresh.into_batch())).await.unwrap();
			let mut buf = [0; 64];
			let (len, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
				.await
				.unwrap()
				.unwrap();
			assert_eq!(&buf[..len], &[0x11; 64]);
		}
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		stop.wait_shutdown_complete().await;
	}

	#[tokio::test]
	async fn reconnect_ping_cannot_deliver_old_batches_before_epoch_activation() {
		use std::time::Duration;
		use tokio::net::UdpSocket;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			pause_rx,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker(&stop),
			Some(650_000_000),
			120,
			false,
		);
		start.open();
		let mut buf = [0u8; 64];
		for codec in [VideoCodec::PyroWave, VideoCodec::Hevc, VideoCodec::PyroWave] {
			let (ready, waiting) = tokio::sync::oneshot::channel();
			pause_tx.send_modify(|g| *g += 1);
			tx.send(VideoPacketMessage::Pause(ready)).await.unwrap();
			waiting.await.unwrap();
			let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
			client.send_to(b"PING", server).await.unwrap();
			// Let the endpoint discovery arrive while negotiation is paused.
			tokio::time::sleep(Duration::from_millis(10)).await;
			let (ready, waiting) = tokio::sync::oneshot::channel();
			pause_tx.send_modify(|g| *g += 1);
			tx.send(VideoPacketMessage::Pause(ready)).await.unwrap();
			waiting.await.unwrap(); // Duplicate disconnect/ANNOUNCE pauses retain the new PING.
			let mut old = shard_batch::ShardBuf::new(1, 64, 0);
			old.shard_mut(0).fill(0xee);
			let mut old = old.into_batch();
			let (sent, completed) = std::sync::mpsc::sync_channel(1);
			old.set_send_completion(sent);
			tx.send(VideoPacketMessage::Batch(old)).await.unwrap();
			let (ready, waiting) = tokio::sync::oneshot::channel();
			tx.send(VideoPacketMessage::BeginEpoch {
				context: VideoStreamContext {
					fps: 120,
					bitrate: 650_000_000,
					format: NegotiatedVideoFormat {
						codec,
						..Default::default()
					},
					..Default::default()
				},
				ready,
			})
			.await
			.unwrap();
			waiting.await.unwrap();
			// Dropped old PyroWave batches still release their synchronous producer.
			assert!(completed.try_recv().is_ok());
			let mut fresh = shard_batch::ShardBuf::new(1, 64, 0);
			fresh.shard_mut(0).fill(0x11);
			tx.send(VideoPacketMessage::Batch(fresh.into_batch())).await.unwrap();
			let (len, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
				.await
				.unwrap()
				.unwrap();
			assert_eq!(&buf[..len], &[0x11; 64]);
			assert!(
				tokio::time::timeout(Duration::from_millis(10), client.recv_from(&mut buf))
					.await
					.is_err()
			);
		}
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	}

	fn worker(stop: &ShutdownManager<SessionShutdownReason>) -> WorkerGuard {
		WorkerGuard::register(stop, SessionShutdownReason::VideoPacketHandlerStopped).unwrap()
	}

	/// Spawn a packet handler on an ephemeral port and return its address
	/// and the identity of the socket it owns.
	async fn spawn_unstarted(
		stop: &ShutdownManager<SessionShutdownReason>,
		start: &StartLatch,
	) -> (std::net::SocketAddr, SocketId, mpsc::Sender<VideoPacketMessage>) {
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let address = socket.local_addr().unwrap();
		let id = SocketId::of(&socket);
		let (tx, rx) = mpsc::channel(16);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			watch::channel(0u64).1,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker(stop),
			None,
			60,
			false,
		);
		(address, id, tx)
	}

	/// STAB-001: a stop before `StartB` completes only after the packet
	/// handler released its socket, so the next session can bind the port.
	#[tokio::test]
	async fn stop_before_start_releases_the_socket_before_completion() {
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (_address, socket, _tx) = spawn_unstarted(&stop, &start).await;
		tokio::task::yield_now().await;
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		assert!(
			!socket.is_open(),
			"completed shutdown must imply the video port is free"
		);
		// A late StartB cannot resurrect the stopped handler.
		start.open();
	}

	/// STAB-001: the stop can also arrive before the spawned task is first polled.
	#[tokio::test(flavor = "current_thread")]
	async fn stop_before_first_poll_is_still_joined() {
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (_address, socket, _tx) = spawn_unstarted(&stop, &start).await;
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		assert!(!socket.is_open());
	}

	/// STAB-001: `StartB` before the handler polls its gate, and duplicate
	/// `StartB`, both leave exactly one running handler.
	#[tokio::test(flavor = "current_thread")]
	async fn early_and_duplicate_start_signals_start_the_handler() {
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (address, socket, tx) = spawn_unstarted(&stop, &start).await;
		assert!(start.open());
		assert!(!start.open());
		let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		client.send_to(b"PING", address).await.unwrap();
		let mut delivered = false;
		for _ in 0..50 {
			send_frame(&tx, 0x5a).await;
			if receives(&client, 0x5a).await {
				delivered = true;
				break;
			}
		}
		assert!(delivered, "an early StartB must not be lost");
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		assert!(!socket.is_open());
	}

	/// A port still owned by another session makes stream construction fail
	/// cleanly (the manager then tears the partial session down); it never
	/// starts a stream on a different port.
	#[tokio::test]
	async fn busy_udp_port_fails_stream_construction() {
		let occupied = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let port = occupied.local_addr().unwrap().port();
		let (_capture_tx, capture_rx) = crate::session::compositor::admission::capture_channel();
		let config = VideoStreamConfig {
			port,
			..Default::default()
		};
		assert!(
			VideoStream::new(
				config,
				"127.0.0.1".into(),
				capture_rx,
				watch::channel(HdrModeState::new(false)).0,
				ShutdownManager::new(),
				broadcast::channel(1).0,
			)
			.await
			.is_err()
		);
		let audio = crate::session::stream::audio::AudioStreamConfig { port };
		assert!(
			crate::session::stream::audio::AudioStream::new(audio, "127.0.0.1".into(), ShutdownManager::new())
				.await
				.is_err()
		);
	}

	/// TEST-001 fault injection on the real owners: `VideoStream::start` spawns
	/// the registered UDP packet handler and then fails to construct the
	/// pipeline (no verified capture device). Over 100 sessions on one fixed
	/// port, the failed start must still be joined by the session's completion
	/// and must release the port for the next session.
	#[tokio::test]
	async fn failed_pipeline_construction_releases_the_started_packet_handler() {
		let port = tokio::net::UdpSocket::bind("127.0.0.1:0")
			.await
			.unwrap()
			.local_addr()
			.unwrap()
			.port();
		let config = VideoStreamConfig {
			port,
			..Default::default()
		};
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		for cycle in 0..100 {
			let stop = ShutdownManager::new();
			let (_capture_tx, capture_rx) = crate::session::compositor::admission::capture_channel();
			let stream = VideoStream::new(
				config.clone(),
				"127.0.0.1".into(),
				capture_rx,
				watch::channel(HdrModeState::new(false)).0,
				stop.clone(),
				broadcast::channel(1).0,
			)
			.await
			.unwrap_or_else(|()| panic!("cycle {cycle}: previous session still owns the video port"));
			let keys = watch::channel(crate::session::keys::KeyLedger::default().publish(
				crate::session::SessionKeyData::new(
					crate::session::RemoteInputKey::from_bytes([1; 16]),
					crate::session::RemoteInputKeyId::new(1),
				),
			));
			let started = stream.start(
				config.clone(),
				VideoStreamContext {
					width: 1920,
					height: 1080,
					fps: 60,
					packet_size: 1392,
					bitrate: 20_000_000,
					..Default::default()
				},
				keys.1,
				authorization_rx.clone(),
				stop.clone(),
			);
			assert!(started.is_err(), "cycle {cycle}: no capture device was verified");
			// The manager's failed-transition path stops the session.
			let _ = stop.trigger_shutdown(SessionShutdownReason::TransitionFailed);
			tokio::time::timeout(std::time::Duration::from_secs(5), stop.wait_shutdown_complete())
				.await
				.unwrap_or_else(|_| panic!("cycle {cycle}: started packet handler was not joined"));
			tokio::net::UdpSocket::bind(("127.0.0.1", port))
				.await
				.unwrap_or_else(|e| panic!("cycle {cycle}: completed stop must release the port: {e}"));
		}
	}

	fn test_authorization(
		client: &str,
	) -> (
		watch::Sender<crate::session::authorization::StreamAuthorization>,
		AuthorizationReceiver,
	) {
		watch::channel(crate::session::authorization::StreamAuthorization::new(1, client.parse().unwrap()).unwrap())
	}

	fn session_ping(authorization: &crate::session::authorization::StreamAuthorization, counter: u32) -> Vec<u8> {
		let mut ping = authorization.ping_payload(MediaStream::Video).as_bytes().to_vec();
		ping.extend(counter.to_be_bytes());
		ping
	}

	async fn send_frame(tx: &mpsc::Sender<VideoPacketMessage>, byte: u8) {
		let mut shard = shard_batch::ShardBuf::new(1, 64, 0);
		shard.shard_mut(0).fill(byte);
		tx.send(VideoPacketMessage::Batch(shard.into_batch())).await.unwrap();
	}

	async fn receives(socket: &tokio::net::UdpSocket, byte: u8) -> bool {
		let mut buf = [0u8; 128];
		match tokio::time::timeout(std::time::Duration::from_millis(200), socket.recv_from(&mut buf)).await {
			Ok(Ok((len, _))) => buf[..len] == [byte; 64],
			_ => false,
		}
	}

	/// Unauthorized hosts, forged/stale payloads and legacy PINGs from session-ID
	/// clients cannot redirect video; the authorized client may change ports.
	#[tokio::test]
	async fn endpoint_discovery_is_bound_to_the_authorized_generation() {
		use tokio::net::UdpSocket;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (authorization_tx, authorization_rx) = test_authorization("127.0.0.1");
		authorization_tx.send_modify(crate::session::authorization::StreamAuthorization::require_session_id);
		spawn_handle_video_packets(
			rx,
			watch::channel(0u64).1,
			socket,
			authorization_rx,
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		start.open();
		let current = authorization_tx.borrow().clone();

		// Another host knowing the payload, and the client with a wrong payload or
		// a legacy PING after announcing session-ID support.
		let attacker = UdpSocket::bind("127.0.0.2:0").await.unwrap();
		attacker.send_to(&session_ping(&current, 1), server).await.unwrap();
		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let mut forged = session_ping(&current, 1);
		forged[3] ^= 0x20;
		client.send_to(&forged, server).await.unwrap();
		client.send_to(b"PING", server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x11).await;
		assert!(!receives(&attacker, 0x11).await);
		assert!(!receives(&client, 0x11).await);

		// The authorized client discovers the endpoint, then moves to a new port.
		client.send_to(&session_ping(&current, 2), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x22).await;
		assert!(receives(&client, 0x22).await);
		let rebound = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		rebound.send_to(&session_ping(&current, 3), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x33).await;
		assert!(receives(&rebound, 0x33).await);
		assert!(!receives(&client, 0x33).await);
		// An attacker cannot steal it back.
		attacker.send_to(&session_ping(&current, 4), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x44).await;
		assert!(receives(&rebound, 0x44).await);
		assert!(!receives(&attacker, 0x44).await);

		// After a resume, the previous generation's payload is stale.
		let next = crate::session::authorization::StreamAuthorization::new(2, "127.0.0.1".parse().unwrap()).unwrap();
		authorization_tx.send_replace(next.clone());
		client.send_to(&session_ping(&current, 5), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x55).await;
		assert!(receives(&rebound, 0x55).await);
		assert!(!receives(&client, 0x55).await);
		client.send_to(&session_ping(&next, 1), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x66).await;
		assert!(receives(&client, 0x66).await);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	}

	#[test]
	fn dialect_changes_require_a_new_stream_epoch() {
		use pyrowave_protocol::PyroWaveDialect;
		let active = VideoStreamContext {
			pyrowave_dialect: Some(PyroWaveDialect::NativeWireV1),
			..Default::default()
		};
		assert!(active.changed_fields(&active).is_empty());
		let records = VideoStreamContext {
			pyrowave_dialect: Some(PyroWaveDialect::RecordFramed),
			..active.clone()
		};
		assert_eq!(active.changed_fields(&records), vec!["PyroWave dialect"]);
		assert_eq!(records.changed_fields(&active), vec!["PyroWave dialect"]);
	}

	#[test]
	fn only_display_properties_change_the_compositor_output_mode() {
		use pyrowave_protocol::PyroWaveDialect;
		let active = valid_context();
		let media_only = [
			VideoStreamContext {
				pyrowave_dialect: Some(PyroWaveDialect::NativeWireV1),
				format: NegotiatedVideoFormat::sdr(
					VideoCodec::PyroWave,
					ChromaFormat::Yuv444,
					BitDepth::Ten,
					ColorRange::Full,
				),
				..active.clone()
			},
			VideoStreamContext {
				format: NegotiatedVideoFormat::sdr(
					VideoCodec::Av1,
					ChromaFormat::Yuv420,
					BitDepth::Ten,
					ColorRange::Full,
				),
				..active.clone()
			},
			VideoStreamContext {
				bitrate: 150_000_000,
				..active.clone()
			},
			VideoStreamContext {
				packet_size: 1024,
				minimum_fec_packets: 4,
				qos: !active.qos,
				encrypt_video: !active.encrypt_video,
				max_reference_frames: 4,
				..active.clone()
			},
		];
		for requested in media_only {
			assert!(!active.changed_fields(&requested).is_empty());
			assert_eq!(requested.output_mode(), active.output_mode(), "{requested:?}");
		}
		let display = [
			VideoStreamContext {
				width: 2560,
				height: 1440,
				..active.clone()
			},
			VideoStreamContext {
				fps: 60,
				..active.clone()
			},
			VideoStreamContext {
				format: NegotiatedVideoFormat::hdr10(VideoCodec::Hevc, ChromaFormat::Yuv420, ColorRange::Limited),
				..active.clone()
			},
		];
		for requested in display {
			assert_ne!(requested.output_mode(), active.output_mode(), "{requested:?}");
		}
	}

	#[test]
	fn stats_logging_defaults_on_and_roundtrips_explicit_choice() {
		let config: VideoStreamConfig = toml::from_str("").unwrap();
		assert!(config.log_stats);
		for enabled in [false, true] {
			let root: crate::config::Config =
				toml::from_str(&format!("[stream.video]\nlog_stats = {enabled}")).unwrap();
			let config = root.stream.video;
			assert_eq!(config.log_stats, enabled);
			let roundtrip: VideoStreamConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
			assert_eq!(roundtrip.log_stats, enabled);
		}
	}

	fn config(max_packet_size: usize) -> VideoStreamConfig {
		VideoStreamConfig {
			max_packet_size,
			..Default::default()
		}
	}

	#[test]
	fn no_cap_honors_requested() {
		assert_eq!(config(0).clamp_packet_size(1392, false), 1392);
	}

	#[test]
	fn smaller_client_request_is_honored() {
		assert_eq!(config(1200).clamp_packet_size(1024, false), 1024);
	}

	#[test]
	fn larger_client_request_is_capped() {
		assert_eq!(config(1200).clamp_packet_size(1392, false), 1200);
	}

	#[test]
	fn encrypted_cap_preserves_the_configured_wire_size() {
		// Moonlight announces 1392 - 32 for an encrypted 1392-byte stream.
		assert_eq!(config(1376).clamp_packet_size(1360, true), 1344);
		assert_eq!(config(1376).clamp_packet_size(1300, true), 1300);
		let wire = |size, encrypted| packetizer::wire_shard_size(size, encrypted).unwrap();
		assert_eq!(wire(config(1376).clamp_packet_size(1360, true), true), 1376 + 16);
		assert_eq!(wire(config(1376).clamp_packet_size(1392, false), false), 1376 + 16);
	}

	fn valid_context() -> VideoStreamContext {
		VideoStreamContext {
			width: 3840,
			height: 2160,
			fps: 120,
			packet_size: 1392,
			bitrate: 900_000_000,
			format: NegotiatedVideoFormat::sdr(
				VideoCodec::Hevc,
				ChromaFormat::Yuv420,
				BitDepth::Eight,
				ColorRange::Limited,
			),
			max_reference_frames: 1,
			..Default::default()
		}
	}

	#[test]
	fn high_end_contexts_validate() {
		for (width, height, fps, bitrate) in [
			(3840, 2160, 120, 650_000_000),
			(3840, 2160, 144, 900_000_000),
			(3840, 2160, 240, 900_000_000),
			(7680, 4320, 60, 900_000_000),
			(2560, 1440, 500, 150_000_000),
		] {
			for encrypt_video in [false, true] {
				let context = VideoStreamContext {
					width,
					height,
					fps,
					bitrate,
					encrypt_video,
					..valid_context()
				};
				assert_eq!(context.validate(), Ok(()), "{width}x{height}@{fps} {bitrate}");
			}
		}
		// PyroWave budgets beyond the conventional encoder's 32-bit range.
		let pyrowave = VideoStreamContext {
			bitrate: u32::MAX as usize + 1,
			format: NegotiatedVideoFormat::sdr(
				VideoCodec::PyroWave,
				ChromaFormat::Yuv420,
				BitDepth::Eight,
				ColorRange::Full,
			),
			..valid_context()
		};
		assert_eq!(pyrowave.validate(), Ok(()));
	}

	#[test]
	fn degenerate_contexts_are_rejected() {
		let cases: [fn(&mut VideoStreamContext); 11] = [
			|c| c.fps = 0,
			|c| c.width = 0,
			|c| c.height = 0,
			|c| c.width = 16_385,
			|c| c.fps = u32::MAX,
			|c| c.bitrate = 0,
			|c| c.bitrate = u32::MAX as usize + 1,
			|c| c.packet_size = 0,
			|c| c.packet_size = 23,
			|c| c.packet_size = gso_socket::MAX_UDP_PAYLOAD,
			|c| c.packet_size = usize::MAX,
		];
		for (index, mutate) in cases.iter().enumerate() {
			let mut context = valid_context();
			mutate(&mut context);
			assert!(context.validate().is_err(), "case {index}: {context:?}");
		}
		// Largest datagram-sized packets: encryption costs exactly its prefix.
		let largest = gso_socket::MAX_UDP_PAYLOAD - 16;
		let plain = VideoStreamContext {
			packet_size: largest,
			..valid_context()
		};
		assert_eq!(plain.validate(), Ok(()));
		let encrypted = VideoStreamContext {
			encrypt_video: true,
			..plain.clone()
		};
		assert!(encrypted.validate().is_err());
		let encrypted = VideoStreamContext {
			packet_size: largest - packetizer::ENC_PREFIX_SIZE,
			..encrypted
		};
		assert_eq!(encrypted.validate(), Ok(()));
		let minimum = VideoStreamContext {
			packet_size: packetizer::MIN_VIDEO_PACKET_SIZE,
			..valid_context()
		};
		assert_eq!(minimum.validate(), Ok(()));
	}

	#[test]
	fn undersized_cap_is_ignored() {
		assert_eq!(config(50).clamp_packet_size(1392, false), 1392);
	}
}
