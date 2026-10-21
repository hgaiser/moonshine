use aes_gcm::{
	Aes128Gcm, Key, Nonce,
	aead::{AeadInPlace, KeyInit},
};
use fec_rs::ReedSolomon;
use std::collections::{HashMap, hash_map::Entry};
use std::time::Instant;

use crate::session::SessionKeysReceiver;

use crate::session::stream::video::shard_batch::{ShardBatch, ShardBuf};

/// Maximum allowed number of shards in the encoder (data + parity).
pub(crate) const MAX_SHARDS: usize = 255;

const NV_VIDEO_PACKET_SIZE: usize = 16;
const RTP_HEADER_SIZE: usize = 12;
const PADDING_SIZE: usize = 4;
/// Byte offset where the NvVideoPacket starts within a shard.
const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE;
/// Byte offset where the payload starts within a shard.
const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE;

/// Size of the per-shard encryption prefix: iv(12) + frameNumber(4) + tag(16).
const ENC_PREFIX_SIZE: usize = 12 + 4 + 16;

/// Two-bit current and last block indices in `multiFecBlocks`.
const MAX_FEC_BLOCKS: usize = 4;
/// Ten-bit data count in `fecInfo`; zero does not represent 1024.
const MAX_DATA_SHARDS_PER_BLOCK: usize = 1023;

/// Choose the wire percentage first: Moonlight derives parity as
/// ceil(data * percentage / 100). Some counts cannot be expressed by an
/// eight-bit percentage, so the client minimum is bounded by both limits.
fn fec_parameters(data_shards: usize, percentage: u8, minimum: u32) -> (usize, usize) {
	let max_parity = MAX_SHARDS.saturating_sub(data_shards);
	let minimum = (minimum as usize).min(max_parity);
	let minimum_percentage = if minimum == 0 {
		0
	} else {
		(minimum - 1) * 100 / data_shards + 1
	};
	let max_percentage = (max_parity * 100 / data_shards).min(u8::MAX as usize);
	let percentage = (percentage as usize).max(minimum_percentage).min(max_percentage);
	((data_shards * percentage).div_ceil(100), percentage)
}

fn data_shards_per_block(percentage: u8) -> usize {
	MAX_SHARDS * 100 / (100 + percentage as usize)
}

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
	frame_processing_latency: u16,
	frame_type: u8,
	last_payload_len: u32,
}

const VIDEO_FRAME_HEADER_SIZE: usize = 8;

impl VideoFrameHeader {
	fn serialize(&self, buffer: &mut [u8]) {
		buffer[0] = self.header_type;
		buffer[1..3].copy_from_slice(&self.frame_processing_latency.to_le_bytes());
		buffer[3] = self.frame_type;
		buffer[4..8].copy_from_slice(&self.last_payload_len.to_le_bytes());
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

pub(crate) struct Packetizer {
	fec_encoders: HashMap<(usize, usize), ReedSolomon>,
	/// Watch channel for encryption keys — read eagerly per `packetize()` call.
	keys_rx: SessionKeysReceiver,
	/// Whether video encryption is enabled by the client.
	encrypt: bool,
	/// AES-128-GCM cipher for video encryption, `None` when disabled or uninitialized.
	cipher: Option<Aes128Gcm>,
	/// Last seen `remote_input_key_id` — used to detect key rotation.
	last_key_id: i64,
	/// Monotonically increasing IV counter (one increment per encrypted shard).
	gcm_iv_counter: u64,
}

impl Packetizer {
	pub fn new(encrypt: bool, keys_rx: SessionKeysReceiver) -> Self {
		Self {
			fec_encoders: HashMap::new(),
			keys_rx,
			encrypt,
			cipher: None,
			last_key_id: i64::MIN,
			gcm_iv_counter: 0,
		}
	}

	/// Update the cipher if the encryption key has rotated.
	/// Called eagerly at the start of each `packetize()` call.
	fn maybe_update_cipher(&mut self) {
		if !self.encrypt {
			return;
		}
		let keys = &*self.keys_rx.borrow();
		if keys.remote_input_key_id == self.last_key_id {
			return;
		}
		self.last_key_id = keys.remote_input_key_id;
		if keys.remote_input_key.len() != 16 {
			tracing::error!(
				"Video encryption key must be exactly 16 bytes, got {}",
				keys.remote_input_key.len()
			);
			self.cipher = None;
			return;
		}
		let key = Key::<Aes128Gcm>::from_slice(&keys.remote_input_key);
		self.cipher = Some(Aes128Gcm::new(key));
		self.gcm_iv_counter = 0;
		tracing::debug!("Video encryption cipher updated for key_id={}", self.last_key_id);
	}

	/// Pre-create FEC encoders for all possible block sizes to avoid
	/// expensive ReedSolomon matrix construction during frame processing.
	pub fn warm_up(&mut self, fec_percentage: u8, minimum_fec_packets: u32) {
		let nr_data_shards_per_block = data_shards_per_block(fec_percentage);

		for nr_data_shards in 1..=nr_data_shards_per_block {
			let (nr_parity_shards, _) = fec_parameters(nr_data_shards, fec_percentage, minimum_fec_packets);
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
		frame_processing_latency: u16,
	) -> Result<ShardBatch, ()> {
		// Eagerly read current encryption key and update cipher if rotated.
		self.maybe_update_cipher();

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
			frame_processing_latency,
			frame_type: if is_key_frame { 2 } else { 1 },
			last_payload_len: last_shard_size as u32,
		};

		let mut header_bytes = [0u8; VIDEO_FRAME_HEADER_SIZE];
		video_frame_header.serialize(&mut header_bytes);

		// The total size of a shard (RTP + padding + NvVideoPacket + payload).
		let requested_shard_size = PAYLOAD_OFFSET + requested_shard_payload_size;

		// When encryption is enabled, reserve space for the per-shard prefix.
		let prefix_size = if self.cipher.is_some() { ENC_PREFIX_SIZE } else { 0 };

		let nr_data_shards = packet_data_len.div_ceil(requested_shard_payload_size);
		assert!(nr_data_shards != 0);
		if nr_data_shards > MAX_FEC_BLOCKS * MAX_DATA_SHARDS_PER_BLOCK {
			tracing::warn!("Encoded frame {frame_number} needs {nr_data_shards} packets, exceeding the wire limit.");
			return Err(());
		}

		let nr_data_shards_per_block = data_shards_per_block(fec_percentage);
		// Preserve the protected layout while the last block is representable.
		// Otherwise balance four unprotected blocks, as a count above 1023
		// would truncate on the wire and prevent reconstruction without loss.
		let (nr_data_shards_per_block, fec_percentage, minimum_fec_packets) =
			if nr_data_shards > (MAX_FEC_BLOCKS - 1) * nr_data_shards_per_block + MAX_DATA_SHARDS_PER_BLOCK {
				(nr_data_shards.div_ceil(MAX_FEC_BLOCKS), 0, 0)
			} else {
				(nr_data_shards_per_block, fec_percentage, minimum_fec_packets)
			};

		// We need to subtract number of data shards by 1, otherwise you can get a situation where
		// there are for example 100 data shards allowed per block and also 100 data shards available.
		// In this case, nr_blocks = 100 / 100 + 1 = 2, but we only need to send 1 block.
		// Subtracting the value of nr_data_shards by 1 avoids this situation.
		let nr_blocks = (nr_data_shards - 1) / nr_data_shards_per_block + 1;
		// Last block occupies bits 6-7, current block bits 4-5.
		let last_block_index = (nr_blocks.min(MAX_FEC_BLOCKS) as u8 - 1) << 6;

		tracing::trace!("Sending a max of {nr_data_shards_per_block} data shards per FEC block.");
		tracing::trace!("Sending {nr_blocks} blocks of video data.");

		// Accumulate all blocks into a single batch.
		let mut all_shards = ShardBatch::empty();

		let mut total_alloc_us = 0u128;
		let mut total_data_write_us = 0u128;
		let mut total_fec_encoder_us = 0u128;
		let mut total_fec_compute_us = 0u128;
		let mut total_fec_headers_us = 0u128;
		let mut total_extend_us = 0u128;

		for block_index in 0..nr_blocks.min(MAX_FEC_BLOCKS) {
			let start = block_index * nr_data_shards_per_block;
			let end = if block_index == MAX_FEC_BLOCKS - 1 {
				nr_data_shards
			} else {
				((block_index + 1) * nr_data_shards_per_block).min(nr_data_shards)
			};

			let nr_data_shards = end - start;
			assert!(nr_data_shards != 0);

			let (nr_parity_shards, fec_percentage) =
				fec_parameters(nr_data_shards, fec_percentage, minimum_fec_packets);

			let t_fec_encoder = Instant::now();
			let encoder = if nr_parity_shards > 0 {
				Some(self.get_fec_encoder(nr_data_shards, nr_parity_shards)?)
			} else {
				None
			};
			total_fec_encoder_us += t_fec_encoder.elapsed().as_micros();

			tracing::trace!(
				"Sending block {block_index} with {nr_data_shards} data shards and {nr_parity_shards} parity shards."
			);

			// Single allocation for all shards in this block (data + parity), zeroed.
			let total_shards = nr_data_shards + nr_parity_shards;
			let t_alloc = Instant::now();
			let mut shard_buf = ShardBuf::new(total_shards, requested_shard_size, prefix_size);
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

			// Encrypt each shard if video encryption is enabled.
			if let Some(cipher) = &self.cipher {
				for shard_index in 0..total_shards {
					// Build the 12-byte IV: bytes 0..8 = counter (LE), byte 11 = 'V'.
					let mut iv = [0u8; 12];
					iv[..8].copy_from_slice(&self.gcm_iv_counter.to_le_bytes());
					iv[11] = b'V';
					self.gcm_iv_counter += 1;

					let nonce = Nonce::from_slice(&iv);

					// Encrypt the shard data in-place, returning a detached 16-byte tag.
					let shard_data = shard_buf.shard_mut(shard_index);
					let tag = cipher
						.encrypt_in_place_detached(nonce, b"", shard_data)
						.map_err(|e| tracing::warn!("Failed to encrypt video shard: {e}"))?;

					// Fill the encryption prefix: iv(12) + frameNumber(4) + tag(16).
					let prefix = shard_buf.prefix_mut(shard_index);
					prefix[..12].copy_from_slice(&iv);
					prefix[12..16].copy_from_slice(&frame_number.to_le_bytes());
					prefix[16..32].copy_from_slice(&tag);
				}
			}

			let t_extend = Instant::now();
			all_shards.extend(shard_buf.into_batch());
			total_extend_us += t_extend.elapsed().as_micros();

			tracing::trace!("Finished sending frame {frame_number}.");
		}

		tracing::trace!(
			"Packetize breakdown: alloc_us={total_alloc_us} data_write_us={total_data_write_us} fec_encoder_us={total_fec_encoder_us} fec_compute_us={total_fec_compute_us} fec_headers_us={total_fec_headers_us} extend_us={total_extend_us}",
		);

		Ok(all_shards)
	}

	fn get_fec_encoder(&mut self, nr_data_shards: usize, nr_parity_shards: usize) -> Result<&mut ReedSolomon, ()> {
		Ok(match self.fec_encoders.entry((nr_data_shards, nr_parity_shards)) {
			Entry::Occupied(e) => {
				tracing::trace!("Found a FEC encoder for this combination of shards.");
				e.into_mut()
			},
			Entry::Vacant(e) => {
				tracing::trace!("No FEC encoder for this combination of shards, creating a new one.");
				let encoder = e.insert(
					ReedSolomon::new(nr_data_shards, nr_parity_shards)
						.map_err(|e| tracing::warn!("Couldn't create error correction encoder: {e}"))?,
				);
				tracing::trace!("Finished preparing FEC encoder.");

				encoder
			},
		})
	}
}

#[cfg(test)]
mod fec_tests {
	use super::*;
	use std::collections::BTreeMap;

	fn packetizer(encrypted: bool) -> Packetizer {
		let (_, keys) = tokio::sync::watch::channel(crate::session::SessionKeyData {
			remote_input_key: vec![0x42; 16],
			remote_input_key_id: 1,
		});
		Packetizer::new(encrypted, keys)
	}

	fn frame(data_shards: usize) -> Vec<u8> {
		(0..data_shards * 64 - VIDEO_FRAME_HEADER_SIZE)
			.map(|index| (index % 251) as u8)
			.collect()
	}

	#[test]
	fn fec_parameters_fit_the_percentage_and_encoder_limits() {
		for data in 1..=1023 {
			for requested in 0..=u8::MAX {
				for minimum in [0, 2, 255, u32::MAX] {
					let (parity, percentage) = fec_parameters(data, requested, minimum);
					assert!(percentage <= 255);
					assert_eq!(parity, (data * percentage).div_ceil(100));
					assert!(parity == 0 || data + parity <= 255);
				}
			}
		}
		assert_eq!(fec_parameters(213, 20, 0), (41, 19));
		assert_eq!(fec_parameters(1, 20, 2), (2, 101));
		assert_eq!(fec_parameters(1, 20, u32::MAX), (3, 255));
	}

	fn reassemble(batch: &ShardBatch, encrypted: bool) -> Vec<u8> {
		let cipher = Aes128Gcm::new_from_slice(&[0x42; 16]).unwrap();
		let mut bytes = Vec::new();
		let mut last_payload = 0;
		let mut data_packets = 0;
		let mut highest_block = 0;
		let mut declared_last_block = None;
		for wire in batch.as_bytes().chunks_exact(batch.shard_size()) {
			let mut packet = if encrypted { wire[32..].to_vec() } else { wire.to_vec() };
			if encrypted {
				cipher
					.decrypt_in_place_detached(
						Nonce::from_slice(&wire[..12]),
						b"",
						&mut packet,
						aes_gcm::Tag::from_slice(&wire[16..32]),
					)
					.unwrap();
			}
			let last_block = packet[27] >> 6;
			assert_eq!(*declared_last_block.get_or_insert(last_block), last_block);
			highest_block = highest_block.max((packet[27] >> 4) & 3);
			let fec = u32::from_le_bytes(packet[28..32].try_into().unwrap());
			if (fec >> 12) & 0x3ff >= fec >> 22 {
				continue;
			}
			if data_packets == 0 {
				last_payload = u32::from_le_bytes(packet[36..40].try_into().unwrap()) as usize;
			}
			bytes.extend_from_slice(&packet[32..]);
			data_packets += 1;
		}
		assert_eq!(declared_last_block, Some(highest_block));
		let payload = batch.shard_size() - if encrypted { 64 } else { 32 };
		bytes.truncate((data_packets - 1) * payload + last_payload);
		bytes.drain(..8);
		bytes
	}

	#[test]
	fn oversized_frames_preserve_every_byte_without_packet_loss() {
		// Cover the fourth block's ten-bit count boundary at 20% FEC,
		// including the adjacent boundary when parity rounds upward.
		for shards in [1659, 1660, 1663, 2000, 4092] {
			for tail in [0, 13] {
				let mut data = frame(shards);
				data.truncate(data.len() - tail);
				for encrypted in [false, true] {
					let batch = packetizer(encrypted)
						.packetize(&data, true, 80, 2, 20, 1, &mut 0, 0, 0)
						.unwrap();
					let received = reassemble(&batch, encrypted);
					assert_eq!(received.len(), data.len(), "{shards} data packets, tail {tail}");
					assert!(received == data, "{shards} data packets, tail {tail}: bytes differ");
				}
			}
		}
	}

	#[test]
	fn oversized_frames_beyond_the_wire_limit_do_not_consume_sequence_numbers() {
		let mut sequence = 77;
		assert!(
			packetizer(false)
				.packetize(&frame(4093), true, 80, 2, 20, 1, &mut sequence, 0, 0)
				.is_err()
		);
		assert_eq!(sequence, 77);
	}

	#[test]
	fn emitted_parity_matches_the_receivers_count_and_recovers_a_packet() {
		for (data_shards, percentage, minimum) in [
			(213, 20, 0),
			(258, 20, 2),
			(137, 20, 0),
			(1, 20, 2),
			(1, 255, 8),
			(40, 0, 2),
			(40, 20, u32::MAX),
			(600, 100, 2),
		] {
			let batch = packetizer(false)
				.packetize(&frame(data_shards), true, 80, minimum, percentage, 1, &mut 0, 0, 0)
				.unwrap();
			let mut blocks = BTreeMap::<u8, Vec<Vec<u8>>>::new();
			for packet in batch.as_bytes().chunks_exact(batch.shard_size()) {
				blocks.entry((packet[27] >> 4) & 3).or_default().push(packet.to_vec());
			}
			for packets in blocks.values() {
				let fec_info = u32::from_le_bytes(packets[0][28..32].try_into().unwrap());
				let data = (fec_info >> 22) as usize;
				let percentage = ((fec_info >> 4) & 0xff) as usize;
				// Moonlight derives this count from the advertised percentage.
				let parity = (data * percentage).div_ceil(100);
				assert_eq!(
					packets.len(),
					data + parity,
					"{data_shards} data shards, minimum {minimum}"
				);
				assert!(parity > 0, "{data_shards} data shards, minimum {minimum}");
				assert!(data + parity <= 255);
				let expected = packets[0][PAYLOAD_OFFSET..].to_vec();
				let mut received: Vec<_> = packets.iter().cloned().map(Some).collect();
				received[0] = None;
				ReedSolomon::new(data, parity)
					.unwrap()
					.reconstruct_data(&mut received)
					.unwrap();
				assert_eq!(&received[0].as_ref().unwrap()[PAYLOAD_OFFSET..], expected);
			}
		}
	}
}
