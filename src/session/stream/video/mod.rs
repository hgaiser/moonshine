use std::sync::{Arc, Mutex};

use async_shutdown::{ShutdownManager, TriggerShutdownToken};
use ffmpeg::{format::Pixel, Frame};
use nvfbc::CudaCapturer;
use tokio::{net::UdpSocket, sync::{broadcast, mpsc, oneshot}};

use crate::{config::Config, ffmpeg::{check_ret, hwdevice::CudaDeviceContextBuilder, hwframe::HwFrameContextBuilder}};

mod capture;
use capture::VideoFrameCapturer;

mod encoder;
use encoder::VideoEncoder;

#[derive(Debug)]
enum VideoStreamCommand {
	Start,
	Stop(oneshot::Sender<()>),
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
	pub async fn new(config: Config, context: VideoStreamContext, stop_signal: ShutdownManager<()>) -> Result<Self, ()> {
		tracing::info!("Starting video stream.");

		let socket = UdpSocket::bind((config.address.as_str(), config.stream.video.port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;

		if context.qos {
			// TODO: Check this value 160, what does it mean exactly?
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
		let inner = VideoStreamInner { context, config, capturer: None, encoder: None };
		tokio::spawn(stop_signal.wrap_cancel(stop_signal.wrap_trigger_shutdown((), inner.run(
			socket,
			command_rx,
			stop_signal.trigger_shutdown_token(()),
		))));

		Ok(Self { command_tx })
	}

	pub async fn start(&self) -> Result<(), ()> {
		tracing::info!("Starting video stream.");

		self.command_tx.send(VideoStreamCommand::Start).await
			.map_err(|e| tracing::warn!("Failed to send Start command: {e}"))
	}

	pub async fn stop(&self) -> Result<(), ()> {
		tracing::info!("Stopping video stream.");
		let (result_tx, result_rx) = oneshot::channel();
		self.command_tx.send(VideoStreamCommand::Stop(result_tx))
			.await
			.map_err(|e| tracing::error!("Failed to send Stop command: {e}"))?;
		result_rx.await
			.map_err(|e| tracing::error!("Failed to wait for result from Stop command: {e}"))
	}

	pub async fn request_idr_frame(&self) -> Result<(), ()> {
		self.command_tx.send(VideoStreamCommand::RequestIdrFrame).await
			.map_err(|e| tracing::warn!("Failed to send RequestIdrFrame command: {e}"))
	}
}

struct VideoStreamInner {
	context: VideoStreamContext,
	config: Config,
	capturer: Option<VideoFrameCapturer>,
	encoder: Option<VideoEncoder>,
}

impl VideoStreamInner {
	async fn run(
		mut self,
		socket: UdpSocket,
		mut command_rx: mpsc::Receiver<VideoStreamCommand>,
		session_stop_token: TriggerShutdownToken<()>,
	) {
		let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(1024);
		tokio::spawn(handle_video_packets(packet_rx, socket));

		let mut started_streaming = false;
		let (idr_frame_request_tx, _idr_frame_request_rx) = tokio::sync::broadcast::channel(1);
		while let Some(command) = command_rx.recv().await {
			match command {
				VideoStreamCommand::RequestIdrFrame => {
					tracing::info!("Received request for IDR frame, next frame will be an IDR frame.");
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
							session_stop_token.clone(),
						).await.is_err() {
						continue;
					}
					started_streaming = true;
				},
				VideoStreamCommand::Stop(result_tx) => {
					if let Some(encoder) = self.encoder.take() {
						let _ = encoder.stop().await;
					}
					if let Some(capturer) = self.capturer.take() {
						let _ = capturer.stop().await;
					}
					let _ = result_tx.send(());
					session_stop_token.forget();
					break;
				},
			}
		}

		tracing::info!("Video stream stopped.");
	}

	async fn start(
		&mut self,
		packet_tx: mpsc::Sender<Vec<u8>>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		session_stop_token: TriggerShutdownToken<()>,
	) -> Result<(), ()> {
		// TODO: Make the GPU index configurable.
		let cuda_device = cudarc::driver::CudaDevice::new(0)
			.map_err(|e| tracing::error!("Failed to initialize CUDA: {e}"))?;

		let capturer = CudaCapturer::new()
			.map_err(|e| tracing::error!("Failed to create NvFBC capture: {e}"))?;
		capturer.release_context()
			.map_err(|e| tracing::error!("Failed to release NvFBC CUDA context: {e}"))?;
		let status = capturer.status()
			.map_err(|e| tracing::error!("Failed to get NvFBC status: {e}"))?;

		if status.screen_size.w != self.context.width || status.screen_size.h != self.context.height {
			// TODO: Resize the CUDA buffer to the requested size?
			tracing::warn!(
				"Client asked for resolution {}x{}, but we are generating a resolution of {}x{}.",
				self.context.width, self.context.height, status.screen_size.w, status.screen_size.h
			);
			self.context.width = status.screen_size.w;
			self.context.height = status.screen_size.h;
		}

		let cuda_device_context = CudaDeviceContextBuilder::new()
			.map_err(|e| tracing::error!("Failed to create CUDA device context: {e}"))?
			.set_cuda_context((*cuda_device.cu_primary_ctx()) as *mut _)
			.build()
			.map_err(|e| tracing::error!("Failed to build CUDA device context: {e}"))?
		;

		let mut hw_frame_context = HwFrameContextBuilder::new(cuda_device_context)
			.map_err(|e| tracing::error!("Failed to create CUDA frame context: {e}"))?
			.set_width(self.context.width)
			.set_height(self.context.height)
			.set_sw_format(Pixel::ZRGB32)
			.set_format(Pixel::CUDA)
			.build()
			.map_err(|e| tracing::error!("Failed to build CUDA frame context: {e}"))?
		;

		let capture_buffer = create_frame(self.context.width, self.context.height, Pixel::CUDA, hw_frame_context.as_raw_mut())?;
		let intermediate_buffer = Arc::new(Mutex::new(create_frame(self.context.width, self.context.height, Pixel::CUDA, hw_frame_context.as_raw_mut())?));
		let encoder_buffer = create_frame(self.context.width, self.context.height, Pixel::CUDA, hw_frame_context.as_raw_mut())?;
		let frame_number = Arc::new(std::sync::atomic::AtomicU32::new(0));
		let frame_notifier = Arc::new(std::sync::Condvar::new());

		let capturer = VideoFrameCapturer::new(
			capturer,
			capture_buffer,
			intermediate_buffer.clone(),
			cuda_device,
			self.context.fps,
			frame_number.clone(),
			frame_notifier.clone(),
			session_stop_token.clone(),
		)?;

		let encoder = VideoEncoder::new(
			encoder_buffer,
			intermediate_buffer,
			hw_frame_context.as_raw_mut(),
			if self.context.video_format == 0 { &self.config.stream.video.codec_h264 } else { &self.config.stream.video.codec_hevc },
			self.context.width, self.context.height,
			self.context.fps,
			self.context.bitrate,
			self.context.packet_size,
			self.context.minimum_fec_packets,
			self.config.stream.video.fec_percentage,
			packet_tx,
			idr_frame_request_rx,
			frame_number,
			frame_notifier,
			session_stop_token,
		)?;

		self.capturer = Some(capturer);
		self.encoder = Some(encoder);

		Ok(())
	}
}

fn create_frame(width: u32, height: u32, pixel_format: Pixel, context: *mut ffmpeg::sys::AVBufferRef) -> Result<Frame, ()> {
	unsafe {
		let mut frame = Frame::empty();
		(*frame.as_mut_ptr()).format = pixel_format as i32;
		(*frame.as_mut_ptr()).width = width as i32;
		(*frame.as_mut_ptr()).height = height as i32;
		(*frame.as_mut_ptr()).hw_frames_ctx = context;

		check_ret(ffmpeg::sys::av_hwframe_get_buffer(context, frame.as_mut_ptr(), 0))
			.map_err(|e| tracing::error!("Failed to create CUDA frame: {e}"))?;
		check_ret(ffmpeg::sys::av_hwframe_get_buffer(context, frame.as_mut_ptr(), 0))
			.map_err(|e| println!("Failed to allocate hardware frame: {e}"))?;
		(*frame.as_mut_ptr()).linesize[0] = (*frame.as_ptr()).width * 4;

		Ok(frame)
	}
}

async fn handle_video_packets(mut packet_rx: mpsc::Receiver<Vec<u8>>, socket: UdpSocket) {
	let mut buf = [0; 1024];
	let mut client_address = None;

	loop {
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

	tracing::info!("Video packet stream stopped.");
}
