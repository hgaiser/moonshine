use std::sync::{Arc, Mutex};

use async_shutdown::ShutdownManager;
use ffmpeg::{FrameBuilder, Frame, HwFrameContext};
use tokio::{net::UdpSocket, sync::mpsc::{self, Sender}};

use crate::config::Config;

mod capture;
use capture::FrameCapturer;

mod encoder;
use encoder::Encoder;

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
	pub bitrate: u64,
	pub minimum_fec_packets: u32,
	pub qos: bool,
	pub video_format: u32,
}

#[derive(Clone)]
pub struct VideoStream {
	command_tx: Sender<VideoStreamCommand>
}

struct VideoStreamInner {
}

impl VideoStream {
	pub fn new(config: Config, context: VideoStreamContext, stop_signal: ShutdownManager<()>) -> Self {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = VideoStreamInner { };
		tokio::spawn(stop_signal.wrap_cancel(stop_signal.wrap_trigger_shutdown((), inner.run(
			config,
			context,
			command_rx,
			stop_signal.clone()
		))));

		Self { command_tx }
	}

	pub async fn start(&self) -> Result<(), ()> {
		self.command_tx.send(VideoStreamCommand::Start).await
			.map_err(|e| log::warn!("Failed to send Start command: {e}"))
	}

	pub async fn request_idr_frame(&self) -> Result<(), ()> {
		self.command_tx.send(VideoStreamCommand::RequestIdrFrame).await
			.map_err(|e| log::warn!("Failed to send RequestIdrFrame command: {e}"))
	}
}

impl VideoStreamInner {
	async fn run(
		self,
		config: Config,
		mut context: VideoStreamContext,
		mut command_rx: mpsc::Receiver<VideoStreamCommand>,
		stop_signal: ShutdownManager<()>,
	) -> Result<(), ()> {
		let socket = UdpSocket::bind((config.address, config.stream.video.port))
			.await
			.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// TODO: Check this value 160, what does it mean exactly?
			log::debug!("Enabling QoS on video socket.");
			socket.set_tos(160)
				.map_err(|e| log::error!("Failed to set QoS on the video socket: {e}"))?;
		}

		log::debug!(
			"Listening for video messages on {}",
			socket.local_addr()
				.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let (packet_tx, mut packet_rx) = mpsc::channel::<Vec<u8>>(1024);
		tokio::spawn(async move {
			let mut buf = [0; 1024];
			let mut client_address = None;

			loop {
				tokio::select! {
					packet = packet_rx.recv() => {
						match packet {
							Some(packet) => {
								if let Some(client_address) = client_address {
									if let Err(e) = socket.send_to(packet.as_slice(), client_address).await {
										log::warn!("Failed to send packet to client: {e}");
									}
								}
							},
							None => {
								log::debug!("Packet channel closed.");
								break;
							},
						}
					},

					message = socket.recv_from(&mut buf) => {
						let (len, address) = match message {
							Ok((len, address)) => (len, address),
							Err(e) => {
								log::warn!("Failed to receive message: {e}");
								break;
							},
						};

						if &buf[..len] == b"PING" {
							log::trace!("Received video stream PING message from {address}.");
							client_address = Some(address);
						} else {
							log::warn!("Received unknown message on video stream of length {len}.");
						}
					},
				}
			}

			log::debug!("Stopping video stream.");
		});

		let mut started_streaming = false;
		let (idr_frame_request_tx, _idr_frame_request_rx) = tokio::sync::broadcast::channel(1);
		while let Some(command) = command_rx.recv().await {
			match command {
				VideoStreamCommand::RequestIdrFrame => {
					log::info!("Received request for IDR frame, next frame will be an IDR frame.");
					idr_frame_request_tx.send(())
						.map_err(|e| log::error!("Failed to send IDR frame request to encoder: {e}"))?;
				},
				VideoStreamCommand::Start => {
					if started_streaming {
						log::warn!("Can't start streaming twice.");
						continue;
					}

					// TODO: Make the GPU index configurable.
					let cuda_device = cudarc::driver::CudaDevice::new(0)
						.map_err(|e| log::error!("Failed to initialize CUDA: {e}"))?;

					let capturer = FrameCapturer::new()?;
					let status = capturer.status()?;
					if status.screen_size.w != context.width || status.screen_size.h != context.height {
						// TODO: Resize the CUDA buffer to the requested size?
						log::warn!(
							"Client asked for resolution {}x{}, but we are generating a resolution of {}x{}.",
							context.width, context.height, status.screen_size.w, status.screen_size.h
						);
						context.width = status.screen_size.w;
						context.height = status.screen_size.h;
					}

					let mut encoder = Encoder::new(
						&cuda_device,
						if context.video_format == 0 { &config.stream.video.codec_h264 } else { &config.stream.video.codec_hevc },
						context.width, context.height,
						context.fps,
						context.bitrate,
					)?;

					let capture_buffer = create_frame(context.width, context.height, ffmpeg::sys::AVPixelFormat_AV_PIX_FMT_CUDA, &mut encoder.hw_frame_context)?;
					let intermediate_buffer = Arc::new(Mutex::new(create_frame(context.width, context.height, ffmpeg::sys::AVPixelFormat_AV_PIX_FMT_CUDA, &mut encoder.hw_frame_context)?));
					let encoder_buffer = create_frame(context.width, context.height, ffmpeg::sys::AVPixelFormat_AV_PIX_FMT_CUDA, &mut encoder.hw_frame_context)?;
					let notifier = Arc::new(std::sync::Condvar::new());

					let capture_thread = std::thread::Builder::new().name("video-capture".to_string()).spawn({
						let intermediate_buffer = intermediate_buffer.clone();
						let notifier = notifier.clone();
						let context = context.clone();
						let stop_signal = stop_signal.clone();
						move || {
							cuda_device.bind_to_thread()
								.map_err(|e| log::error!("Failed to bind CUDA device to thread: {e}"))?;
							capturer.run(
								context.fps,
								capture_buffer,
								intermediate_buffer,
								notifier,
								stop_signal,
							)
						}
					});
					if let Err(e) = capture_thread {
						log::error!("Failed to start video capture thread: {e}");
						continue;
					}

					let encode_thread = std::thread::Builder::new().name("video-encode".to_string()).spawn({
						let packet_tx = packet_tx.clone();
						let notifier = notifier.clone();
						let idr_frame_request_rx = idr_frame_request_tx.subscribe();
						let context = context.clone();
						let stop_signal = stop_signal.clone();
						move || {
							encoder.run(
								packet_tx,
								idr_frame_request_rx,
								context.packet_size,
								context.minimum_fec_packets,
								config.stream.video.fec_percentage,
								encoder_buffer,
								intermediate_buffer,
								notifier,
								stop_signal,
							)
						}
					});
					if let Err(e) = encode_thread {
						log::error!("Failed to video encoding thread: {e}");
						continue;
					}

					started_streaming = true;
				},
			}
		}

		log::debug!("Command channel closed.");
		Ok(())
	}
}

fn create_frame(width: u32, height: u32, pixel_format: i32, context: &mut HwFrameContext) -> Result<Frame, ()> {
	let mut frame_builder = FrameBuilder::new()
		.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
	frame_builder
		.set_format(pixel_format)
		.set_width(width)
		.set_height(height)
		.set_hw_frames_ctx(context);
	let mut frame = frame_builder.allocate_hwframe()
		.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

	// frame.make_writable()
	// 	.map_err(|e| log::error!("Failed to make frame writable: {e}"))?;

	unsafe {
		ffmpeg::check_ret(ffmpeg::sys::av_hwframe_get_buffer(frame.as_raw_mut().hw_frames_ctx, frame.as_raw_mut(), 0))
			.map_err(|e| println!("Failed to allocate hardware frame: {e}"))?;
		frame.as_raw_mut().linesize[0] = frame.as_raw().width * 4
	}

	Ok(frame)
}
