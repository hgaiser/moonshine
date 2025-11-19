use async_shutdown::ShutdownManager;
use gst::prelude::*;
use gst_app::AppSink;
use tokio::sync::{broadcast, mpsc};

use crate::session::manager::SessionShutdownReason;

use super::packetizer::Packetizer;

#[derive(Debug, Clone, Copy)]
pub enum VideoFormat {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoDynamicRange {
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

pub struct VideoPipeline { }

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
		};

		std::thread::Builder::new().name("video-pipeline".to_string()).spawn(
			move || {
				inner.run(
					packet_tx,
					idr_frame_request_rx,
					stop_session_manager,
				);
			}
		)
			.map_err(|e| tracing::error!("Failed to start video pipeline thread: {e}"))?;

		Ok(Self { })
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
}

impl VideoPipelineInner {
	pub fn run(
		self,
		packet_tx: mpsc::Sender<Vec<u8>>,
		mut idr_frame_request_rx: broadcast::Receiver<()>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		tracing::debug!("Starting video pipeline.");

		// Trigger session shutdown if we exit unexpectedly.
		let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoEncoderStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		if let Err(e) = gst::init() {
			tracing::error!("Failed to initialize GStreamer: {e}");
			return;
		}

		// Convert bitrate to kbit/sec for nvh264enc
		let bitrate_kbit = self.bitrate / 1000;

		let (encoder, parser, caps_filter) = match (self.video_format, self.dynamic_range) {
			(VideoFormat::H264, _) => ("nvh264enc", "h264parse config-interval=-1", "video/x-h264,stream-format=byte-stream,profile=high"),
			(VideoFormat::Hevc, VideoDynamicRange::Sdr) => ("nvh265enc", "h265parse config-interval=-1", "video/x-h265,stream-format=byte-stream"),
			(VideoFormat::Hevc, VideoDynamicRange::Hdr) => ("nvh265enc", "h265parse config-interval=-1", "video/x-h265,stream-format=byte-stream,profile=main-10"),
			(VideoFormat::Av1, _) => ("nvav1enc", "av1parse", "video/x-av1,profile=main"),
		};

		let format = if self.dynamic_range == VideoDynamicRange::Hdr { "P010_10LE" } else { "NV12" };

		// TODO: Make encoder configurable (nvh264enc, vaapih264enc, x264enc, etc.)
		// For now, we target NVIDIA.
		// We use `cudaupload` and `cudascale` to ensure the video frames stay in GPU memory.
		// We use `cudaconvert` to ensure the video frames are in the correct format (NV12 or P010_10LE).
		let pipeline_str = format!(
			"pipewiresrc path={} ! cudaupload ! cudascale ! cudaconvert ! video/x-raw(memory:CUDAMemory),width={},height={},format={} ! {} preset=p3 tune=ultra-low-latency rc-mode=cbr bitrate={} gop-size=-1 zerolatency=true bframes=0 ! {} ! {} ! appsink name=sink",
			self.node_id, self.width, self.height, format, encoder, bitrate_kbit, parser, caps_filter
		);

		tracing::debug!("Launching pipeline: {}", pipeline_str);

		let pipeline = match gst::parse::launch(&pipeline_str) {
			Ok(pipeline) => pipeline,
			Err(e) => {
				tracing::error!("Failed to parse GStreamer pipeline: {e}");
				return;
			}
		};

		let pipeline = match pipeline.dynamic_cast::<gst::Pipeline>() {
			Ok(pipeline) => pipeline,
			Err(_) => {
				tracing::error!("Failed to cast to GStreamer pipeline.");
				return;
			}
		};

		let sink = match pipeline.by_name("sink") {
			Some(sink) => sink,
			None => {
				tracing::error!("Failed to find sink element in pipeline.");
				return;
			}
		};

		let sink = match sink.dynamic_cast::<AppSink>() {
			Ok(sink) => sink,
			Err(_) => {
				tracing::error!("Failed to cast sink to AppSink.");
				return;
			}
		};

		// Configure appsink
		// We want encoded buffers (video/x-h264).
		// The caps will be negotiated automatically, but we can enforce it if needed.
		sink.set_max_buffers(1);
		sink.set_drop(false);

		if let Err(e) = pipeline.set_state(gst::State::Playing) {
			tracing::error!("Failed to set pipeline state to Playing: {e}");
			let bus = pipeline.bus().unwrap();
			while let Some(msg) = bus.pop() {
				if let gst::MessageView::Error(err) = msg.view() {
					tracing::error!(
						"Error from {:?}: {} ({:?})",
						err.src().map(|s| s.path_string()),
						err.error(),
						err.debug()
					);
				}
			}
			return;
		}
		tracing::debug!("GStreamer pipeline started playing.");

		let mut packetizer = Packetizer::new();
		let mut sequence_number = 0u32;
		let mut frame_number = 0u32;

		// Main loop
		while !stop_session_manager.is_shutdown_triggered() {
			// Check bus for errors
			let bus = pipeline.bus().unwrap();
			while let Some(msg) = bus.pop() {
				use gst::MessageView;
				match msg.view() {
					MessageView::Error(err) => {
						tracing::error!(
							"Error from {:?}: {} ({:?})",
							err.src().map(|s| s.path_string()),
							err.error(),
							err.debug()
						);
					}
					MessageView::Warning(warn) => {
						tracing::warn!(
							"Warning from {:?}: {} ({:?})",
							warn.src().map(|s| s.path_string()),
							warn.error(),
							warn.debug()
						);
					}
					_ => (),
				}
			}
			// Check for IDR requests
			match idr_frame_request_rx.try_recv() {
				Ok(_) => {
					tracing::debug!("Received request for IDR frame.");
					// Send Force Key Unit event
					let event = gst_video::UpstreamForceKeyUnitEvent::builder()
						.all_headers(true)
						.build();
					pipeline.send_event(event);
				},
				Err(broadcast::error::TryRecvError::Empty) => {},
				Err(broadcast::error::TryRecvError::Lagged(_)) => {},
				Err(broadcast::error::TryRecvError::Closed) => {
					tracing::debug!("IDR frame channel closed.");
					break;
				}
			}

			// Pull sample from appsink
			let sample = match sink.try_pull_sample(gst::ClockTime::from_mseconds(10)) {
				Some(sample) => sample,
				None => {
					if sink.is_eos() {
						tracing::debug!("GStreamer pipeline EOS.");
						break;
					}
					// Timeout, continue loop to check for shutdown/IDR
					continue;
				}
			};

			let buffer = match sample.buffer() {
				Some(buffer) => buffer,
				None => {
					tracing::warn!("Received sample without buffer.");
					continue;
				}
			};

			let map = match buffer.map_readable() {
				Ok(map) => map,
				Err(e) => {
					tracing::error!("Failed to map buffer readable: {e}");
					continue;
				}
			};

			let data = map.as_slice();

			// Check if it's a keyframe
			// GST_BUFFER_FLAG_DELTA_UNIT is set if it's a delta unit (P/B frame).
			// So if it is NOT set, it's a keyframe (I frame).
			let is_key_frame = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

			let pts = buffer.pts().unwrap_or(gst::ClockTime::ZERO);
			let rtp_timestamp = (pts.nseconds() as u64 * 90 / 1000000) as u32;

			tracing::trace!("Received sample: size={}, keyframe={}, pts={}", data.len(), is_key_frame, pts);

			frame_number += 1;

			if packetizer.packetize(
				data,
				is_key_frame,
				&packet_tx,
				self.packet_size,
				self.minimum_fec_packets,
				self.fec_percentage,
				frame_number,
				&mut sequence_number,
				rtp_timestamp,
			).is_err() {
				tracing::error!("Failed to packetize frame.");
				break;
			}
		}

		let _ = pipeline.set_state(gst::State::Null);
		tracing::debug!("Video pipeline stopped.");
	}
}
