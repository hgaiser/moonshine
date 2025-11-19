use std::collections::{hash_map::Entry, HashMap};
use reed_solomon_erasure::{galois_8, ReedSolomon};
use tokio::sync::mpsc;
use crate::session::stream::RtpHeader;

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

pub struct Packetizer {
	fec_encoders: HashMap<(usize, usize), ReedSolomon<galois_8::Field>>,
}

impl Packetizer {
	pub fn new() -> Self {
		Self {
			fec_encoders: HashMap::new(),
		}
	}

	#[allow(clippy::too_many_arguments)]
	pub fn packetize(
		&mut self,
		encoded_data: &[u8],
		is_key_frame: bool,
		packet_tx: &mpsc::Sender<Vec<u8>>,
		requested_packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		frame_number: u32,
		sequence_number: &mut u32,
		rtp_timestamp: u32,
	) -> Result<(), ()> {
		tracing::trace!("Packetizing frame {}, size={}, keyframe={}", frame_number, encoded_data.len(), is_key_frame);

		// Random padding, because we need it.
		const PADDING: u32 = 0;

		// TODO: Figure out what this header means?
		let video_frame_header = VideoFrameHeader {
			header_type: 0x01, // Always 0x01 for short headers. What is this exactly?
			padding1: 0,
			frame_type: if is_key_frame { 2 } else { 1 },
			padding2: 0,
		};

		// Prefix the frame with a VideoFrameHeader.
		let mut buffer = Vec::with_capacity(std::mem::size_of::<VideoFrameHeader>());
		video_frame_header.serialize(&mut buffer);
		let packet_data = [&buffer, encoded_data].concat();

		let requested_shard_payload_size = requested_packet_size - std::mem::size_of::<NvVideoPacket>();

		// The total size of a shard.
		let requested_shard_size =
			std::mem::size_of::<RtpHeader>()
			+ std::mem::size_of_val(&PADDING)
			+ std::mem::size_of::<NvVideoPacket>()
			+ requested_shard_payload_size;

		// Determine how many data shards we will be sending.
		let nr_data_shards = packet_data.len().div_ceil(requested_shard_payload_size);
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
					timestamp: rtp_timestamp,
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
