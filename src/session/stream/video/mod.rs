use ashpd::desktop::{
	screencast::{CursorMode, Screencast, SourceType},
	PersistMode,
	Session,
};
use async_shutdown::ShutdownManager;
use tokio::{net::UdpSocket, sync::{broadcast, mpsc}};

use crate::{config::Config, session::manager::SessionShutdownReason, state::State};

mod packetizer;
mod pipeline;
use pipeline::VideoPipeline;

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
	pub video_format: u32,
}

#[derive(Clone)]
pub struct VideoStream {
	command_tx: mpsc::Sender<VideoStreamCommand>
}

impl VideoStream {
	pub async fn new(
		config: Config,
		state: State,
		context: VideoStreamContext,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video stream.");

		let socket = UdpSocket::bind((config.address.as_str(), config.stream.video.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// 160 corresponds to DSCP CS5 (Video)
			tracing::debug!("Enabling QoS on video socket.");
			socket.set_tos(160)
				.map_err(|e| tracing::error!("Failed to set QoS on the video socket: {e}"))?;
		}

		tracing::debug!(
			"Listening for video messages on {}",
			socket.local_addr()
				.map_err(|e| tracing::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = VideoStreamInner { context, config, state, pipeline: None, screencast: None, session: None };
		tokio::spawn(inner.run(
			socket,
			command_rx,
			stop_session_manager.clone(),
		));

		Ok(Self { command_tx })
	}

	pub async fn start(&self) -> Result<(), ()> {
		tracing::debug!("Starting video stream.");

		self.command_tx.send(VideoStreamCommand::Start).await
			.map_err(|e| tracing::warn!("Failed to send Start command: {e}"))
	}

	pub async fn request_idr_frame(&self) -> Result<(), ()> {
		self.command_tx.send(VideoStreamCommand::RequestIdrFrame).await
			.map_err(|e| tracing::warn!("Failed to send RequestIdrFrame command: {e}"))
	}
}

struct VideoStreamInner {
	context: VideoStreamContext,
	config: Config,
	state: State,
	pipeline: Option<VideoPipeline>,
	screencast: Option<Screencast<'static>>,
	session: Option<Session<'static, Screencast<'static>>>,
}

impl VideoStreamInner {
	async fn run(
		mut self,
		socket: UdpSocket,
		mut command_rx: mpsc::Receiver<VideoStreamCommand>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		// Trigger session shutdown if we exit unexpectedly.
		let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoStreamStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(1024);
		tokio::spawn(handle_video_packets(packet_rx, socket, stop_session_manager.clone()));

		let mut started_streaming = false;
		let (idr_frame_request_tx, _idr_frame_request_rx) = tokio::sync::broadcast::channel(1);
		while let Ok(Some(command)) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
			match command {
				VideoStreamCommand::RequestIdrFrame => {
					tracing::debug!("Received request for IDR frame, next frame will be an IDR frame.");
					let _ = idr_frame_request_tx.send(())
						.map_err(|e| tracing::error!("Failed to send IDR frame request to encoder: {e}"));
				},
				VideoStreamCommand::Start => {
					if started_streaming {
						tracing::warn!("Can't start streaming twice.");
						continue;
					}

					if self.start(
						packet_tx.clone(),
						idr_frame_request_tx.subscribe(),
						stop_session_manager.clone(),
					).await.is_err() {
						break;
					}
					started_streaming = true;
				},
			}
		}

		tracing::debug!("Video stream stopped.");

		if let Some(session) = self.session {
			let _ = session.close().await;
		}
	}

	async fn start(
		&mut self,
		packet_tx: mpsc::Sender<Vec<u8>>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<(), ()> {
		tracing::debug!("Creating Screencast proxy.");
		let proxy = Screencast::new().await
			.map_err(|e| tracing::error!("Failed to create Screencast proxy: {e}"))?;
		tracing::debug!("Creating Screencast session.");
		let session = proxy.create_session().await
			.map_err(|e| tracing::error!("Failed to create Screencast session: {e}"))?;

		let restore_token = self.state.get_screencast_token().await
			.map_err(|_| tracing::error!("Failed to get screencast token."))?;

		tracing::debug!("Selecting sources.");
		proxy.select_sources(
			&session,
			CursorMode::Embedded,
			SourceType::Monitor.into(),
			false,
			restore_token.as_deref(),
			PersistMode::ExplicitlyRevoked,
		).await
			.map_err(|e| tracing::error!("Failed to select sources: {e}"))?;

		tracing::debug!("Starting session (waiting for user input).");
		let response = proxy.start(&session, None).await
			.map_err(|e| tracing::error!("Failed to start session: {e}"))?
			.response()
			.map_err(|e| tracing::error!("Failed to get response: {e}"))?;
		tracing::debug!("Session started.");

		if let Some(token) = response.restore_token() {
			self.state.set_screencast_token(token.to_string()).await
				.map_err(|_| tracing::error!("Failed to save screencast token."))?;
		}

		let stream = response.streams().first()
			.ok_or_else(|| tracing::error!("No streams selected"))?;
		let node_id = stream.pipe_wire_node_id();

		tracing::debug!("Creating pipeline with node_id: {}", node_id);
		let pipeline = VideoPipeline::new(
			node_id,
			self.context.width,
			self.context.height,
			self.context.fps,
			self.context.bitrate,
			self.context.packet_size,
			self.context.minimum_fec_packets,
			self.config.stream.video.fec_percentage,
			self.context.video_format,
			packet_tx,
			idr_frame_request_rx,
			stop_session_manager.clone(),
		)?;

		self.pipeline = Some(pipeline);
		self.screencast = Some(proxy);
		self.session = Some(session);

		Ok(())
	}
}

async fn handle_video_packets(
	mut packet_rx: mpsc::Receiver<Vec<u8>>,
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
			packet = packet_rx.recv() => {
				match packet {
					Some(packet) => {
						if let Some(client_address) = client_address {
							if let Err(e) = socket.send_to(packet.as_slice(), client_address).await {
								tracing::warn!("Failed to send packet to client: {e}");
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
