//! Video encoding and streaming pipeline.
//!
//! This module handles video capture from PipeWire nodes, encoding with pixelforge,
//! and packetization for network transmission.

mod capture;
mod dmabuf;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use tokio::sync::{broadcast, mpsc};

use crate::session::manager::SessionShutdownReason;

use super::packetizer::Packetizer;
use super::{VideoChromaSampling, VideoDynamicRange, VideoFormat};

use capture::{start_capture, CaptureConfig, CapturePixelFormat};
use dmabuf::{DmaBufImporter, DmaBufPlane};

use pixelforge::{
	Codec, ColorConverter, ColorConverterConfig, EncodeConfig, EncodedPacket, Encoder, InputFormat, OutputFormat,
	PixelFormat, RateControlMode, VideoContext, VideoContextBuilder,
};

pub struct VideoPipeline {}

impl VideoPipeline {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		node_id: u32,
		width: u32,
		height: u32,
		framerate: u32,
		bitrate: usize,
		packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		video_format: VideoFormat,
		dynamic_range: VideoDynamicRange,
		chroma_sampling: VideoChromaSampling,
		max_reference_frames: u32,
		packet_tx: mpsc::Sender<Vec<u8>>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video pipeline.");

		let inner = VideoPipelineInner {
			node_id,
			width,
			height,
			framerate,
			bitrate,
			packet_size,
			minimum_fec_packets,
			fec_percentage,
			video_format,
			dynamic_range,
			chroma_sampling,
			max_reference_frames,
		};

		std::thread::Builder::new()
			.name("video-pipeline".to_string())
			.spawn(move || {
				inner.run(packet_tx, idr_frame_request_rx, stop_session_manager);
			})
			.map_err(|e| tracing::error!("Failed to start video pipeline thread: {e}"))?;

		Ok(Self {})
	}
}

struct VideoPipelineInner {
	node_id: u32,
	width: u32,
	height: u32,
	framerate: u32,
	bitrate: usize,
	packet_size: usize,
	minimum_fec_packets: u32,
	fec_percentage: u8,
	video_format: VideoFormat,
	dynamic_range: VideoDynamicRange,
	chroma_sampling: VideoChromaSampling,
	max_reference_frames: u32,
}

impl VideoPipelineInner {
	pub fn run(
		self,
		packet_tx: mpsc::Sender<Vec<u8>>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		tracing::debug!("Starting video pipeline.");

		// Trigger session shutdown if we exit unexpectedly.
		let _session_stop_token =
			stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoEncoderStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		// Create the encoder.
		let (context, encoder) = match self.create_encoder() {
			Ok(result) => result,
			Err(e) => {
				tracing::error!("Failed to create video encoder: {e}");
				return;
			},
		};

		// Start the capture and encoding loop.
		if let Err(e) = self.run_encoding_loop(context, encoder, packet_tx, idr_frame_request_rx, stop_session_manager)
		{
			tracing::error!("Video encoding loop failed: {e}");
		}

		tracing::debug!("Video pipeline stopped.");
	}

	fn create_encoder(&self) -> Result<(VideoContext, Encoder), String> {
		// Create Vulkan video context.
		let context = VideoContextBuilder::new()
			.build()
			.map_err(|e| format!("Failed to create video context: {e}"))?;

		// Convert our video format to pixelforge's codec.
		let codec = match self.video_format {
			VideoFormat::H264 => Codec::H264,
			VideoFormat::Hevc => Codec::H265,
			VideoFormat::Av1 => {
				// PIXELFORGE_TODO: AV1 encoding is not yet implemented in pixelforge.
				return Err("AV1 encoding not yet supported".to_string());
			},
		};

		// Convert pixel format.
		let pixel_format = match self.chroma_sampling {
			VideoChromaSampling::Yuv420 => PixelFormat::Yuv420,
			VideoChromaSampling::Yuv444 => PixelFormat::Yuv444,
		};

		// Convert bit depth based on dynamic range.
		let bit_depth = match self.dynamic_range {
			VideoDynamicRange::Sdr => pixelforge::EncodeBitDepth::Eight,
			VideoDynamicRange::Hdr => pixelforge::EncodeBitDepth::Ten,
		};

		// Create encode configuration.
		let config = match codec {
			Codec::H264 => EncodeConfig::h264(self.width, self.height),
			Codec::H265 => EncodeConfig::h265(self.width, self.height),
			Codec::AV1 => {
				return Err("AV1 not supported".to_string());
			},
		}
		.with_pixel_format(pixel_format)
		.with_bit_depth(bit_depth)
		.with_rate_control(RateControlMode::Cbr)
		.with_target_bitrate(self.bitrate as u32)
		.with_frame_rate(self.framerate, 1)
		.with_gop_size(0) // Infinite GOP, we'll request IDR frames manually
		.with_b_frames(0) // No B-frames for low latency
		.with_max_reference_frames(self.max_reference_frames);

		let encoder = Encoder::new(context.clone(), config).map_err(|e| format!("Failed to create encoder: {e}"))?;

		Ok((context, encoder))
	}

	fn run_encoding_loop(
		&self,
		context: VideoContext,
		mut encoder: Encoder,
		packet_tx: mpsc::Sender<Vec<u8>>,
		mut idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<(), String> {
		// Flag to request an IDR frame.
		let idr_requested = Arc::new(AtomicBool::new(false));
		let idr_requested_clone = idr_requested.clone();

		// Start a thread to listen for IDR requests.
		let stop_clone = stop_session_manager.clone();
		std::thread::spawn(move || {
			while !stop_clone.is_shutdown_triggered() {
				match idr_frame_request_rx.try_recv() {
					Ok(_) => {
						tracing::debug!("Received request for IDR frame.");
						idr_requested_clone.store(true, Ordering::SeqCst);
					},
					Err(broadcast::error::TryRecvError::Empty) => {
						std::thread::sleep(std::time::Duration::from_millis(1));
					},
					Err(broadcast::error::TryRecvError::Lagged(_)) => {},
					Err(broadcast::error::TryRecvError::Closed) => {
						tracing::debug!("IDR frame channel closed.");
						break;
					},
				}
			}
		});

		let mut packetizer = Packetizer::new();
		let mut sequence_number = 0u32;
		let mut frame_number = 0u32;

		// Determine output YUV format based on chroma sampling and dynamic range.
		let output_format = match (self.chroma_sampling, self.dynamic_range) {
			(VideoChromaSampling::Yuv420, VideoDynamicRange::Sdr) => OutputFormat::NV12,
			(VideoChromaSampling::Yuv420, VideoDynamicRange::Hdr) => OutputFormat::P010,
			(VideoChromaSampling::Yuv444, VideoDynamicRange::Sdr) => OutputFormat::YUV444,
			(VideoChromaSampling::Yuv444, VideoDynamicRange::Hdr) => OutputFormat::YUV444P10,
		};

		// Color converter will be initialized on first RGB frame.
		let mut color_converter: Option<ColorConverter> = None;

		// Start the capture.
		let capture_config = CaptureConfig {
			node_id: self.node_id,
			width: self.width,
			height: self.height,
		};

		let capture = start_capture(capture_config, stop_session_manager.clone())?;

		// Encoding loop - receives frames from capture thread.
		let frame_interval = std::time::Duration::from_secs_f64(1.0 / self.framerate as f64);
		let mut last_frame_time = std::time::Instant::now();

		// DMA-BUF importer for zero-copy capture (initialized on first DMA-BUF frame)
		let mut dmabuf_importer: Option<DmaBufImporter> = None;

		while !stop_session_manager.is_shutdown_triggered() {
			// Check for IDR request.
			if idr_requested.swap(false, Ordering::SeqCst) {
				encoder.request_idr();
				tracing::debug!("IDR frame requested");
			}

			// Try to receive a frame from capture thread (with timeout)
			match capture.recv_timeout(frame_interval) {
				Ok(frame) => {
					// Zero-copy DMA-BUF path - import directly to Vulkan image.
					let dmabuf_info = &frame.dmabuf;
					tracing::debug!(
						"Received frame: format={:?}, {}x{}, fd={}, offset={}, stride={}",
						frame.format,
						dmabuf_info.width,
						dmabuf_info.height,
						dmabuf_info.fd,
						dmabuf_info.offset,
						dmabuf_info.stride
					);
					let importer = match &mut dmabuf_importer {
						Some(imp) => imp,
						None => match DmaBufImporter::new(context.clone()) {
							Ok(imp) => {
								dmabuf_importer = Some(imp);
								dmabuf_importer.as_mut().unwrap()
							},
							Err(e) => {
								tracing::error!("Failed to create DMA-BUF importer: {e}");
								continue;
							},
						},
					};

					// Create DmaBufPlane from the info.
					let plane = DmaBufPlane {
						fd: dmabuf_info.fd,
						offset: dmabuf_info.offset,
						stride: dmabuf_info.stride,
						modifier: dmabuf_info.modifier,
					};
					let planes = [plane];

					// Handle based on format.
					let encode_result = match frame.format {
						CapturePixelFormat::NV12 => {
							// NV12 DMA-BUF can be passed directly to encoder (zero-copy)
							let dmabuf_image =
								match importer.import_nv12(dmabuf_info.width, dmabuf_info.height, &planes) {
									Ok(img) => img,
									Err(e) => {
										tracing::error!("Failed to import NV12 DMA-BUF: {e}");
										continue;
									},
								};
							encoder.encode(dmabuf_image.image())
						},
						CapturePixelFormat::BGRx
						| CapturePixelFormat::BGRA
						| CapturePixelFormat::RGBx
						| CapturePixelFormat::RGBA => {
							// RGB DMA-BUF needs conversion to YUV.
							let input_format = match frame.format {
								CapturePixelFormat::BGRx => InputFormat::BGRx,
								CapturePixelFormat::RGBx => InputFormat::RGBx,
								CapturePixelFormat::BGRA => InputFormat::BGRA,
								CapturePixelFormat::RGBA => InputFormat::RGBA,
								_ => unreachable!(),
							};

							// Import the DMA-BUF as a Vulkan image.
							let dmabuf_image = match frame.format {
								CapturePixelFormat::BGRx | CapturePixelFormat::BGRA => {
									importer.import_bgra(dmabuf_info.width, dmabuf_info.height, &planes)
								},
								CapturePixelFormat::RGBx | CapturePixelFormat::RGBA => {
									importer.import_rgba(dmabuf_info.width, dmabuf_info.height, &planes)
								},
								_ => unreachable!(),
							};

							let dmabuf_image = match dmabuf_image {
								Ok(img) => img,
								Err(e) => {
									tracing::error!("Failed to import RGB DMA-BUF: {e}");
									continue;
								},
							};

							// Initialize converter if needed.
							let converter = match &mut color_converter {
								Some(conv) => conv,
								None => {
									let config = ColorConverterConfig {
										width: self.width,
										height: self.height,
										input_format,
										output_format,
									};
									match ColorConverter::new(context.clone(), config) {
										Ok(conv) => {
											color_converter = Some(conv);
											color_converter.as_mut().unwrap()
										},
										Err(e) => {
											tracing::error!("Failed to create color converter: {e}");
											continue;
										},
									}
								},
							};

							// Convert RGB to YUV directly into the encoder's input image.
							if let Err(e) = converter.convert(dmabuf_image.image(), encoder.input_image()) {
								tracing::error!("GPU color conversion failed: {e}");
								continue;
							}

							// Encode the converted image.
							encoder.encode(encoder.input_image())
						},
						CapturePixelFormat::I420 => {
							// I420 DMA-BUF - import and encode.
							// Note: Encoder might need I420 support
							tracing::warn!("I420 DMA-BUF not yet supported");
							continue;
						},
					};

					// Process encoded packets.
					match encode_result {
						Ok(packets) => {
							for packet in packets {
								if let Err(()) = self.send_packet(
									&packet,
									&packet_tx,
									&mut packetizer,
									&mut frame_number,
									&mut sequence_number,
								) {
									tracing::error!("Failed to send encoded packet");
									return Err("Failed to send packet".to_string());
								}
							}
						},
						Err(e) => {
							tracing::error!("Failed to encode frame: {e}");
						},
					}

					last_frame_time = std::time::Instant::now();
				},
				Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
					// No frame received within timeout - check if we should warn.
					if last_frame_time.elapsed() > std::time::Duration::from_secs(5) {
						tracing::warn!("No frames received for 5 seconds");
						last_frame_time = std::time::Instant::now(); // Reset to avoid spam
					}
				},
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
					tracing::debug!("Frame channel disconnected - capture thread ended");
					break;
				},
			}
		}

		// Wait for capture thread to finish (handled by CaptureHandle Drop)
		drop(capture);

		// Flush the encoder.
		match encoder.flush() {
			Ok(packets) => {
				for packet in packets {
					let _ = self.send_packet(
						&packet,
						&packet_tx,
						&mut packetizer,
						&mut frame_number,
						&mut sequence_number,
					);
				}
			},
			Err(e) => {
				tracing::warn!("Failed to flush encoder: {e}");
			},
		}

		Ok(())
	}

	fn send_packet(
		&self,
		packet: &EncodedPacket,
		packet_tx: &mpsc::Sender<Vec<u8>>,
		packetizer: &mut Packetizer,
		frame_number: &mut u32,
		sequence_number: &mut u32,
	) -> Result<(), ()> {
		// Calculate RTP timestamp from PTS (convert to 90kHz clock)
		let rtp_timestamp = (packet.pts * 90000 / self.framerate as u64) as u32;

		tracing::trace!(
			"Sending packet: size={}, keyframe={}, pts={}",
			packet.data.len(),
			packet.is_key_frame,
			packet.pts
		);

		*frame_number += 1;

		packetizer.packetize(
			&packet.data,
			packet.is_key_frame,
			packet_tx,
			self.packet_size,
			self.minimum_fec_packets,
			self.fec_percentage,
			*frame_number,
			sequence_number,
			rtp_timestamp,
		)
	}
}
