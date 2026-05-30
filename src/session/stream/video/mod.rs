use std::sync::Arc;

use async_shutdown::ShutdownManager;
use tokio::{
	net::UdpSocket,
	sync::{broadcast, mpsc, watch, Notify},
};

use crate::config::Config;
use crate::session::compositor::frame::{ExportedFrame, HdrModeState};
use crate::session::manager::SessionShutdownReason;
use crate::session::SessionKeysReceiver;

mod packetizer;
mod pipeline;
mod shard_batch;
use pipeline::VideoPipeline;
use shard_batch::ShardBatch;

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
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub packet_size: usize,
	pub bitrate: usize,
	pub minimum_fec_packets: u32,
	pub qos: bool,
	pub video_format: VideoFormat,
	pub dynamic_range: VideoDynamicRange,
	pub chroma_sampling_type: VideoChromaSampling,
	pub max_reference_frames: u32,
	/// Whether the client has enabled video encryption.
	pub encrypt_video: bool,
	/// FEC percentage for video packets (from config).
	pub fec_percentage: u8,
	/// Whether to log per-frame latency spikes.
	pub log_frame_spikes: bool,
}

/// Handle returned by `VideoStream::start` that gates the pipeline and packet handler.
///
/// The pipeline and packet handler are spawned immediately but block on a `Notify`
/// until `trigger()` is called on `StartB`.
pub struct VideoStreamHandle {
	notify: Arc<Notify>,
	idr_tx: broadcast::Sender<()>,
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
}

pub struct VideoStream {
	socket: UdpSocket,
	frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
	hdr_metadata_tx: watch::Sender<HdrModeState>,
}

impl VideoStream {
	pub async fn new(
		config: Config,
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
		_stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video stream.");

		let socket = UdpSocket::bind((config.address.as_str(), config.stream.video.port))
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
		})
	}

	#[allow(clippy::too_many_arguments)]
	pub fn start(
		self,
		context: VideoStreamContext,
		keys_rx: SessionKeysReceiver,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<VideoStreamHandle, ()> {
		let Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
		} = self;

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = socket.set_tos_v4(160);
		}

		// Gate for pipeline + packet handler.
		let start_notify = Arc::new(Notify::new());

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<ShardBatch>(128);

		// Spawn packet handler — gated behind start_notify.
		spawn_handle_video_packets(packet_rx, socket, start_notify.clone(), stop.clone());

		// Spawn pipeline thread — gated behind start_notify.
		VideoPipeline::new(
			frame_rx,
			context,
			keys_rx,
			packet_tx,
			idr_tx.subscribe(),
			stop.clone(),
			hdr_metadata_tx,
			start_notify.clone(),
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			notify: start_notify,
			idr_tx,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<ShardBatch>,
	socket: UdpSocket,
	start: Arc<Notify>,
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
					} else {
						tracing::warn!("Received unknown message on video stream of length {len}.");
					}
				},
			}
		}

		tracing::debug!("Video packet stream stopped.");
	});
}
