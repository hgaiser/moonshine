use super::pyrowave_protocol::{PyroWaveDialect, record_boundaries};
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
/// The GameStream FEC header allocates 10 bits to the data-shard count.
const MAX_DATA_SHARDS_WITHOUT_FEC: usize = 1023;
/// The GameStream multi-FEC header allocates two bits to the last block index.
const MAX_FEC_BLOCKS: usize = 4;

const NV_VIDEO_PACKET_SIZE: usize = 16;
const RTP_HEADER_SIZE: usize = 12;
const PADDING_SIZE: usize = 4;
/// Byte offset where the NvVideoPacket starts within a shard.
const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE;
/// Byte offset where the payload starts within a shard.
const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE;

/// Size of the per-shard encryption prefix: iv(12) + frameNumber(4) + tag(16).
const ENC_PREFIX_SIZE: usize = 12 + 4 + 16;

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
	pyrowave_dialect: Option<PyroWaveDialect>,
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
	/// Rate limit recurring layout warnings for consistently large frames.
	last_fec_warning: Option<Instant>,
}

impl Packetizer {
	pub fn new(encrypt: bool, keys_rx: SessionKeysReceiver) -> Self {
		Self {
			pyrowave_dialect: None,
			fec_encoders: HashMap::new(),
			keys_rx,
			encrypt,
			cipher: None,
			last_key_id: i64::MIN,
			gcm_iv_counter: 0,
			last_fec_warning: None,
		}
	}

	pub fn set_pyrowave_dialect(&mut self, dialect: Option<PyroWaveDialect>) {
		self.pyrowave_dialect = dialect;
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
		for nr_data_shards in 1..MAX_SHARDS {
			if let Some((_, nr_parity_shards)) =
				block_fec_parameters(nr_data_shards, fec_percentage, minimum_fec_packets as usize)
				&& nr_parity_shards > 0
			{
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

		let requested_shard_payload_size =
			requested_packet_size.checked_sub(NV_VIDEO_PACKET_SIZE).ok_or_else(|| {
				tracing::warn!(
					requested_packet_size,
					"Video packet size is smaller than the transport header"
				)
			})?;
		if requested_shard_payload_size == 0 {
			tracing::warn!(requested_packet_size, "Video packet size leaves no room for a payload");
			return Err(());
		}
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
		let record_mode = self.pyrowave_dialect == Some(PyroWaveDialect::RecordFramed);
		let record_starts = if record_mode {
			let (starts, critical_packets) = record_boundaries(encoded_data, requested_shard_payload_size)?;
			header_bytes[6..8].copy_from_slice(&critical_packets.to_le_bytes());
			starts
		} else {
			Vec::new()
		};

		// The total size of a shard (RTP + padding + NvVideoPacket + payload).
		let requested_shard_size = PAYLOAD_OFFSET + requested_shard_payload_size;

		// When encryption is enabled, reserve space for the per-shard prefix.
		let prefix_size = if self.cipher.is_some() { ENC_PREFIX_SIZE } else { 0 };

		let nr_data_shards = packet_data_len.div_ceil(requested_shard_payload_size);
		assert!(nr_data_shards != 0);

		let layout = packet_layout(nr_data_shards, fec_percentage, minimum_fec_packets as usize)?;
		if (layout.fec_percentage < fec_percentage || (layout.disable_fec && minimum_fec_packets > 0))
			&& self
				.last_fec_warning
				.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
		{
			if layout.disable_fec {
				tracing::warn!(
					nr_data_shards,
					requested_fec_percentage = fec_percentage,
					minimum_fec_packets,
					"No protected video FEC layout is representable; sending this frame without parity"
				);
			} else {
				tracing::warn!(
					nr_data_shards,
					requested_fec_percentage = fec_percentage,
					effective_fec_percentage = layout.fec_percentage,
					blocks = layout.blocks,
					"Reduced video FEC to fit the GameStream four-block limit"
				);
			}
			self.last_fec_warning = Some(Instant::now());
		}
		let nr_blocks = layout.blocks;
		let nr_data_shards_per_block = layout.data_shards_per_block;
		let fec_percentage = layout.fec_percentage;
		let disable_fec = layout.disable_fec;
		let last_block_index = (nr_blocks as u8 - 1) << 6;

		tracing::trace!(
			nr_data_shards_per_block,
			fec_percentage,
			"Selected video FEC block layout"
		);
		tracing::trace!("Sending {nr_blocks} blocks of video data.");

		// Accumulate all blocks into a single batch.
		let mut all_shards = ShardBatch::empty();

		let mut total_alloc_us = 0u128;
		let mut total_data_write_us = 0u128;
		let mut total_fec_encoder_us = 0u128;
		let mut total_fec_compute_us = 0u128;
		let mut total_fec_headers_us = 0u128;
		let mut total_extend_us = 0u128;
		let mut total_parity_shards = 0usize;

		for block_index in 0..nr_blocks {
			let start = block_index * nr_data_shards_per_block;
			let end = ((block_index + 1) * nr_data_shards_per_block).min(nr_data_shards);

			let nr_data_shards = end - start;
			assert!(nr_data_shards != 0);

			let (fec_percentage, nr_parity_shards) = if disable_fec {
				(0, 0)
			} else {
				block_fec_parameters(nr_data_shards, fec_percentage, minimum_fec_packets as usize)
					.ok_or_else(|| tracing::error!(nr_data_shards, "Selected video FEC layout became invalid"))?
			};

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
			total_parity_shards = total_parity_shards
				.checked_add(nr_parity_shards)
				.ok_or_else(|| tracing::error!("Video parity-shard count overflow"))?;
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
					(block_shard_index << 12 | nr_data_shards << 22 | usize::from(fec_percentage) << 4) as u32,
				);

				// This exact record-start metadata is included in plaintext FEC
				// and encryption. Native and conventional shard bytes stay intact.
				if record_mode
					&& (payload_start == 0
						|| payload_start >= 8 && record_starts.binary_search(&(payload_start - 8)).is_ok())
				{
					shard[NV_PACKET_OFFSET + 9] |= 0x80;
				}
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
						| usize::from(fec_percentage) << 4) as u32;
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
			all_shards.extend_from(&shard_buf.into_batch());
			total_extend_us += t_extend.elapsed().as_micros();

			tracing::trace!("Finished sending frame {frame_number}.");
		}

		tracing::trace!(
			"Packetize breakdown: alloc_us={total_alloc_us} data_write_us={total_data_write_us} fec_encoder_us={total_fec_encoder_us} fec_compute_us={total_fec_compute_us} fec_headers_us={total_fec_headers_us} extend_us={total_extend_us}",
		);

		all_shards.set_frame_metadata(
			frame_number,
			encoded_data.len(),
			nr_data_shards,
			total_parity_shards,
			nr_blocks,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PacketLayout {
	blocks: usize,
	data_shards_per_block: usize,
	fec_percentage: u8,
	disable_fec: bool,
}

/// Moonlight derives parity as `ceil(data * percentage / 100)`. Keeping this
/// as the single sender-side rule guarantees that the percentage written to
/// `fecInfo` describes exactly the number of emitted parity shards.
fn receiver_parity_shards(nr_data_shards: usize, fec_percentage: u8) -> usize {
	(nr_data_shards * fec_percentage as usize).div_ceil(100)
}

/// Select the smallest wire percentage at or above `requested_percentage`
/// that also satisfies the client minimum. Some parity counts are not
/// representable by Moonlight's integer percentage formula, so parity is
/// always derived from the selected wire percentage rather than vice versa.
fn block_fec_parameters(
	nr_data_shards: usize,
	requested_percentage: u8,
	minimum_parity_shards: usize,
) -> Option<(u8, usize)> {
	for percentage in requested_percentage..=u8::MAX {
		let parity = receiver_parity_shards(nr_data_shards, percentage);
		if parity >= minimum_parity_shards && nr_data_shards + parity <= MAX_SHARDS {
			return Some((percentage, parity));
		}
	}
	None
}

/// Select a transport layout compatible with Moonlight's four-block and
/// Reed-Solomon's 255-shard limit. If the requested percentage does not fit,
/// protection is reduced one representable percentage point at a time. Only
/// when no protected four-block layout exists is FEC disabled for the frame.
fn packet_layout(
	nr_data_shards: usize,
	requested_fec_percentage: u8,
	minimum_parity_shards: usize,
) -> Result<PacketLayout, ()> {
	debug_assert!(nr_data_shards > 0);
	if requested_fec_percentage == 0 && minimum_parity_shards == 0 {
		let blocks = nr_data_shards.div_ceil(MAX_DATA_SHARDS_WITHOUT_FEC);
		if blocks > MAX_FEC_BLOCKS {
			return Err(());
		}
		return Ok(PacketLayout {
			blocks,
			data_shards_per_block: nr_data_shards.div_ceil(blocks),
			fec_percentage: 0,
			disable_fec: true,
		});
	}

	for fec_percentage in (0..=requested_fec_percentage).rev() {
		for blocks in 1..=MAX_FEC_BLOCKS {
			let data_shards_per_block = nr_data_shards.div_ceil(blocks);
			if data_shards_per_block > MAX_SHARDS {
				continue;
			}
			let valid = (0..blocks).all(|block_index| {
				let start = block_index * data_shards_per_block;
				let data = nr_data_shards.saturating_sub(start).min(data_shards_per_block);
				data > 0 && block_fec_parameters(data, fec_percentage, minimum_parity_shards).is_some()
			});
			if valid {
				return Ok(PacketLayout {
					blocks,
					data_shards_per_block,
					fec_percentage,
					disable_fec: fec_percentage == 0 && minimum_parity_shards == 0,
				});
			}
		}
	}

	let blocks = nr_data_shards.div_ceil(MAX_DATA_SHARDS_WITHOUT_FEC);
	if blocks > MAX_FEC_BLOCKS {
		tracing::error!(
			nr_data_shards,
			"Encoded frame exceeds the GameStream 10-bit shard-count limit"
		);
		return Err(());
	}
	let data_shards_per_block = nr_data_shards.div_ceil(blocks);
	Ok(PacketLayout {
		blocks,
		data_shards_per_block,
		fec_percentage: 0,
		disable_fec: true,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn native_wire_v1_uses_the_ordinary_packetizer_byte_for_byte() {
		// Opaque bytes deliberately lack a codec header: the native packetizer
		// must not inspect them, add record metadata, or choose another framing.
		for encrypt in [false, true] {
			for fec in [0, 20] {
				let data: Vec<u8> = (0..32768).map(|i| (i * 131) as u8).collect();
				let mut ordinary = packetizer();
				ordinary.encrypt = encrypt;
				let mut native = packetizer();
				native.encrypt = encrypt;
				native.set_pyrowave_dialect(Some(PyroWaveDialect::NativeWireV1));
				let a = ordinary.packetize(&data, true, 1392, 0, fec, 1, &mut 0, 42, 3).unwrap();
				let b = native.packetize(&data, true, 1392, 0, fec, 1, &mut 0, 42, 3).unwrap();
				assert_eq!(a.as_bytes(), b.as_bytes());
				let mut records = packetizer();
				records.set_pyrowave_dialect(Some(PyroWaveDialect::RecordFramed));
				assert!(records.packetize(&data, true, 1392, 0, fec, 1, &mut 0, 42, 3).is_err());
			}
		}
	}

	#[test]
	fn record_metadata_survives_encryption_and_fec_loss() {
		let mut frame = Vec::new();
		frame.extend_from_slice(&(0x80000000u32 | 127 | (127 << 14)).to_le_bytes());
		frame.extend_from_slice(&32u32.to_le_bytes());
		for block in 0..32u32 {
			frame.extend_from_slice(&(2u32 << 16).to_le_bytes());
			frame.extend_from_slice(&(block << 8).to_le_bytes());
		}
		for encrypt in [false, true] {
			let mut p = packetizer();
			p.encrypt = encrypt;
			p.set_pyrowave_dialect(Some(PyroWaveDialect::RecordFramed));
			let batch = p.packetize(&frame, true, 80, 0, 20, 1, &mut 0, 0, 0).unwrap();
			let prefix = if encrypt { ENC_PREFIX_SIZE } else { 0 };
			let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&[0; 16]));
			let mut plain: Vec<Vec<u8>> = batch
				.as_bytes()
				.chunks_exact(batch.shard_size())
				.map(|shard| {
					let mut data = shard[prefix..].to_vec();
					if encrypt {
						cipher
							.decrypt_in_place_detached(
								Nonce::from_slice(&shard[..12]),
								b"",
								&mut data,
								aes_gcm::Tag::from_slice(&shard[16..32]),
							)
							.unwrap();
					}
					data
				})
				.collect();
			assert_eq!(
				u16::from_le_bytes(plain[0][PAYLOAD_OFFSET + 6..PAYLOAD_OFFSET + 8].try_into().unwrap()),
				2
			);
			for data in &plain[..batch.data_shards()] {
				assert_ne!(data[NV_PACKET_OFFSET + 9] & 0x80, 0);
			}
			// Parity headers are patched on wire; reconstruct only the payload
			// region so header patching cannot invalidate the RS equation.
			let original: Vec<Vec<u8>> = plain.iter().map(|s| s[PAYLOAD_OFFSET..].to_vec()).collect();
			let mut missing: Vec<Option<Vec<u8>>> =
				plain.drain(..).map(|s| Some(s[PAYLOAD_OFFSET..].to_vec())).collect();
			missing[0] = None;
			ReedSolomon::new(batch.data_shards(), batch.parity_shards())
				.unwrap()
				.reconstruct(&mut missing)
				.unwrap();
			assert_eq!(missing[0].as_ref().unwrap(), &original[0]);
			let rebuilt: Vec<u8> = missing[..batch.data_shards()]
				.iter()
				.flat_map(|s| s.as_ref().unwrap().iter().copied())
				.collect();
			assert_eq!(&rebuilt[8..8 + frame.len()], &frame);
		}
	}

	#[test]
	fn ordinary_frames_keep_fec() {
		let layout = packet_layout(400, 20, 0).unwrap();
		assert_eq!(layout.fec_percentage, 20);
		assert!(!layout.disable_fec);
		assert_eq!(layout.blocks, 2);
		assert_eq!(layout.data_shards_per_block, 200);
	}

	#[test]
	fn oversized_frames_reduce_fec_instead_of_disabling_it() {
		let layout = packet_layout(934, 20, 0).unwrap();
		assert_eq!(
			layout,
			PacketLayout {
				blocks: 4,
				data_shards_per_block: 234,
				fec_percentage: 8,
				disable_fec: false,
			}
		);
	}

	#[test]
	fn frames_beyond_the_header_limit_are_rejected() {
		assert!(packet_layout(MAX_DATA_SHARDS_WITHOUT_FEC * MAX_FEC_BLOCKS + 1, 20, 0).is_err());
	}

	#[test]
	fn zero_percentage_and_zero_minimum_disable_fec() {
		let layout = packet_layout(100, 0, 0).unwrap();
		assert!(layout.disable_fec);
		assert_eq!(layout.fec_percentage, 0);
	}

	fn packetizer() -> Packetizer {
		let (_tx, rx) = tokio::sync::watch::channel(crate::session::SessionKeyData {
			remote_input_key: vec![0; 16],
			remote_input_key_id: 0,
		});
		Packetizer::new(false, rx)
	}

	#[test]
	fn emitted_metadata_reconstructs_the_actual_parity_count() {
		for percentage in [5, 10, 20, 25] {
			let mut packetizer = packetizer();
			let mut sequence = 0;
			let batch = packetizer
				.packetize(&vec![0x55; 992], true, 116, 0, percentage, 1, &mut sequence, 0, 0)
				.unwrap();
			let first = &batch.as_bytes()[..batch.shard_size()];
			let fec_info = u32::from_le_bytes(first[NV_PACKET_OFFSET + 12..NV_PACKET_OFFSET + 16].try_into().unwrap());
			let data = ((fec_info >> 22) & 0x3ff) as usize;
			let wire_percentage = ((fec_info >> 4) & 0xff) as u8;
			let parity = batch.shard_count() - data;
			assert_eq!(batch.frame_number(), 1);
			assert_eq!(batch.encoded_size(), 992);
			assert_eq!(batch.data_shards(), data);
			assert_eq!(batch.parity_shards(), parity);
			assert_eq!(batch.fec_blocks(), 1);
			assert_eq!(wire_percentage, percentage);
			assert_eq!(parity, receiver_parity_shards(data, wire_percentage));
		}
	}

	#[test]
	fn minimum_parity_metadata_is_receiver_symmetric_for_a_tiny_frame() {
		let mut packetizer = packetizer();
		let mut sequence = 0;
		let batch = packetizer
			.packetize(&[0x55; 16], true, 116, 2, 20, 1, &mut sequence, 0, 0)
			.unwrap();
		let first = &batch.as_bytes()[..batch.shard_size()];
		let fec_info = u32::from_le_bytes(first[NV_PACKET_OFFSET + 12..NV_PACKET_OFFSET + 16].try_into().unwrap());
		let data = ((fec_info >> 22) & 0x3ff) as usize;
		let wire_percentage = ((fec_info >> 4) & 0xff) as u8;
		assert_eq!(data, 1);
		assert_eq!(batch.shard_count(), 3);
		assert_eq!(receiver_parity_shards(data, wire_percentage), 2);
	}

	#[test]
	fn no_fec_packetization_is_continuous_across_gso_chunk_boundaries() {
		for data_shards in [46 * 6, 46 * 7, 46 * 7 + 1, 46 * 8 - 7, 46 * 8] {
			let encoded_size = data_shards * 1376 - VIDEO_FRAME_HEADER_SIZE;
			let mut packetizer = packetizer();
			let mut sequence = 0;
			let batch = packetizer
				.packetize(&vec![0x55; encoded_size], true, 1392, 0, 0, 1, &mut sequence, 0, 0)
				.unwrap();
			assert_eq!(batch.shard_size(), 1408);
			assert_eq!(batch.data_shards(), data_shards);
			assert_eq!(batch.parity_shards(), 0);
			assert_eq!(batch.shard_count(), data_shards);
		}
	}

	#[test]
	fn reed_solomon_recovers_up_to_parity_count_including_bursts() {
		let data_shards = 50;
		let parity_shards = receiver_parity_shards(data_shards, 20);
		let encoder = ReedSolomon::new(data_shards, parity_shards).unwrap();
		let mut shards: Vec<Vec<u8>> = (0..data_shards + parity_shards)
			.map(|index| {
				if index < data_shards {
					(0..128).map(|byte| (index ^ byte) as u8).collect()
				} else {
					vec![0; 128]
				}
			})
			.collect();
		encoder.encode(&mut shards).unwrap();
		let original = shards.clone();

		let mut recoverable: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
		for shard in &mut recoverable[7..7 + parity_shards] {
			*shard = None;
		}
		encoder.reconstruct(&mut recoverable).unwrap();
		for index in 0..data_shards {
			assert_eq!(recoverable[index].as_ref().unwrap(), &original[index]);
		}

		let mut unrecoverable: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
		for shard in &mut unrecoverable[7..7 + parity_shards + 1] {
			*shard = None;
		}
		assert!(encoder.reconstruct(&mut unrecoverable).is_err());
	}

	#[test]
	fn client_minimum_uses_a_receiver_representable_percentage() {
		for data in 1..=255 {
			for requested in [0, 1, 5, 10, 20, 25] {
				for minimum in [0, 1, 2] {
					if let Some((percentage, parity)) = block_fec_parameters(data, requested, minimum) {
						assert_eq!(receiver_parity_shards(data, percentage), parity);
						assert!(parity >= minimum);
						assert!(data + parity <= MAX_SHARDS);
					}
				}
			}
		}
	}

	#[test]
	fn four_block_boundary_degrades_smoothly() {
		let before = packet_layout(848, 20, 0).unwrap();
		let after = packet_layout(849, 20, 0).unwrap();
		assert_eq!(before.blocks, 4);
		assert_eq!(before.fec_percentage, 20);
		assert_eq!(after.blocks, 4);
		assert!(after.fec_percentage > 0);
		assert!(after.fec_percentage <= 20);
	}
}
