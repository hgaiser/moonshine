use std::sync::{Arc, Mutex};

use ffmpeg::{FrameBuilder, Frame};
use tokio::{net::UdpSocket, sync::mpsc};

use crate::config::Config;

mod capture;
use capture::FrameCapturer;

mod encoder;
use encoder::Encoder;

#[derive(Debug)]
pub(super) enum VideoCommand {
	StartStreaming,
	RequestIdrFrame,
}

#[derive(Clone, Default)]
pub struct VideoStreamContext {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub packet_size: usize,
	pub bitrate: u64,
	pub minimum_fec_packets: u32,
}

fn create_frame(width: u32, height: u32, pixel_format: i32) -> Result<Frame, ()> {
	let mut frame_builder = FrameBuilder::new()
		.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
	frame_builder
		.set_format(pixel_format)
		.set_width(width)
		.set_height(height);
	let frame = frame_builder.allocate(0)
		.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

	frame.make_writable()
		.map_err(|e| log::error!("Failed to make frame writable: {e}"))?;

	Ok(frame)
}

pub(super) async fn run_video_stream(
	config: Config,
	context: VideoStreamContext,
	mut video_command_rx: mpsc::Receiver<VideoCommand>,
) -> Result<(), ()> {
	let socket = UdpSocket::bind((config.address, config.stream.video.port)).await
		.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

	log::info!(
		"Listening for video messages on {}",
		socket.local_addr()
			.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
	);

	let mut started_streaming = false;
	let mut buf = [0; 1024];
	let mut client_address = None;
	let (idr_frame_request_tx, _idr_frame_request_rx) = tokio::sync::broadcast::channel(1);
	let (packet_tx, mut packet_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
	loop {
		tokio::select! {
			client_recv_result = socket.recv_from(&mut buf) => {
				match client_recv_result {
					Ok((len, addr)) => {
						if &buf[..len] == b"PING" {
							log::debug!("Received video stream PING message from {addr}.");
							client_address = Some(addr);
						} else {
							log::warn!("Received unknown message on video stream of length {len}.");
						}
					},
					Err(ref e) => {
						if e.kind() != std::io::ErrorKind::WouldBlock {
							log::error!("Failed to receive UDP message: {e}");
							return Err(());
						}
					}
				}
			},

			packets_recv_result = packet_rx.recv() => {
				match packets_recv_result {
					Some(packet) => {
						if let Some(client_address) = client_address {
							socket.send_to(packet.as_slice(), client_address).await
								.map_err(|e| log::error!("Failed to send packet to client: {e}"))?;
						}
					},
					None => {
						log::error!("Failed to receive packets from encoder, channel closed.");
						return Err(());
					},
				}
			},

			video_command_recv_result = video_command_rx.recv() => {
				match video_command_recv_result {
					Some(command) => {
						match command {
							VideoCommand::RequestIdrFrame => {
								log::info!("Received request for IDR frame, next frame will be an IDR frame.");
								idr_frame_request_tx.send(())
									.map_err(|e| log::error!("Failed to send IDR frame request to encoder: {e}"))?;
							},
							VideoCommand::StartStreaming => {
								if started_streaming {
									panic!("Can't start streaming twice.");
								}

								let capture_buffer = create_frame(context.width, context.height, ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)?;
								let intermediate_buffer = Arc::new(Mutex::new(create_frame(context.width, context.height, ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)?));
								let encoder_buffer = create_frame(context.width, context.height, ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)?;
								let notifier = Arc::new(tokio::sync::Notify::new());

								let capturer = FrameCapturer::new(context.width, context.height)?;

								let encoder = Encoder::new(
									&config.stream.video.codec,
									context.width, context.height,
									context.fps,
									context.bitrate,
								)?;

								tokio::spawn(capturer.run(
									context.fps,
									capture_buffer,
									intermediate_buffer.clone(),
									notifier.clone(),
								));

								tokio::spawn(encoder.run(
									packet_tx.clone(),
									idr_frame_request_tx.subscribe(),
									context.packet_size,
									context.minimum_fec_packets,
									config.stream.video.fec_percentage,
									encoder_buffer,
									intermediate_buffer,
									notifier.clone(),
								));

								started_streaming = true;
							},
						}
					},
					None => {
						log::error!("Failed to receive video stream command, channel closed.");
						return Err(());
					}
				}
			},
		}
	}
}