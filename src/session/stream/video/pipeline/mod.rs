//! Video encoding and streaming pipeline.
//!
//! This module handles video encoding with pixelforge
//! and packetization for network transmission.

mod dmabuf;
mod hdr_sei;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ash::vk;
use async_shutdown::ShutdownManager;
use tokio::sync::{broadcast, mpsc, watch};

use crate::session::compositor::frame::{ExportedFrame, FrameColorSpace, HdrModeState};
use crate::session::manager::SessionShutdownReason;
use crate::telemetry::{FrameAttrs, PipelineLatency, PipelineMetrics, TraceMode};
use opentelemetry::global as otel_global;

use super::packetizer::Packetizer;
use super::shard_batch::ShardBatch;
use super::{VideoChromaSampling, VideoDynamicRange, VideoFormat};

use dmabuf::{DmaBufImporter, DmaBufPlane};

use pixelforge::{
	Codec, ColorConverter, ColorConverterConfig, ColorDescription, ColorSpace, EncodeConfig, EncodedPacket, Encoder,
	InputFormat, OutputFormat, PixelFormat, RateControlMode, VideoContext, VideoContextBuilder,
};

/// Label string for OTel metric attributes. Keeps cardinality low (one
/// of three known values) so collectors don't have to deal with arbitrary
/// strings.
fn codec_label(format: VideoFormat) -> &'static str {
	match format {
		VideoFormat::H264 => "h264",
		VideoFormat::Hevc => "hevc",
		VideoFormat::Av1 => "av1",
	}
}

/// Map a DRM fourcc format code to the corresponding pixelforge InputFormat
/// and Vulkan import format.
fn drm_fourcc_to_input(fourcc: u32) -> (InputFormat, vk::Format) {
	// DRM fourcc values (from drm_fourcc.h):
	// ARGB8888 = 0x34325241, XRGB8888 = 0x34325258
	// ABGR8888 = 0x34324241, XBGR8888 = 0x34324258
	// ABGR2101010 = 0x30334241
	// ABGR16161616F = 0x48344241
	match fourcc {
		0x34324241 | 0x34324258 => (InputFormat::RGBA, vk::Format::R8G8B8A8_UNORM), // ABGR/XBGR8888
		0x30334241 => (InputFormat::ABGR2101010, vk::Format::A2B10G10R10_UNORM_PACK32), // ABGR2101010
		0x48344241 => (InputFormat::RGBA16F, vk::Format::R16G16B16A16_SFLOAT),      // ABGR16161616F
		_ => (InputFormat::BGRx, vk::Format::B8G8R8A8_UNORM),                       // ARGB/XRGB8888 (fallback)
	}
}

pub struct VideoPipeline {}

/// A single frame's latency breakdown for periodic summary reporting.
#[derive(Clone)]
pub(crate) struct LatencySample {
	pub channel_wait: std::time::Duration,
	pub import: std::time::Duration,
	pub convert: std::time::Duration,
	pub encode: std::time::Duration,
	pub packetize: std::time::Duration,
	pub send: std::time::Duration,
	pub total: std::time::Duration,
	pub encoded_bytes: usize,
	pub is_key_frame: bool,
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

	let mut sends: Vec<u64> = samples.iter().map(|s| s.send.as_micros() as u64).collect();
	sends.sort_unstable();

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
		send_p50_us = p50(&sends),
		send_p95_us = p95(&sends),
		send_p99_us = p99(&sends),
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
		encryption_key: Option<Vec<u8>>,
		packet_tx: mpsc::Sender<ShardBatch>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
		log_frame_spikes: bool,
		stats_tx: Option<std::sync::mpsc::Sender<LatencySample>>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video pipeline.");

		// Resolve metrics instruments from the global meter. When telemetry
		// init didn't run with an OTLP endpoint, the global meter is a
		// no-op provider and instrument creation is essentially free —
		// recording into them just drops on the floor.
		let metrics = Some(Arc::new(PipelineMetrics::new(&otel_global::meter(
			"moonshine.pipeline",
		))));
		let trace_mode = crate::telemetry::trace_mode();

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
			encryption_key,
			log_frame_spikes,
			stats_tx,
			metrics,
			trace_mode,
		};

		std::thread::Builder::new()
			.name("video-pipeline".to_string())
			.spawn(move || {
				inner.run(
					frame_rx,
					packet_tx,
					idr_frame_request_rx,
					stop_session_manager,
					hdr_metadata_tx,
				);
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
	encryption_key: Option<Vec<u8>>,
	log_frame_spikes: bool,
	/// OTel metrics, resolved from the global meter at construction.
	/// `None` is the path that runs in tests / when no meter is registered.
	metrics: Option<Arc<PipelineMetrics>>,
	/// Snapshot of `crate::telemetry::TraceMode` for the running session.
	/// Read fresh from the global telemetry config at construction so we
	/// don't pay an Arc deref per frame.
	trace_mode: TraceMode,
	/// Optional sink for per-frame latency samples. Used by the bench harness;
	/// `None` in normal sessions to avoid extra work on the hot path.
	stats_tx: Option<std::sync::mpsc::Sender<LatencySample>>,
}

impl VideoPipelineInner {
	pub fn run(
		self,
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
		packet_tx: mpsc::Sender<ShardBatch>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
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
			hdr_metadata_tx,
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
			VideoFormat::Av1 => Codec::AV1,
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

		// Select color description for VUI signaling.
		let color_description = match self.dynamic_range {
			VideoDynamicRange::Sdr => ColorDescription::bt709(),
			VideoDynamicRange::Hdr => ColorDescription::bt2020_pq(),
		};

		// Create encode configuration.
		let config = match codec {
			Codec::H264 => EncodeConfig::h264(self.width, self.height),
			Codec::H265 => EncodeConfig::h265(self.width, self.height),
			Codec::AV1 => EncodeConfig::av1(self.width, self.height),
		}
		.with_pixel_format(pixel_format)
		.with_bit_depth(bit_depth)
		.with_color_description(color_description)
		.with_rate_control(RateControlMode::Cbr)
		.with_target_bitrate(self.bitrate as u32)
		.with_frame_rate(self.framerate, 1)
		.with_gop_size(0) // Infinite GOP, we'll request IDR frames manually
		.with_b_frames(0) // No B-frames for low latency
		.with_max_reference_frames(self.max_reference_frames)
		.with_virtual_buffer_size_ms(1000 / self.framerate)
		.with_initial_virtual_buffer_size_ms(0);

		let encoder = Encoder::new(context.clone(), config).map_err(|e| format!("Failed to create encoder: {e}"))?;

		Ok((context, encoder))
	}

	#[allow(clippy::too_many_arguments)]
	fn run_encoding_loop(
		&self,
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
		context: VideoContext,
		mut encoder: Encoder,
		packet_tx: mpsc::Sender<ShardBatch>,
		mut idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
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

		let mut packetizer = Packetizer::new(self.encryption_key.as_deref())
			.map_err(|()| "Failed to create packetizer: invalid encryption key length".to_string())?;
		packetizer.warm_up(self.fec_percentage, self.minimum_fec_packets);
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

		// Pre-built attribute set for the metrics hot path. Codec + hdr
		// don't change across a session, so we build the KeyValue arrays
		// once and borrow them on every frame. `None` when telemetry is
		// disabled — zero overhead when the global meter is the no-op.
		let frame_attrs = self.metrics.as_ref().map(|_| {
			FrameAttrs::new(
				codec_label(self.video_format),
				self.dynamic_range == VideoDynamicRange::Hdr,
			)
		});

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

		// Track the last HDR mode state sent to the control stream.
		let mut last_hdr_state = HdrModeState {
			enabled: self.dynamic_range == VideoDynamicRange::Hdr,
			metadata: None,
		};

		// Track the encoder's current VUI color description, initialized
		// to match what the encoder was created with. When the required
		// color_desc differs, we call set_color_description() to update
		// the SPS/sequence header.
		let mut encoder_color_desc: Option<ColorDescription> = Some(match self.dynamic_range {
			VideoDynamicRange::Sdr => ColorDescription::bt709(),
			VideoDynamicRange::Hdr => ColorDescription::bt2020_pq(),
		});

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
								// Use current time as frame_created_at for re-encoded IDR (no actual frame)
								if let Err(()) = self.send_packet(
									&packet,
									&packet_tx,
									&mut packetizer,
									&mut frame_number,
									&mut sequence_number,
									std::time::Instant::now(),
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
					"Received frame: format=0x{:08X}, modifier={:#x}, {}x{}, planes={}",
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

				// Determine Vulkan format and input format from the frame's DRM fourcc.
				let (frame_input_format, import_vk_format) = drm_fourcc_to_input(frame.format);

				// Import the DMA-BUF (reuses cached VkImage for known buffer indices).
				let (source_image, needs_transition) = match importer.import_or_reuse(
					frame.buffer_index,
					frame.width,
					frame.height,
					import_vk_format,
					planes,
				) {
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

				// Recreate the converter if the input format changed (e.g. GBM pool
				// ABGR2101010 → direct scanout XBGR8888). The converter's image view
				// format must match the source image format.
				if let Some(ref conv) = color_converter {
					if conv.config().input_format != frame_input_format {
						tracing::info!(
							"Input format changed from {:?} to {:?}, recreating color converter",
							conv.config().input_format,
							frame_input_format,
						);
						color_converter = None;
					}
				}

				// Initialize converter if needed.
				let converter = match &mut color_converter {
					Some(conv) => conv,
					None => {
						let (color_space, full_range) = match self.dynamic_range {
							VideoDynamicRange::Sdr => (ColorSpace::Bt709, true),
							VideoDynamicRange::Hdr => (ColorSpace::Bt2020, false),
						};
						let mut config =
							ColorConverterConfig::new(self.width, self.height, frame_input_format, output_format);
						config.color_space = color_space;
						config.full_range = full_range;
						match ColorConverter::new(context.clone(), config) {
							Ok(conv) => {
								color_converter = Some(conv);
								color_converter.as_mut().unwrap()
							},
							Err(e) => {
								tracing::warn!("Failed to create color converter: {e}");
								frame.consumed.store(true, Ordering::Release);
								continue;
							},
						}
					},
				};

				// In HDR mode, select per-frame color space and encoder VUI
				// based on the frame's actual color space. SDR frames are
				// encoded as BT.709 and HDR frames as BT.2020+PQ, with
				// dynamic VUI switching in the encoder.
				if self.dynamic_range == VideoDynamicRange::Hdr {
					let frame_cs = frame.color_space;
					let (cs, full_range, color_desc) = match frame_cs {
						FrameColorSpace::Srgb => (ColorSpace::Bt709, true, ColorDescription::bt709()),
						FrameColorSpace::Bt2020Pq => (ColorSpace::Bt2020, false, ColorDescription::bt2020_pq()),
					};

					// Switch encoder VUI first. Only update the converter if
					// the encoder switch succeeds, so that the converter's
					// color space stays in sync with the encoder's VUI.
					if encoder_color_desc != Some(color_desc) {
						tracing::info!(
							"Switching encoder color description to {color_desc:?} (frame_cs: {frame_cs:?})"
						);
						match encoder.set_color_description(color_desc) {
							Ok(()) => {
								encoder_color_desc = Some(color_desc);
								converter.set_color_space(cs);
								converter.set_full_range(full_range);
							},
							Err(e) => return Err(format!("Failed to update encoder color description: {e}")),
						}
					} else {
						converter.set_color_space(cs);
						converter.set_full_range(full_range);
					}
				}

				// Convert to YUV.
				if let Err(e) = converter.convert(source_image, src_layout, encoder.input_image()) {
					tracing::warn!("GPU color conversion failed: {e}");
					frame.consumed.store(true, Ordering::Release);
					continue;
				}

				// The DMA-BUF content has been read into the encoder's input
				// image — signal the compositor that this GBM buffer is free.
				frame.consumed.store(true, Ordering::Release);

				// Forward HDR mode state changes to the control stream.
				// In HDR sessions, `enabled` reflects whether the current
				// frame is encoded as BT.2020+PQ (true) or BT.709 (false).
				if self.dynamic_range == VideoDynamicRange::Hdr {
					let hdr_enabled = encoder_color_desc == Some(ColorDescription::bt2020_pq());
					let new_state = HdrModeState {
						enabled: hdr_enabled,
						metadata: frame.hdr_metadata,
					};
					if new_state != last_hdr_state {
						tracing::info!(
							"HDR mode state changed: enabled={}, metadata={}",
							new_state.enabled,
							if new_state.metadata.is_some() {
								"present"
							} else {
								"none"
							}
						);
						last_hdr_state = new_state.clone();
						let _ = hdr_metadata_tx.send(new_state);
					}
				}

				let t3_converted = std::time::Instant::now();

				// Encode the converted image.
				let encode_result = encoder.encode(encoder.input_image());

				let t4_encoded = std::time::Instant::now();

				// Process encoded packets.
				let mut packetize_dur = std::time::Duration::ZERO;
				let mut send_dur = std::time::Duration::ZERO;
				let mut encoded_bytes = 0usize;
				let mut is_key_frame = false;
				match encode_result {
					Ok(packets) => {
						for mut packet in packets {
							// Inject HDR metadata into the bitstream on key frames,
							// but only when encoding as BT.2020+PQ (HDR frames).
							// SDR frames encoded as BT.709 should not carry HDR SEI.
							if packet.is_key_frame && encoder_color_desc == Some(ColorDescription::bt2020_pq()) {
								if let Some(ref m) = last_hdr_state.metadata {
									packet.data = hdr_sei::inject_hdr_metadata(&packet.data, m, self.video_format);
								}
							}

							encoded_bytes += packet.data.len();
							is_key_frame |= packet.is_key_frame;

							let (p, s) = match self.send_packet(
								&packet,
								&packet_tx,
								&mut packetizer,
								&mut frame_number,
								&mut sequence_number,
								frame.created_at,
							) {
								Ok(durations) => durations,
								Err(()) => {
									tracing::warn!("Failed to send encoded packet");
									return Err("Failed to send packet".to_string());
								},
							};
							packetize_dur += p;
							send_dur += s;
						}
					},
					Err(e) => {
						tracing::warn!("Failed to encode frame: {e}");
					},
				}

				let t5_done = std::time::Instant::now();

				// Log per-frame latency breakdown.
				let channel_wait = t1_received.duration_since(frame.created_at);
				let import_dur = t2_imported.duration_since(t1_received);
				let convert_dur = t3_converted.duration_since(t2_imported);
				let encode_dur = t4_encoded.duration_since(t3_converted);
				let total = t5_done.duration_since(frame.created_at);

				tracing::debug!(
					channel_wait_us = channel_wait.as_micros() as u64,
					import_us = import_dur.as_micros() as u64,
					convert_us = convert_dur.as_micros() as u64,
					encode_us = encode_dur.as_micros() as u64,
					packetize_us = packetize_dur.as_micros() as u64,
					send_us = send_dur.as_micros() as u64,
					total_us = total.as_micros() as u64,
					"Frame latency breakdown"
				);

				// Warn on spike frames (total > frame interval) so they stand out in logs.
				let frame_interval_us = 1_000_000 / self.framerate as u128;
				if self.log_frame_spikes && total.as_micros() > frame_interval_us {
					tracing::warn!(
						total_us = total.as_micros() as u64,
						channel_us = channel_wait.as_micros() as u64,
						import_us = import_dur.as_micros() as u64,
						convert_us = convert_dur.as_micros() as u64,
						encode_us = encode_dur.as_micros() as u64,
						packetize_us = packetize_dur.as_micros() as u64,
						send_us = send_dur.as_micros() as u64,
						encoded_bytes,
						is_key_frame,
						buffer_index = frame.buffer_index,
						"SPIKE: frame latency exceeds {}us",
						frame_interval_us
					);
				}

				let sample = LatencySample {
					channel_wait,
					import: import_dur,
					convert: convert_dur,
					encode: encode_dur,
					packetize: packetize_dur,
					send: send_dur,
					total,
					encoded_bytes,
					is_key_frame,
				};

				// Record into OTel metrics (counters/histograms/gauges).
				// Cheap by design — pre-built FrameAttrs, lock-free
				// instruments, no allocations.
				let total_us = total.as_micros() as u64;
				let frame_budget_us = frame_interval_us as u64;
				let pipeline_lat = PipelineLatency {
					channel_wait_us: channel_wait.as_micros() as u64,
					import_us: import_dur.as_micros() as u64,
					convert_us: convert_dur.as_micros() as u64,
					encode_us: encode_dur.as_micros() as u64,
					packetize_us: packetize_dur.as_micros() as u64,
					send_us: send_dur.as_micros() as u64,
					total_us,
					encoded_bytes,
					frame_budget_us,
				};
				if let (Some(metrics), Some(attrs)) = (&self.metrics, &frame_attrs) {
					metrics.record_frame(attrs, &pipeline_lat);
				}

				// Trace emission decision based on the configured mode:
				//   None      — no span at all
				//   Static(r) — emit on `r` fraction of frames, decided
				//                client-side so we don't pay span-creation
				//                cost on the rejected frames. Hash-based
				//                so the keep set is deterministic per
				//                buffer_index — easier to reason about
				//                than a stateful counter.
				//   Outliers  — span only when the frame spiked
				let is_spike = total_us > frame_budget_us;
				let emit_span = match self.trace_mode {
					TraceMode::None => false,
					TraceMode::Static(r) if r >= 1.0 => true,
					TraceMode::Static(r) if r <= 0.0 => false,
					TraceMode::Static(r) => {
						// Cheap deterministic 1-bit decision based on
						// buffer_index × frame_count. No allocations,
						// no global state.
						let h = (frame.buffer_index as u64).wrapping_mul(0x9E3779B97F4A7C15);
						(h as f64 / u64::MAX as f64) < r
					},
					TraceMode::Outliers => is_spike,
				};
				if emit_span {
					let span = tracing::info_span!(
						"frame.encode",
						codec = codec_label(self.video_format),
						hdr = self.dynamic_range == VideoDynamicRange::Hdr,
						buffer_index = frame.buffer_index,
						channel_wait_us = pipeline_lat.channel_wait_us,
						import_us = pipeline_lat.import_us,
						convert_us = pipeline_lat.convert_us,
						encode_us = pipeline_lat.encode_us,
						packetize_us = pipeline_lat.packetize_us,
						send_us = pipeline_lat.send_us,
						total_us,
						encoded_bytes,
						is_key_frame,
						spike = is_spike,
					);
					// Enter+exit immediately; we don't have nested work
					// to wrap, only metadata to ship.
					let _g = span.enter();
				}

				if let Some(tx) = self.stats_tx.as_ref() {
					// Bench-mode sink. If the receiver is gone we just stop trying.
					let _ = tx.send(sample.clone());
				}

				latency_samples.push(sample);

				// Periodic summary every 5 seconds.
				if last_summary_time.elapsed() >= std::time::Duration::from_secs(5) && !latency_samples.is_empty() {
					log_latency_summary(&latency_samples);
					latency_samples.clear();
					last_summary_time = std::time::Instant::now();

					// Sample the dmabuf cache size into a gauge once per
					// summary tick. Lets dashboards alert on unbounded
					// growth (the leak that fix-dmabuf-leak addresses).
					if let (Some(metrics), Some(importer)) = (&self.metrics, &dmabuf_importer) {
						metrics.dmabuf_cache_size.record(importer.cache_len() as u64, &[]);
					}
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
					// Use current time as frame_created_at for flush packets (no actual frame)
					let _ = self.send_packet(
						&packet,
						&packet_tx,
						&mut packetizer,
						&mut frame_number,
						&mut sequence_number,
						std::time::Instant::now(),
					);
				}
			},
			Err(e) => {
				tracing::warn!("Failed to flush encoder: {e}");
			},
		}

		Ok(())
	}

	/// Returns `(packetize_duration, send_duration)` on success.
	fn send_packet(
		&self,
		packet: &EncodedPacket,
		packet_tx: &mpsc::Sender<ShardBatch>,
		packetizer: &mut Packetizer,
		frame_number: &mut u32,
		sequence_number: &mut u32,
		frame_created_at: std::time::Instant,
	) -> Result<(std::time::Duration, std::time::Duration), ()> {
		// Calculate RTP timestamp from PTS (convert to 90kHz clock)
		let rtp_timestamp = (packet.pts * 90000 / self.framerate as u64) as u32;

		tracing::trace!(
			"Sending packet: size={}, keyframe={}, pts={}",
			packet.data.len(),
			packet.is_key_frame,
			packet.pts
		);

		*frame_number += 1;

		let t_start = std::time::Instant::now();

		// Calculate frame processing latency (capture to packetization) in 100µs units (1/10 ms)
		// Clamp to u16::MAX to prevent overflow (~6.55 seconds max)
		let processing_latency = t_start.duration_since(frame_created_at);
		let latency_100us = std::cmp::min((processing_latency.as_micros() / 100) as u16, u16::MAX);

		let shards = packetizer.packetize(
			&packet.data,
			packet.is_key_frame,
			self.packet_size,
			self.minimum_fec_packets,
			self.fec_percentage,
			*frame_number,
			sequence_number,
			rtp_timestamp,
			latency_100us,
		)?;

		let t_packetized = std::time::Instant::now();

		if packet_tx.blocking_send(shards).is_err() {
			tracing::debug!("Couldn't send packet batch, video packet channel closed.");
			return Err(());
		}

		let t_sent = std::time::Instant::now();

		Ok((t_packetized - t_start, t_sent - t_packetized))
	}
}
