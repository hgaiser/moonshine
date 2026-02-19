//! Video encoding and streaming pipeline.
//!
//! This module handles video encoding with pixelforge
//! and packetization for network transmission.

mod dmabuf;

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_shutdown::ShutdownManager;
use tokio::sync::{broadcast, mpsc};

use crate::session::compositor::frame::ExportedFrame;
use crate::session::manager::SessionShutdownReason;

use super::packetizer::Packetizer;
use super::{VideoChromaSampling, VideoDynamicRange, VideoFormat};

use dmabuf::{DmaBufImporter, DmaBufPlane};

use pixelforge::{
	Codec, ColorConverter, ColorConverterConfig, EncodeConfig, EncodedPacket, Encoder, InputFormat, OutputFormat,
	PixelFormat, RateControlMode, VideoContext, VideoContextBuilder,
};

pub struct VideoPipeline {}

impl VideoPipeline {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
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
				inner.run(frame_rx, packet_tx, idr_frame_request_rx, stop_session_manager);
			})
			.map_err(|e| tracing::error!("Failed to start video pipeline thread: {e}"))?;

		Ok(Self {})
	}
}

struct VideoPipelineInner {
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
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
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
		if let Err(e) = self.run_encoding_loop(frame_rx, context, encoder, packet_tx, idr_frame_request_rx, stop_session_manager)
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
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
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

		// Color converter will be initialized on first frame.
		let mut color_converter: Option<ColorConverter> = None;

		// Encoding loop - receives frames from compositor.
		let frame_interval = std::time::Duration::from_secs_f64(1.0 / self.framerate as f64);
		let mut last_frame_time = std::time::Instant::now();

		// DMA-BUF importer for zero-copy capture (initialized on first DMA-BUF frame).
		let mut dmabuf_importer: Option<DmaBufImporter> = None;

		// Cache for the last received frame to handle IDR requests during static screen.
		let mut last_frame: Option<ExportedFrame> = None;

		while !stop_session_manager.is_shutdown_triggered() {
			// Check for IDR request.
			let mut pending_idr = false;
			if idr_requested.swap(false, Ordering::SeqCst) {
				encoder.request_idr();
				tracing::debug!("IDR frame requested");
				pending_idr = true;
			}

			// Try to receive a frame from compositor (with timeout).
			let received_frame = match frame_rx.recv_timeout(frame_interval) {
				Ok(frame) => Some(frame),
				Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
					// No frame received within timeout.
					// If we have a pending IDR request and a cached frame, re-encode it.
					if pending_idr {
						if let Some(frame) = &last_frame {
							tracing::debug!("Re-encoding last frame for IDR request");
							Some(frame.clone())
						} else {
							None
						}
					} else {
						if last_frame_time.elapsed() > std::time::Duration::from_secs(5) {
							tracing::warn!("No frames received for 5 seconds");
							last_frame_time = std::time::Instant::now(); // Reset to avoid spam
						}
						None
					}
				},
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
					tracing::debug!("Frame channel disconnected - compositor stopped");
					break;
				},
			};

			if let Some(frame) = received_frame {
				// Update cache.
				last_frame = Some(frame.clone());

				tracing::debug!(
					"Received frame: format={}, modifier={:#x}, {}x{}, planes={}",
					frame.format,
					frame.modifier,
					frame.width,
					frame.height,
					frame.planes.len()
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

				// Build DmaBufPlane array from ExportedFrame planes.
				let planes: Vec<DmaBufPlane> = frame
					.planes
					.iter()
					.map(|p| DmaBufPlane {
						fd: p.fd.as_raw_fd(),
						offset: p.offset,
						stride: p.stride,
						modifier: frame.modifier,
					})
					.collect();

				// The compositor exports ARGB/XRGB buffers — import as BGRA and convert.
				let dmabuf_image = match importer.import_bgra(frame.width, frame.height, &planes) {
					Ok(img) => img,
					Err(e) => {
						tracing::error!("Failed to import DMA-BUF: {e}");
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
							input_format: InputFormat::BGRx,
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

				// Convert to YUV.
				if let Err(e) = converter.convert(dmabuf_image.image(), encoder.input_image()) {
					tracing::error!("GPU color conversion failed: {e}");
					continue;
				}

				// Encode the converted image.
				let encode_result = encoder.encode(encoder.input_image());

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

				if !pending_idr {
					last_frame_time = std::time::Instant::now();
				}
			}
		}

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
