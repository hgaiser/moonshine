use std::sync::{Arc, Mutex};

use async_shutdown::ShutdownManager;
use ffmpeg::{Codec, CodecContextBuilder, Frame, CodecContext, Packet, HwFrameContextBuilder, HwFrameContext, CudaDeviceContextBuilder};

use crate::{cuda::CudaContext, session::stream::RtpHeader};

#[repr(u8)]
enum RtpFlag {
	ContainsPicData = 0x1,
	EndOfFrame = 0x2,
	StartOfFrame = 0x4,
}

struct VideoFrameHeader {
	header_type: u8,
	padding1: u16,
	frame_type: u8,
	padding2: u32,
}

impl VideoFrameHeader {
	fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.header_type.to_le_bytes());
		buffer.extend(self.padding1.to_le_bytes());
		buffer.extend(self.frame_type.to_le_bytes());
		buffer.extend(self.padding2.to_le_bytes());
	}
}

struct NvVideoPacket {
	padding: u32,
	stream_packet_index: u32,
	frame_index: u32,
	flags: u8,
	reserved: u8,
	multi_fec_flags: u8,
	multi_fec_blocks: u8,
	fec_info: u32,
}

impl NvVideoPacket {
	fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.padding.to_le_bytes());
		buffer.extend(self.stream_packet_index.to_le_bytes());
		buffer.extend(self.frame_index.to_le_bytes());
		buffer.extend(self.flags.to_le_bytes());
		buffer.extend(self.reserved.to_le_bytes());
		buffer.extend(self.multi_fec_flags.to_le_bytes());
		buffer.extend(self.multi_fec_blocks.to_le_bytes());
		buffer.extend(self.fec_info.to_le_bytes());
	}
}

pub struct Encoder {
	codec_context: CodecContext,
	pub hw_frame_context: HwFrameContext,
}

impl Encoder {
	pub fn new(
		cuda_context: &CudaContext,
		codec_name: &str,
		width: u32,
		height: u32,
		framerate: u32,
		bitrate: u64,
	) -> Result<Self, ()> {
		let cuda_device_context = CudaDeviceContextBuilder::new()
			.map_err(|e| log::error!("Failed to create CUDA device context: {e}"))?
			.set_cuda_context(cuda_context.as_raw())
			.build()
			.map_err(|e| log::error!("Failed to build CUDA device context: {e}"))?
		;

		let mut hw_frame_context = HwFrameContextBuilder::new(cuda_device_context)
			.map_err(|e| log::error!("Failed to create CUDA frame context: {e}"))?
			.set_width(width)
			.set_height(height)
			.set_sw_format(ffmpeg_sys::AV_PIX_FMT_0RGB32)
			.set_format(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA)
			.build()
			.map_err(|e| log::error!("Failed to build CUDA frame context: {e}"))?
		;

		let codec = Codec::new(codec_name)
			.map_err(|e| log::error!("Failed to create codec: {e}"))?;

		let mut codec_context_builder = CodecContextBuilder::new(&codec)
			.map_err(|e| log::error!("Failed to create codec context builder: {e}"))?;
		codec_context_builder
			.set_width(width)
			.set_height(height)
			.set_fps(framerate)
			.set_max_b_frames(0)
			.set_pix_fmt(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA)
			.set_bit_rate(bitrate)
			.set_gop_size(i32::max_value() as u32)
			.set_preset("fast")
			.set_tune("ull")
			.set_hw_frames_ctx(&mut hw_frame_context)
			.set_forced_idr(true)
		;
		codec_context_builder.as_raw_mut().refs = 1;

		let codec_context = codec_context_builder
			.open()
			.map_err(|e| log::error!("Failed to create codec context: {e}"))?;

		Ok(Self {
			codec_context,
			hw_frame_context,
		})
	}

	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub fn run(
		mut self,
		packet_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
		mut idr_frame_request_rx: tokio::sync::broadcast::Receiver<()>,
		packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		mut encoder_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		notifier: Arc<std::sync::Condvar>,
		stop_signal: ShutdownManager<()>,
	) -> Result<(), ()> {
		let mut packet = Packet::new()
			.map_err(|e| log::error!("Failed to create packet: {e}"))?;

		let mut frame_number = 0u32;
		let mut sequence_number = 0u32;
		let stream_start_time = std::time::Instant::now();
		while !stop_signal.is_shutdown_triggered() {
			// Swap the intermediate buffer with the output buffer.
			// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
			{
				log::trace!("Waiting for new frame.");
				// Wait for a new frame.
				let mut lock = notifier.wait(intermediate_buffer.lock().unwrap())
					.map_err(|e| log::error!("Failed to wait for new frame: {e}"))?;
				log::trace!("Received notification of new frame.");

				std::mem::swap(&mut *lock, &mut encoder_buffer);
			}
			log::trace!("Swapped new frame with old frame.");
			frame_number += 1;
			encoder_buffer.as_raw_mut().pts = frame_number as i64;

			log::trace!("Encoding frame {}", encoder_buffer.as_raw().pts);

			// TODO: Check if this is necessary?
			// Reset possible previous request for keyframe.
			encoder_buffer.as_raw_mut().pict_type = ffmpeg_sys::AVPictureType_AV_PICTURE_TYPE_NONE;
			encoder_buffer.as_raw_mut().key_frame = 0;

			// Check if there was an IDR frame request.
			match idr_frame_request_rx.try_recv() {
				Ok(_) => {
					log::debug!("Received request for IDR frame.");
					encoder_buffer.as_raw_mut().pict_type = ffmpeg_sys::AVPictureType_AV_PICTURE_TYPE_I;
					encoder_buffer.as_raw_mut().key_frame = 1;
				},
				Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {},
				Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {},
				Err(_) => {
					log::debug!("Channel closed, quitting encoder task.");
					return Ok(());
				}
			}

			// Send the frame to the encoder.
			self.codec_context.send_frame(Some(&encoder_buffer))
				.map_err(|e| log::error!("Error sending frame for encoding: {e}"))?;

			loop {
				match self.codec_context.receive_packet(&mut packet) {
					Ok(()) => {
						log::trace!("Sending frame {}", packet.as_raw().pts);
						encode_packet(
							&packet,
							&packet_tx,
							packet_size,
							minimum_fec_packets,
							fec_percentage,
							frame_number,
							&mut sequence_number,
							stream_start_time,
						)?
					},
					Err(e) => {
						if e.code == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
							// log::info!("Need more frames for encoding...");
							break;
						} else if e.code == ffmpeg_sys::AVERROR_EOF {
							log::info!("End of file");
							break;
						} else {
							log::error!("Error while encoding: {e}");
							break;
						}
					}
				}
			}
		}

		log::debug!("Received stop signal.");

		Ok(())
	}
}

#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
fn encode_packet(
	packet: &Packet,
	packet_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
	packet_size: usize,
	minimum_fec_packets: u32,
	fec_percentage: u8,
	frame_number: u32,
	sequence_number: &mut u32,
	stream_start_time: std::time::Instant,
) -> Result<(), ()> {
	// TODO: Figure out what this header means?
	let video_frame_header = VideoFrameHeader {
		header_type: 0x01, // Always 0x01 for short headers. What is this exactly?
		padding1: 0,
		frame_type: if (packet.as_raw().flags & ffmpeg_sys::AV_PKT_FLAG_KEY as i32) != 0 { 2 } else { 1 },
		padding2: 0,
	};
	let timestamp = ((std::time::Instant::now() - stream_start_time).as_micros() / (1000 / 90)) as u32;

	let mut buffer = Vec::new();
	video_frame_header.serialize(&mut buffer);
	let packet_data = [&buffer, packet.data()].concat();

	let payload_size = packet_size - std::mem::size_of::<NvVideoPacket>();
	let nr_data_shards = (packet_data.len() + payload_size - 1) / payload_size;
	let mut nr_parity_shards = (nr_data_shards * fec_percentage as usize / 100)
		.max(minimum_fec_packets as usize);
	let fec_percentage = nr_parity_shards * 100 / nr_data_shards;
	log::trace!("Number of packets: {nr_data_shards}, number of parity packets: {nr_parity_shards}");

	let encoder = reed_solomon::ReedSolomon::new(nr_data_shards, nr_parity_shards);
	if let Err(e) = &encoder {
		log::debug!("Couldn't create error correction: {e}");
		nr_parity_shards = 0;
	}

	let mut shards = Vec::with_capacity(nr_data_shards + nr_parity_shards);
	for i in 0..nr_data_shards {
		let start = i * payload_size;
		let end = ((i + 1) * payload_size).min(packet_data.len());

		// TODO: Do this without cloning.
		let mut shard = vec![0u8; payload_size];
		shard[..(end - start)].copy_from_slice(&packet_data[start..end]);
		shards.push(shard);
	}
	for _ in 0..nr_parity_shards {
		shards.push(vec![0u8; payload_size]);
	}
	if let Ok(encoder) = encoder {
		encoder.encode(&mut shards)
			.map_err(|e| log::error!("Failed to encode packet as FEC shards: {e}"))?;
	}

	for (index, shard) in shards.iter().enumerate() {
		let rtp_header = RtpHeader {
			header: 0x90, // What is this?
			packet_type: 0,
			sequence_number: *sequence_number as u16,
			timestamp,
			ssrc: 0,
		};

		let mut video_packet_header = NvVideoPacket {
			padding: 0,
			stream_packet_index: *sequence_number << 8,
			frame_index: frame_number,
			flags: RtpFlag::ContainsPicData as u8,
			reserved: 0,
			multi_fec_flags: 0x10,
			multi_fec_blocks: 0, // TODO: Support multiple blocks
			fec_info: (index << 12 | nr_data_shards << 22 | fec_percentage << 4) as u32,
		};
		if index == 0 {
			video_packet_header.flags |= RtpFlag::StartOfFrame as u8;
		}
		if index == nr_data_shards - 1 {
			video_packet_header.flags |= RtpFlag::EndOfFrame as u8;
		}

		let mut buffer = Vec::with_capacity(
			std::mem::size_of::<RtpHeader>()
			+ std::mem::size_of::<NvVideoPacket>()
			+ shard.len(),
		);
		rtp_header.serialize(&mut buffer);
		video_packet_header.serialize(&mut buffer);
		buffer.extend(shard);

		log::trace!("Sending packet {}/{} with size {} bytes.", index + 1, shards.len(), buffer.len());
		if packet_tx.blocking_send(buffer).is_err() {
			log::info!("Channel closed, couldn't send packet.");
			return Ok(());
		}

		*sequence_number += 1;
	}

	Ok(())
}