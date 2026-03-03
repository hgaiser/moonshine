use reed_solomon_erasure::{galois_8, ReedSolomon};
use std::collections::{hash_map::Entry, HashMap};
use std::time::Instant;

use super::shard_batch::{ShardBatch, ShardBuf};

/// Maximum allowed number of shards in the encoder (data + parity).
pub const MAX_SHARDS: usize = 255;

const NV_VIDEO_PACKET_SIZE: usize = 16;
const RTP_HEADER_SIZE: usize = 12;
const PADDING_SIZE: usize = 4;
/// Byte offset where the NvVideoPacket starts within a shard.
const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE;
/// Byte offset where the payload starts within a shard.
const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE;

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

const VIDEO_FRAME_HEADER_SIZE: usize = 8;

impl VideoFrameHeader {
	fn serialize(&self, buffer: &mut [u8]) {
		buffer[0] = self.header_type;
		buffer[1..3].copy_from_slice(&self.padding1.to_le_bytes());
		buffer[3] = self.frame_type;
		buffer[4..8].copy_from_slice(&self.padding2.to_le_bytes());
	}
}

/// Write an RTP header directly into a byte slice at offset 0.
fn write_rtp_header(buf: &mut [u8], sequence_number: u16, timestamp: u32) {
	buf[0] = 0x90;
	buf[1] = 0; // packet_type
	buf[2..4].copy_from_slice(&sequence_number.to_be_bytes());
	buf[4..8].copy_from_slice(&timestamp.to_be_bytes());
	buf[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc
}

/// Write an NvVideoPacket directly into a byte slice.
fn write_nv_video_packet(
	buf: &mut [u8],
	stream_packet_index: u32,
	frame_index: u32,
	flags: u8,
	multi_fec_blocks: u8,
	fec_info: u32,
) {
	buf[0..4].copy_from_slice(&stream_packet_index.to_le_bytes());
	buf[4..8].copy_from_slice(&frame_index.to_le_bytes());
	buf[8] = flags;
	buf[9] = 0; // reserved
	buf[10] = 0x10; // multi_fec_flags
	buf[11] = multi_fec_blocks;
	buf[12..16].copy_from_slice(&fec_info.to_le_bytes());
}

/// Copy bytes from the logical [header ++ encoded_data] stream into a
/// destination slice, without materializing the concatenation.
fn copy_header_and_data(
	dst: &mut [u8],
	header: &[u8; VIDEO_FRAME_HEADER_SIZE],
	encoded_data: &[u8],
	offset: usize,
	len: usize,
) {
	let total = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
	let end = (offset + len).min(total);
	let mut written = 0;

	if offset < VIDEO_FRAME_HEADER_SIZE {
		let header_end = VIDEO_FRAME_HEADER_SIZE.min(end);
		let n = header_end - offset;
		dst[written..written + n].copy_from_slice(&header[offset..header_end]);
		written += n;
		if end > VIDEO_FRAME_HEADER_SIZE {
			let n = end - VIDEO_FRAME_HEADER_SIZE;
			dst[written..written + n].copy_from_slice(&encoded_data[..n]);
		}
	} else {
		let data_start = offset - VIDEO_FRAME_HEADER_SIZE;
		let data_end = end - VIDEO_FRAME_HEADER_SIZE;
		let n = data_end - data_start;
		dst[written..written + n].copy_from_slice(&encoded_data[data_start..data_end]);
	}
}

pub struct Packetizer {
	fec_encoders: HashMap<(usize, usize), ReedSolomon<galois_8::Field>>,
}

impl Packetizer {
	pub fn new() -> Self {
		Self {
			fec_encoders: HashMap::new(),
		}
	}

	/// Pre-create FEC encoders for all possible block sizes to avoid
	/// expensive ReedSolomon matrix construction during frame processing.
	pub fn warm_up(&mut self, fec_percentage: u8, minimum_fec_packets: u32) {
		let nr_parity_shards_per_block = MAX_SHARDS * fec_percentage as usize / (100 + fec_percentage as usize);
		let nr_data_shards_per_block = MAX_SHARDS - nr_parity_shards_per_block;

		for nr_data_shards in 1..=nr_data_shards_per_block {
			let nr_parity_shards = (nr_data_shards * fec_percentage as usize / 100)
				.max(minimum_fec_packets as usize)
				.min(MAX_SHARDS.saturating_sub(nr_data_shards));
			if nr_parity_shards > 0 {
				let _ = self.get_fec_encoder(nr_data_shards, nr_parity_shards);
			}
		}

		tracing::debug!("FEC encoder cache warmed with {} entries.", self.fec_encoders.len());
	}

	/// Packetize an encoded frame into a batch of network-ready shards.
	///
	/// Returns a `ShardBatch` containing all data + parity shards packed
	/// contiguously in a single allocation per block.
	#[allow(clippy::too_many_arguments)]
	pub fn packetize(
		&mut self,
		encoded_data: &[u8],
		is_key_frame: bool,
		requested_packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		frame_number: u32,
		sequence_number: &mut u32,
		rtp_timestamp: u32,
	) -> Result<ShardBatch, ()> {
		tracing::trace!(
			"Packetizing frame {}, size={}, keyframe={}",
			frame_number,
			encoded_data.len(),
			is_key_frame
		);

		let requested_shard_payload_size = requested_packet_size - NV_VIDEO_PACKET_SIZE;
		let packet_data_len = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
		let last_shard_size = packet_data_len % requested_shard_payload_size;
		let last_shard_size = if last_shard_size == 0 {
			requested_shard_payload_size
		} else {
			last_shard_size
		};

		let video_frame_header = VideoFrameHeader {
			header_type: 0x01,
			padding1: 0,
			frame_type: if is_key_frame { 2 } else { 1 },
			padding2: last_shard_size as u32,
		};

		let mut header_bytes = [0u8; VIDEO_FRAME_HEADER_SIZE];
		video_frame_header.serialize(&mut header_bytes);

		// The total size of a shard (RTP + padding + NvVideoPacket + payload).
		let requested_shard_size = PAYLOAD_OFFSET + requested_shard_payload_size;

		let nr_data_shards = packet_data_len.div_ceil(requested_shard_payload_size);
		assert!(nr_data_shards != 0);

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

		// Accumulate all blocks into a single batch.
		let mut all_shards = ShardBatch::empty();

		let mut total_alloc_us = 0u128;
		let mut total_data_write_us = 0u128;
		let mut total_fec_encoder_us = 0u128;
		let mut total_fec_compute_us = 0u128;
		let mut total_fec_headers_us = 0u128;
		let mut total_extend_us = 0u128;

		for block_index in 0..nr_blocks {
			let start = block_index * nr_data_shards_per_block;
			let mut end = ((block_index + 1) * nr_data_shards_per_block).min(nr_data_shards);

			if block_index == 3 {
				tracing::debug!("Trying to create {nr_blocks} blocks, but we are limited to 4 blocks so we are sending all remaining packets without FEC.");
				end = nr_data_shards;
			}

			let nr_data_shards = end - start;
			assert!(nr_data_shards != 0);

			let nr_parity_shards = (nr_data_shards * fec_percentage as usize / 100)
				.max(minimum_fec_packets as usize)
				.min(MAX_SHARDS.saturating_sub(nr_data_shards));

			let t_fec_encoder = Instant::now();
			let encoder = if nr_parity_shards > 0 {
				Some(self.get_fec_encoder(nr_data_shards, nr_parity_shards)?)
			} else {
				None
			};
			total_fec_encoder_us += t_fec_encoder.elapsed().as_micros();

			// Recompute the actual FEC percentage in case of a rounding error or when there are 0 parity shards.
			let fec_percentage = nr_parity_shards * 100 / nr_data_shards;

			tracing::trace!(
				"Sending block {block_index} with {nr_data_shards} data shards and {nr_parity_shards} parity shards."
			);

			// Single allocation for all shards in this block (data + parity), zeroed.
			let total_shards = nr_data_shards + nr_parity_shards;
			let t_alloc = Instant::now();
			let mut shard_buf = ShardBuf::new(total_shards, requested_shard_size);
			total_alloc_us += t_alloc.elapsed().as_micros();

			let t_data_write = Instant::now();

			// Write data shards directly into the flat buffer.
			for (block_shard_index, data_shard_index) in (start..end).enumerate() {
				let payload_start = data_shard_index * requested_shard_payload_size;
				let payload_len = requested_shard_payload_size.min(packet_data_len - payload_start);

				let shard = shard_buf.shard_mut(block_shard_index);

				// Write RTP header.
				write_rtp_header(shard, *sequence_number as u16, rtp_timestamp);

				// Padding (4 bytes of zeros) is already zeroed.

				// Write NvVideoPacket header.
				let mut flags = RtpFlag::ContainsPicData as u8;
				if block_shard_index == 0 {
					flags |= RtpFlag::StartOfFrame as u8;
				}
				if block_shard_index == nr_data_shards - 1 {
					flags |= RtpFlag::EndOfFrame as u8;
				}
				write_nv_video_packet(
					&mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE],
					*sequence_number << 8,
					frame_number,
					flags,
					((block_index as u8) << 4) | last_block_index,
					(block_shard_index << 12 | nr_data_shards << 22 | fec_percentage << 4) as u32,
				);

				// Copy payload from [header ++ encoded_data].
				copy_header_and_data(
					&mut shard[PAYLOAD_OFFSET..],
					&header_bytes,
					encoded_data,
					payload_start,
					payload_len,
				);

				// Remaining bytes are already zero (padding for undersized last shard).

				*sequence_number += 1;
			}

			// Parity shards are already zeroed from ShardBuf::new().

			total_data_write_us += t_data_write.elapsed().as_micros();

			if let Some(encoder) = encoder {
				// Create FEC-compatible slice views into the flat buffer.
				let mut fec_slices = shard_buf.as_fec_slices();

				let t_fec_compute = Instant::now();

				encoder
					.encode(&mut fec_slices)
					.map_err(|e| tracing::warn!("Failed to encode packet as FEC shards: {e}"))?;

				total_fec_compute_us += t_fec_compute.elapsed().as_micros();

				let t_fec_headers = Instant::now();

				// Write headers for parity shards. FEC overwrites the entire shard
				// content, so we patch the fields Moonlight needs afterward.
				for block_shard_index in 0..nr_parity_shards {
					let shard = shard_buf.shard_mut(nr_data_shards + block_shard_index);

					// RTP header.
					shard[0] = 0x90;
					shard[1] = 0; // packet_type
					shard[2..4].copy_from_slice(&(*sequence_number as u16).to_be_bytes());

					// NvVideoPacket fields that Moonlight needs.
					let nv = &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE];
					nv[4..8].copy_from_slice(&frame_number.to_le_bytes()); // frame_index
					nv[11] = ((block_index as u8) << 4) | last_block_index; // multi_fec_blocks
					let fec_info = ((nr_data_shards + block_shard_index) << 12
						| nr_data_shards << 22
						| fec_percentage << 4) as u32;
					nv[12..16].copy_from_slice(&fec_info.to_le_bytes()); // fec_info

					*sequence_number += 1;
				}

				total_fec_headers_us += t_fec_headers.elapsed().as_micros();
			}

			let t_extend = Instant::now();
			all_shards.extend_from(&shard_buf.into_batch());
			total_extend_us += t_extend.elapsed().as_micros();

			tracing::trace!("Finished sending frame {frame_number}.");

			if block_index == 3 {
				break;
			}
		}

		tracing::debug!(
			"Packetize breakdown: alloc_us={total_alloc_us} data_write_us={total_data_write_us} fec_encoder_us={total_fec_encoder_us} fec_compute_us={total_fec_compute_us} fec_headers_us={total_fec_headers_us} extend_us={total_extend_us}",
		);

		Ok(all_shards)
	}

	fn get_fec_encoder(
		&mut self,
		nr_data_shards: usize,
		nr_parity_shards: usize,
	) -> Result<&mut ReedSolomon<galois_8::Field>, ()> {
		Ok(match self.fec_encoders.entry((nr_data_shards, nr_parity_shards)) {
			Entry::Occupied(e) => {
				tracing::trace!("Found a FEC encoder for this combination of shards.");
				e.into_mut()
			},
			Entry::Vacant(e) => {
				tracing::trace!("No FEC encoder for this combination of shards, creating a new one.");
				let encoder = e.insert(
					ReedSolomon::<galois_8::Field>::new(nr_data_shards, nr_parity_shards)
						.map_err(|e| tracing::warn!("Couldn't create error correction encoder: {e}"))?,
				);
				tracing::trace!("Finished preparing FEC encoder.");

				encoder
			},
		})
	}
}
