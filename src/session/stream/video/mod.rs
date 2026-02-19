use async_shutdown::ShutdownManager;
use tokio::{
	net::UdpSocket,
	sync::{broadcast, mpsc},
};

use crate::{config::Config, session::manager::SessionShutdownReason};
use crate::session::compositor::frame::ExportedFrame;

mod packetizer;
mod pipeline;
use pipeline::VideoPipeline;

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

#[derive(Debug)]
enum VideoStreamCommand {
	Start,
	RequestIdrFrame,
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
}

#[derive(Clone)]
pub struct VideoStream {
	command_tx: mpsc::Sender<VideoStreamCommand>,
}

impl VideoStream {
	pub async fn new(
		config: Config,
		context: VideoStreamContext,
		frame_rx: Option<std::sync::mpsc::Receiver<ExportedFrame>>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video stream.");

		let socket = UdpSocket::bind((config.address.as_str(), config.stream.video.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// 160 corresponds to DSCP CS5 (Video)
			tracing::debug!("Enabling QoS on video socket.");
			socket
				.set_tos(160)
				.map_err(|e| tracing::warn!("Failed to set QoS on the video socket: {e}"))?;
		}

		tracing::debug!(
			"Listening for video messages on {}",
			socket
				.local_addr()
				.map_err(|e| tracing::warn!("Failed to get local address associated with control socket: {e}"))?
		);

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = VideoStreamInner {
			context,
			config,
			pipeline: None,
			frame_rx,
		};
		tokio::spawn(inner.run(socket, command_rx, stop_session_manager.clone()));

		Ok(Self { command_tx })
	}

	pub async fn start(&self) -> Result<(), ()> {
		tracing::debug!("Starting video stream.");

		self.command_tx
			.send(VideoStreamCommand::Start)
			.await
			.map_err(|e| tracing::warn!("Failed to send Start command: {e}"))
	}

	pub async fn request_idr_frame(&self) -> Result<(), ()> {
		self.command_tx
			.send(VideoStreamCommand::RequestIdrFrame)
			.await
			.map_err(|e| tracing::warn!("Failed to send RequestIdrFrame command: {e}"))
	}
}

struct VideoStreamInner {
	context: VideoStreamContext,
	config: Config,
	pipeline: Option<VideoPipeline>,
	frame_rx: Option<std::sync::mpsc::Receiver<ExportedFrame>>,
}

impl VideoStreamInner {
	async fn run(
		mut self,
		socket: UdpSocket,
		mut command_rx: mpsc::Receiver<VideoStreamCommand>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		// Trigger session shutdown if we exit unexpectedly.
		let _session_stop_token =
			stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoStreamStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		let (packet_tx, packet_rx) = mpsc::channel::<Vec<Vec<u8>>>(128);
		tokio::spawn(handle_video_packets(packet_rx, socket, stop_session_manager.clone()));

		let mut started_streaming = false;
		let (idr_frame_request_tx, _idr_frame_request_rx) = tokio::sync::broadcast::channel(1);
		while let Ok(Some(command)) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
			match command {
				VideoStreamCommand::RequestIdrFrame => {
					tracing::debug!("Received request for IDR frame, next frame will be an IDR frame.");
					let _ = idr_frame_request_tx
						.send(())
						.map_err(|e| tracing::warn!("Failed to send IDR frame request to encoder: {e}"));
				},
				VideoStreamCommand::Start => {
					if started_streaming {
						tracing::warn!("Can't start streaming twice.");
						continue;
					}

					if self
						.start(
							packet_tx.clone(),
							idr_frame_request_tx.subscribe(),
							stop_session_manager.clone(),
						)
						.await
						.is_err()
					{
						break;
					}
					started_streaming = true;
				},
			}
		}

		tracing::debug!("Video stream stopped.");
	}

	async fn start(
		&mut self,
		packet_tx: mpsc::Sender<Vec<Vec<u8>>>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<(), ()> {
		let frame_rx = self.frame_rx.take().ok_or_else(|| {
			tracing::warn!("No frame receiver available for video pipeline");
		})?;

		tracing::debug!("Creating video pipeline with compositor frame receiver.");
		let pipeline = VideoPipeline::new(
			frame_rx,
			self.context.width,
			self.context.height,
			self.context.fps,
			self.context.bitrate,
			self.context.packet_size,
			self.context.minimum_fec_packets,
			self.config.stream.video.fec_percentage,
			self.context.video_format,
			self.context.dynamic_range,
			self.context.chroma_sampling_type,
			self.context.max_reference_frames,
			packet_tx,
			idr_frame_request_rx,
			stop_session_manager.clone(),
		)?;

		self.pipeline = Some(pipeline);

		Ok(())
	}
}

async fn handle_video_packets(
	mut packet_rx: mpsc::Receiver<Vec<Vec<u8>>>,
	socket: UdpSocket,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
) {
	let mut buf = [0; 1024];
	let mut client_address = None;

	// Trigger session shutdown if we exit unexpectedly.
	let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoPacketHandlerStopped);
	let _delay_stop = stop_session_manager.delay_shutdown_token();

	while !stop_session_manager.is_shutdown_triggered() {
		tokio::select! {
			batch = packet_rx.recv() => {
				match batch {
					Some(shards) => {
						if let Some(client_address) = client_address {
							for shard in &shards {
								if let Err(e) = socket.send_to(shard.as_slice(), client_address).await {
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
}
