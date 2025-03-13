use std::{collections::{hash_map::Entry, HashMap}, sync::{atomic::{AtomicU32, Ordering}, Arc, Condvar, Mutex}};

use async_shutdown::ShutdownManager;
use ffmpeg::{
	codec::packet::flag::Flags, format::Pixel, option::Settable, Frame, Packet
};
use reed_solomon_erasure::{galois_8, ReedSolomon};
use tokio::sync::{broadcast::{self, error::TryRecvError}, mpsc};

use crate::session::{manager::SessionShutdownReason, stream::RtpHeader};

/// Maximum allowed number of shards in the encoder (data + parity).
pub const MAX_SHARDS: usize = 255;

#[repr(u8)]
enum RtpFlag {
	ContainsPicData = 0x1,
	EndOfFrame = 0x2,
	StartOfFrame = 0x4,
}

#[derive(Debug)]
#[repr(C)]
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

#[derive(Debug)]
#[repr(C)]
struct NvVideoPacket {
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
		buffer.extend(self.stream_packet_index.to_le_bytes());
		buffer.extend(self.frame_index.to_le_bytes());
		buffer.extend(self.flags.to_le_bytes());
		buffer.extend(self.reserved.to_le_bytes());
		buffer.extend(self.multi_fec_flags.to_le_bytes());
		buffer.extend(self.multi_fec_blocks.to_le_bytes());
		buffer.extend(self.fec_info.to_le_bytes());
	}
}

pub struct VideoEncoder { }

impl VideoEncoder {
	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	pub fn new(
		encoder_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		hw_frame_context: *mut ffmpeg::sys::AVBufferRef,
		codec_name: &str,
		width: u32,
		height: u32,
		framerate: u32,
		bitrate: usize,
		packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		packet_tx: mpsc::Sender<Vec<u8>>,
		idr_frame_request_rx: broadcast::Receiver<()>,
		frame_number: Arc<AtomicU32>,
		frame_notifier: Arc<Condvar>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		tracing::debug!("Using codec with name '{codec_name}'.");
		let codec = ffmpeg::encoder::find_by_name(codec_name)
			.ok_or_else(|| tracing::error!("Failed to find codec by name '{codec_name}'."))?;

		let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
			.encoder()
			.video()
			.map_err(|e| tracing::error!("Failed to create video encoder: {e}"))?;

		encoder.set_width(width);
		encoder.set_height(height);
		encoder.set_frame_rate(Some((framerate as i32, 1)));
		encoder.set_time_base((framerate as i32, 1));
		encoder.set_max_b_frames(0);
		encoder.set_bit_rate(bitrate);
		encoder.set_gop(i32::MAX as u32);
		unsafe {
			(*encoder.as_mut_ptr()).pix_fmt = Pixel::CUDA.into();
			(*encoder.as_mut_ptr()).hw_frames_ctx = hw_frame_context;
			(*encoder.as_mut_ptr()).refs = 0;
		}
		encoder.set_str("preset", "fast")
			.map_err(|e| tracing::error!("Failed to set preset for encoder: {e}"))?;
		encoder.set_str("tune", "ull")
			.map_err(|e| tracing::error!("Failed to set tuning option for encoder: {e}"))?;
		encoder.set_str("forced-idr", "1")
			.map_err(|e| tracing::error!("Failed to set forced-idr for encoder: {e}"))?;

		let encoder = encoder.open()
			.map_err(|e| tracing::error!("Failed to start encoder: {e}"))?;

		let inner = VideoEncoderInner { encoder, fec_encoders: HashMap::new() };
		std::thread::Builder::new().name("video-encode".to_string()).spawn(
			move || inner.run(
				packet_tx,
				idr_frame_request_rx,
				packet_size,
				minimum_fec_packets,
				fec_percentage,
				encoder_buffer,
				intermediate_buffer,
				frame_number,
				frame_notifier,
				stop_session_manager,
			)
		)
			.map_err(|e| tracing::error!("Failed to start video encode thread: {e}"))?;

		Ok(Self { })
	}
}

struct VideoEncoderInner {
	encoder: ffmpeg::encoder::Video,
	fec_encoders: HashMap<(usize, usize), ReedSolomon<galois_8::Field>>,
 }

impl VideoEncoderInner {
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
		captured_frame_number: Arc<std::sync::atomic::AtomicU32>,
		frame_notifier: Arc<std::sync::Condvar>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		tracing::debug!("Starting video encoder.");

		// Trigger session shutdown if we exit unexpectedly.
		let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoEncoderStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		let mut packet = Packet::empty();

		// The last frame number we used.
		let mut processed_captured_frame_number = 0;

		// The sequential frame number for sending to the client.
		let mut frame_number = 0;

		let mut sequence_number = 0u32;
		let stream_start_time = std::time::Instant::now();
		while !stop_session_manager.is_shutdown_triggered() {
			// Swap the intermediate buffer with the output buffer.
			// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
			{
				tracing::trace!("Checking for new frame.");

				// Acquire the lock for a new frame.
				let mut lock = match intermediate_buffer.lock() {
					Ok(lock) => lock,
					Err(e) => {
						tracing::error!("Failed to acquire buffer lock: {e}");
						continue;
					},
				};

				// Check if we missed a frame, in that case we don't need to wait for a new frame notification.
				let captured_frame_number = captured_frame_number.load(Ordering::Relaxed);
				// Check if we just started recording, or if we have not yet received a new frame.
				if captured_frame_number == 0 || captured_frame_number == processed_captured_frame_number {
					// Realistically we can wait indefinitely, but it feels safer to have a timeout just in case.
					let mut lock = match frame_notifier.wait_timeout(lock, std::time::Duration::from_secs(5)) {
						Ok(result) => result,
						Err(e) => {
							tracing::warn!("Failed to wait for new frame: {e}");
							continue;
						},
					};

					// Didn't get a lock, let's check shutdown status and try again.
					if lock.1.timed_out() {
						tracing::warn!("Failed to acquire lock for frame buffer.");
						continue;
					}

					tracing::trace!("Received notification for a new frame.");
					std::mem::swap(&mut *lock.0, &mut encoder_buffer);
					processed_captured_frame_number = captured_frame_number + 1;
				} else {
					tracing::debug!("We missed {} frame notification(s), continuing with newest frame.", captured_frame_number - processed_captured_frame_number);
					std::mem::swap(&mut *lock, &mut encoder_buffer);
					processed_captured_frame_number = captured_frame_number;
				}
			}

			frame_number += 1;

			tracing::trace!("Swapped new frame with old frame.");
			encoder_buffer.set_pts(Some(frame_number as i64));

			tracing::trace!("Sending frame {} to encoder", frame_number);

			// TODO: Check if this is necessary?
			// Reset possible previous request for keyframe.
			unsafe {
				(*encoder_buffer.as_mut_ptr()).pict_type = ffmpeg::picture::Type::None.into();
				(*encoder_buffer.as_mut_ptr()).key_frame = 0;
			}

			// Check if there was an IDR frame request.
			match idr_frame_request_rx.try_recv() {
				Ok(_) => {
					tracing::debug!("Received request for IDR frame.");
					unsafe {
						(*encoder_buffer.as_mut_ptr()).pict_type = ffmpeg::picture::Type::I.into();
						(*encoder_buffer.as_mut_ptr()).key_frame = 1;
					}
				},
				Err(TryRecvError::Empty) => {},
				Err(TryRecvError::Lagged(_)) => {},
				Err(TryRecvError::Closed) => {
					tracing::debug!("IDR frame channel closed, stopping video encoder.");
					break;
				}
			}

			// Send the frame to the encoder.
			tracing::trace!("Sending frame {}", frame_number);
			if let Err(e) = self.encoder.send_frame(&encoder_buffer) {
				tracing::error!("Error sending frame for encoding: {e}");
				continue;
			}

			loop {
				match self.encoder.receive_packet(&mut packet) {
					Ok(()) => {
						tracing::trace!("Received frame {} from encoder, converting frame to packets.", packet.pts().unwrap_or(-1));
						if self.encode_packet(
							&packet,
							&packet_tx,
							packet_size,
							minimum_fec_packets,
							fec_percentage,
							frame_number,
							&mut sequence_number,
							stream_start_time,
						).is_err() {
							continue;
						}
						tracing::trace!("Done converting frame {} to packets.", packet.pts().unwrap_or(-1));
					},
					Err(e) => {
						match e {
							ffmpeg::Error::Eof => {
								tracing::info!("End of file");
								break;
							},
							ffmpeg::Error::Other { errno: ffmpeg::sys::EAGAIN } => {
								// tracing::info!("Need more frames for encoding...");
								break;
							},
							e => {
								tracing::error!("Unexpected error while encoding: {e}");
								break;
							},
						}
					}
				}
			}
		}

		tracing::debug!("Video encoder stopped.");
	}

	#[allow(clippy::too_many_arguments)] // TODO: Problem for later..
	fn encode_packet(
		&mut self,
		packet: &Packet,
		packet_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
		requested_packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		frame_number: u32,
		sequence_number: &mut u32,
		stream_start_time: std::time::Instant,
	) -> Result<(), ()> {
		// Random padding, because we need it.
		const PADDING: u32 = 0;

		let timestamp = ((std::time::Instant::now() - stream_start_time).as_micros() / (1000 / 90)) as u32;

		// TODO: Figure out what this header means?
		let video_frame_header = VideoFrameHeader {
			header_type: 0x01, // Always 0x01 for short headers. What is this exactly?
			padding1: 0,
			frame_type: if packet.flags().contains(Flags::KEY) { 2 } else { 1 },
			padding2: 0,
		};

		// Prefix the frame with a VideoFrameHeader.
		let mut buffer = Vec::with_capacity(std::mem::size_of::<VideoFrameHeader>());
		video_frame_header.serialize(&mut buffer);
		let packet_data = packet.data()
			.ok_or_else(|| tracing::error!("Packet is empty, but we expected it to be full."))?;
		let packet_data = [&buffer, packet_data].concat();

		let requested_shard_payload_size = requested_packet_size - std::mem::size_of::<NvVideoPacket>();

		// The total size of a shard.
		let requested_shard_size =
			std::mem::size_of::<RtpHeader>()
			+ std::mem::size_of_val(&PADDING)
			+ std::mem::size_of::<NvVideoPacket>()
			+ requested_shard_payload_size;

		// Determine how many data shards we will be sending.
		let nr_data_shards = packet_data.len() / requested_shard_payload_size + (packet_data.len() % requested_shard_payload_size != 0) as usize; // TODO: Replace with div_ceil when it lands in stable (https://doc.rust-lang.org/std/primitive.i32.html#method.div_ceil).
		assert!(nr_data_shards != 0);

		// Determine how many parity and data shards are permitted per FEC block.
		let nr_parity_shards_per_block = MAX_SHARDS * fec_percentage as usize / (100 + fec_percentage as usize);
		let nr_data_shards_per_block = MAX_SHARDS - nr_parity_shards_per_block;

		// We need to subtract number of data shards by 1, otherwise you can get a situation where
		// there are for example 100 data shards allowed per block and also 100 data shards available.
		// In this case, nr_blocks = 100 / 100 + 1 = 2, but we only need to send 1 block.
		// Subtracting the value of nr_data_shards by 1 avoids this situation.
		let nr_blocks = (nr_data_shards - 1) / nr_data_shards_per_block + 1;
		let last_block_index = (nr_blocks.min(4) as u8 - 1) << 6; // TODO: Why the bit shift? To 'force' a limit of 4 blocks?

		tracing::trace!("Sending a max of {nr_data_shards_per_block} data shards and {nr_parity_shards_per_block} parity shards per block.");
		tracing::trace!("Sending {nr_blocks} blocks of video data.");

		for block_index in 0..nr_blocks {
			// Determine what data shards are in this block.
			let start = block_index * nr_data_shards_per_block;
			let mut end = ((block_index + 1) * nr_data_shards_per_block)
				.min(nr_data_shards);

			if block_index == 3 {
				tracing::debug!("Trying to create {nr_blocks} blocks, but we are limited to 4 blocks so we are sending all remaining packets without FEC.");
				end = nr_data_shards;
			}

			// Compute how many parity shards we will need (approximately) in this block.
			let nr_data_shards = end - start;
			assert!(nr_data_shards != 0);

			let nr_parity_shards = (nr_data_shards * fec_percentage as usize / 100)
				.max(minimum_fec_packets as usize) // Lower limit by the minimum number of parity shards.
				.min(MAX_SHARDS.saturating_sub(nr_data_shards)); // But hard total upper limit in the number of shards.

			// Create the FEC encoder for this amount of shards.
			let encoder = if nr_parity_shards > 0 {
				Some(self.get_fec_encoder(nr_data_shards, nr_parity_shards)?)
			} else {
				None
			};

			// Recompute the actual FEC percentage in case of a rounding error or when there are 0 parity shards.
			let fec_percentage = nr_parity_shards * 100 / nr_data_shards;

			tracing::trace!("Sending block {block_index} with {nr_data_shards} data shards and {nr_parity_shards} parity shards.");

			let mut shards = Vec::with_capacity(nr_data_shards + nr_parity_shards);
			for (block_shard_index, data_shard_index) in (start..end).enumerate() {
				// Determine which part of the payload is in this shard.
				let start = data_shard_index * requested_shard_payload_size;
				let end = ((data_shard_index + 1) * requested_shard_payload_size).min(packet_data.len());

				let mut shard = Vec::with_capacity(requested_shard_size);

				let rtp_header = RtpHeader {
					header: 0x90, // What is this?
					packet_type: 0,
					sequence_number: *sequence_number as u16,
					timestamp,
					ssrc: 0,
				};
				rtp_header.serialize(&mut shard);
				shard.extend(PADDING.to_le_bytes());

				let mut video_packet_header = NvVideoPacket {
					stream_packet_index: *sequence_number << 8,
					frame_index: frame_number,
					flags: RtpFlag::ContainsPicData as u8,
					reserved: 0,
					multi_fec_flags: 0x10,
					multi_fec_blocks: ((block_index as u8) << 4) | last_block_index,
					fec_info: (block_shard_index << 12 | nr_data_shards << 22 | fec_percentage << 4) as u32,
				};
				if block_shard_index == 0 {
					video_packet_header.flags |= RtpFlag::StartOfFrame as u8;
				}
				if block_shard_index == nr_data_shards - 1 {
					video_packet_header.flags |= RtpFlag::EndOfFrame as u8;
				}
				video_packet_header.serialize(&mut shard);

				// Append the payload.
				shard.extend(&packet_data[start..end]);

				// Pad with zeros at the end to make an equally sized shard.
				if end - start < requested_shard_payload_size {
					shard.extend(vec![0u8; requested_shard_payload_size - (end - start)]);
				}

				shards.push(shard);

				*sequence_number += 1;
			}

			if let Some(encoder) = encoder {
				for _ in 0..nr_parity_shards {
					shards.push(vec![0u8; requested_shard_size]);
				}

				encoder.encode(&mut shards)
					.map_err(|e| tracing::error!("Failed to encode packet as FEC shards: {e}"))?;

				// Force these values for the parity shards, we don't need to reconstruct them, but Moonlight needs them to match with the frame they came from.
				for (block_shard_index, shard) in shards[nr_data_shards..].iter_mut().enumerate() {
					let rtp_header = unsafe { &mut *(shard.as_mut_ptr() as *mut RtpHeader) };
					rtp_header.header = 0x90u8.to_be(); // The `.to_be` is redundant for u8, but is there to make it clear it should be big-endian.
					rtp_header.sequence_number = (*sequence_number as u16).to_be();

					let video_packet_header = unsafe {
						&mut *(shard.as_mut_ptr().add(std::mem::size_of::<RtpHeader>() + std::mem::size_of_val(&PADDING)) as *mut NvVideoPacket)
					};
					video_packet_header.multi_fec_blocks = ((block_index as u8) << 4) | last_block_index;
					video_packet_header.fec_info = ((nr_data_shards + block_shard_index) << 12 | nr_data_shards << 22 | fec_percentage << 4) as u32;
					video_packet_header.frame_index = frame_number;

					*sequence_number += 1;
				}
			}

			for (index, shard) in shards.into_iter().enumerate() {
				tracing::trace!("Sending shard {}/{} with size {} bytes.", index + 1, nr_data_shards + nr_parity_shards, shard.len());
				if packet_tx.blocking_send(shard).is_err() {
					tracing::info!("Couldn't send packet, video packet channel closed.");
					return Ok(());
				}
			}

			tracing::trace!("Finished sending frame {frame_number}.");

			// At this point we should have sent all the data shards in the last block, so we can break the loop.
			if block_index == 3 {
				break;
			}
		}

		Ok(())
	}

	fn get_fec_encoder(&mut self, nr_data_shards: usize, nr_parity_shards: usize) -> Result<&mut ReedSolomon<galois_8::Field>, ()> {
		Ok(match self.fec_encoders.entry((nr_data_shards, nr_parity_shards)) {
			Entry::Occupied(e) => {
				tracing::trace!("Found a FEC encoder for this combination of shards.");
				e.into_mut()
			},
			Entry::Vacant(e) => {
				tracing::trace!("No FEC encoder for this combination of shards, creating a new one.");
				let encoder = e.insert(ReedSolomon::<galois_8::Field>::new(nr_data_shards, nr_parity_shards)
					.map_err(|e| tracing::error!("Couldn't create error correction encoder: {e}"))?);
				tracing::trace!("Finished preparing FEC encoder.");

				encoder
			}
		})
	}
}
