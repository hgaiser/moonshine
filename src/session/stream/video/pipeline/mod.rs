//! Video encoding and streaming pipeline.
//!
//! This module handles video encoding with pixelforge
//! and packetization for network transmission.

mod dmabuf;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ash::vk;
use async_shutdown::ShutdownManager;
use tokio::sync::{broadcast, mpsc};

use crate::session::compositor::frame::ExportedFrame;
use crate::session::manager::SessionShutdownReason;

use super::packetizer::Packetizer;
use super::shard_batch::ShardBatch;
use super::{VideoChromaSampling, VideoDynamicRange, VideoFormat};

use dmabuf::{DmaBufImporter, DmaBufPlane};

use pixelforge::{
	Codec, ColorConverter, ColorConverterConfig, EncodeConfig, EncodedPacket, Encoder, InputFormat, OutputFormat,
	PixelFormat, RateControlMode, VideoContext, VideoContextBuilder,
};

pub struct VideoPipeline {}

/// A single frame's latency breakdown for periodic summary reporting.
struct LatencySample {
	channel_wait: std::time::Duration,
	import: std::time::Duration,
	convert: std::time::Duration,
	encode: std::time::Duration,
	packetize: std::time::Duration,
	total: std::time::Duration,
}

/// Log a summary of latency statistics over a batch of samples.
fn log_latency_summary(samples: &[LatencySample]) {
	let n = samples.len();
	if n == 0 {
		return;
	}

	let mut totals: Vec<u64> = samples.iter().map(|s| s.total.as_micros() as u64).collect();
	totals.sort_unstable();

	let mut channel: Vec<u64> = samples.iter().map(|s| s.channel_wait.as_micros() as u64).collect();
	channel.sort_unstable();

	let mut imports: Vec<u64> = samples.iter().map(|s| s.import.as_micros() as u64).collect();
	imports.sort_unstable();

	let mut converts: Vec<u64> = samples.iter().map(|s| s.convert.as_micros() as u64).collect();
	converts.sort_unstable();

	let mut encodes: Vec<u64> = samples.iter().map(|s| s.encode.as_micros() as u64).collect();
	encodes.sort_unstable();

	let mut packetizes: Vec<u64> = samples.iter().map(|s| s.packetize.as_micros() as u64).collect();
	packetizes.sort_unstable();

	let p50 = |v: &[u64]| v[v.len() / 2];
	let p95 = |v: &[u64]| v[(v.len() as f64 * 0.95) as usize];
	let p99 = |v: &[u64]| v[(v.len() as f64 * 0.99) as usize];

	tracing::debug!(
		frames = n,
		total_p50_us = p50(&totals),
		total_p95_us = p95(&totals),
		total_p99_us = p99(&totals),
		channel_p50_us = p50(&channel),
		channel_p95_us = p95(&channel),
		channel_p99_us = p99(&channel),
		import_p50_us = p50(&imports),
		convert_p50_us = p50(&converts),
		convert_p95_us = p95(&converts),
		convert_p99_us = p99(&converts),
		encode_p50_us = p50(&encodes),
		encode_p95_us = p95(&encodes),
		encode_p99_us = p99(&encodes),
		packetize_p50_us = p50(&packetizes),
		"Frame latency summary (μs)"
	);
}

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
		packet_tx: mpsc::Sender<ShardBatch>,
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
		packet_tx: mpsc::Sender<ShardBatch>,
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
		if let Err(e) = self.run_encoding_loop(
			frame_rx,
			context,
			encoder,
			packet_tx,
			idr_frame_request_rx,
			stop_session_manager,
		) {
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
		packet_tx: mpsc::Sender<ShardBatch>,
		mut idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<(), String> {
		// Flag to request an IDR frame.
		let idr_requested = Arc::new(AtomicBool::new(false));
		let idr_requested_clone = idr_requested.clone();

		// Start a thread to listen for IDR requests.
		// Uses blocking_recv() so the thread sleeps until a request arrives.
		std::thread::spawn(move || loop {
			match idr_frame_request_rx.blocking_recv() {
				Ok(_) => {
					tracing::debug!("Received request for IDR frame.");
					idr_requested_clone.store(true, Ordering::SeqCst);
				},
				Err(broadcast::error::RecvError::Lagged(n)) => {
					tracing::debug!("IDR frame channel lagged by {n} messages.");
					idr_requested_clone.store(true, Ordering::SeqCst);
				},
				Err(broadcast::error::RecvError::Closed) => {
					tracing::debug!("IDR frame channel closed.");
					break;
				},
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

		// Whether at least one frame has been encoded (for IDR re-encode).
		let mut has_encoded = false;

		// Rolling latency statistics for periodic summary.
		let mut latency_samples: Vec<LatencySample> = Vec::with_capacity(512);
		let mut last_summary_time = std::time::Instant::now();

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
					// If we have a pending IDR request and have encoded before,
					// re-encode the encoder's input image (still contains
					// the last frame's data after color conversion).
					if pending_idr && has_encoded {
						tracing::debug!("Re-encoding last frame for IDR request (no re-import)");
						let encode_result = encoder.encode(encoder.input_image());
						if let Ok(packets) = encode_result {
							for packet in packets {
								if let Err(()) = self.send_packet(
									&packet,
									&packet_tx,
									&mut packetizer,
									&mut frame_number,
									&mut sequence_number,
								) {
									tracing::warn!("Failed to send IDR re-encode packet");
								}
							}
						}
					}
					if !pending_idr && last_frame_time.elapsed() > std::time::Duration::from_secs(5) {
						tracing::warn!("No frames received for 5 seconds");
						last_frame_time = std::time::Instant::now();
					}
					None
				},
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
					tracing::debug!("Frame channel disconnected - compositor stopped");
					break;
				},
			};

			if let Some(frame) = received_frame {
				let t1_received = std::time::Instant::now();

				tracing::trace!(
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
							tracing::warn!("Failed to create DMA-BUF importer: {e}");
							frame.consumed.store(true, Ordering::Release);
							continue;
						},
					},
				};

				// Build DmaBufPlane array from ExportedFrame planes.
				let mut planes_buf = [DmaBufPlane {
					fd: 0,
					offset: 0,
					stride: 0,
					modifier: 0,
				}; 4];
				let plane_count = frame.planes.len().min(4);
				for (i, p) in frame.planes.iter().take(4).enumerate() {
					planes_buf[i] = DmaBufPlane {
						fd: p.fd,
						offset: p.offset,
						stride: p.stride,
						modifier: frame.modifier,
					};
				}
				let planes = &planes_buf[..plane_count];

				// Import the DMA-BUF (reuses cached VkImage for known buffer indices).
				let (source_image, needs_transition) =
					match importer.import_or_reuse_bgra(frame.buffer_index, frame.width, frame.height, planes) {
						Ok(result) => result,
						Err(e) => {
							tracing::warn!("Failed to import DMA-BUF: {e}");
							frame.consumed.store(true, Ordering::Release);
							continue;
						},
					};

				// First-time imports are in UNDEFINED layout; the converter
				// will handle the transition inside its command buffer.
				// Cached imports were left in GENERAL by the previous convert.
				let src_layout = if needs_transition {
					vk::ImageLayout::UNDEFINED
				} else {
					vk::ImageLayout::GENERAL
				};

				let t2_imported = std::time::Instant::now();

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
								tracing::warn!("Failed to create color converter: {e}");
								continue;
							},
						}
					},
				};

				// Convert to YUV.
				if let Err(e) = converter.convert(source_image, src_layout, encoder.input_image()) {
					tracing::warn!("GPU color conversion failed: {e}");
					frame.consumed.store(true, Ordering::Release);
					continue;
				}

				// The DMA-BUF content has been read into the encoder's input
				// image — signal the compositor that this GBM buffer is free.
				frame.consumed.store(true, Ordering::Release);

				let t3_converted = std::time::Instant::now();

				// Encode the converted image.
				let encode_result = encoder.encode(encoder.input_image());

				let t4_encoded = std::time::Instant::now();

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
								tracing::warn!("Failed to send encoded packet");
								return Err("Failed to send packet".to_string());
							}
						}
					},
					Err(e) => {
						tracing::warn!("Failed to encode frame: {e}");
					},
				}

				let t5_packetized = std::time::Instant::now();

				// Log per-frame latency breakdown.
				let channel_wait = t1_received.duration_since(frame.created_at);
				let import_dur = t2_imported.duration_since(t1_received);
				let convert_dur = t3_converted.duration_since(t2_imported);
				let encode_dur = t4_encoded.duration_since(t3_converted);
				let packetize_dur = t5_packetized.duration_since(t4_encoded);
				let total = t5_packetized.duration_since(frame.created_at);

				tracing::debug!(
					channel_wait_us = channel_wait.as_micros() as u64,
					import_us = import_dur.as_micros() as u64,
					convert_us = convert_dur.as_micros() as u64,
					encode_us = encode_dur.as_micros() as u64,
					packetize_us = packetize_dur.as_micros() as u64,
					total_us = total.as_micros() as u64,
					"Frame latency breakdown"
				);

				// Warn on spike frames (total > 4ms) so they stand out in logs.
				if total.as_micros() > 4000 {
					tracing::warn!(
						total_us = total.as_micros() as u64,
						channel_us = channel_wait.as_micros() as u64,
						import_us = import_dur.as_micros() as u64,
						convert_us = convert_dur.as_micros() as u64,
						encode_us = encode_dur.as_micros() as u64,
						packetize_us = packetize_dur.as_micros() as u64,
						buffer_index = frame.buffer_index,
						"SPIKE: frame latency exceeds 4ms"
					);
				}

				latency_samples.push(LatencySample {
					channel_wait,
					import: import_dur,
					convert: convert_dur,
					encode: encode_dur,
					packetize: packetize_dur,
					total,
				});

				// Periodic summary every 5 seconds.
				if last_summary_time.elapsed() >= std::time::Duration::from_secs(5) && !latency_samples.is_empty() {
					log_latency_summary(&latency_samples);
					latency_samples.clear();
					last_summary_time = std::time::Instant::now();
				}

				if !pending_idr {
					last_frame_time = std::time::Instant::now();
				}

				has_encoded = true;
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
		packet_tx: &mpsc::Sender<ShardBatch>,
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

		let shards = packetizer.packetize(
			&packet.data,
			packet.is_key_frame,
			self.packet_size,
			self.minimum_fec_packets,
			self.fec_percentage,
			*frame_number,
			sequence_number,
			rtp_timestamp,
		)?;

		if packet_tx.blocking_send(shards).is_err() {
			tracing::debug!("Couldn't send packet batch, video packet channel closed.");
			return Err(());
		}

		Ok(())
	}
}
