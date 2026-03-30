//! HDR10 metadata injection into encoded video bitstreams.
//!
//! Injects Mastering Display Colour Volume (MDCV) and Content Light Level
//! Info (CLLI) metadata as:
//! - SEI NAL units for H.264 and H.265 bitstreams.
//! - OBU metadata for AV1 bitstreams.
//!
//! Metadata is injected on key frames only, before the first VCL NAL unit.

use crate::session::compositor::frame::HdrMetadata;
use crate::session::stream::video::VideoFormat;

/// Build an H.264/H.265 MDCV SEI NAL unit.
///
/// H.265 SEI uses **GBR order** for display primaries (Green, Blue, Red),
/// while HdrMetadata stores them in RGB order.
fn build_mdcv_sei_payload(m: &HdrMetadata, is_hevc: bool) -> Vec<u8> {
	let mut payload = Vec::with_capacity(24);

	if is_hevc {
		// H.265: GBR order (Green, Blue, Red).
		let order = [1, 2, 0]; // index into display_primaries
		for &i in &order {
			let (x, y) = m.display_primaries[i];
			payload.extend(x.to_be_bytes());
			payload.extend(y.to_be_bytes());
		}
	} else {
		// H.264: RGB order (same as HdrMetadata).
		for &(x, y) in &m.display_primaries {
			payload.extend(x.to_be_bytes());
			payload.extend(y.to_be_bytes());
		}
	}

	// White point.
	payload.extend(m.white_point.0.to_be_bytes());
	payload.extend(m.white_point.1.to_be_bytes());

	// Max and min luminance (already in 0.0001 cd/m² units, big-endian u32).
	payload.extend(m.max_luminance.to_be_bytes());
	payload.extend(m.min_luminance.to_be_bytes());

	payload
}

/// Build an H.264/H.265 CLLI SEI NAL unit payload.
fn build_clli_sei_payload(m: &HdrMetadata) -> Vec<u8> {
	let mut payload = Vec::with_capacity(4);
	payload.extend(m.max_cll.to_be_bytes());
	payload.extend(m.max_fall.to_be_bytes());
	payload
}

/// Insert emulation prevention bytes (0x03) per H.264/H.265 Annex B RBSP rules.
///
/// Any occurrence of `00 00 XX` where `XX` is `00`, `01`, `02`, or `03` must be
/// escaped as `00 00 03 XX` to avoid accidental start-code patterns in the RBSP.
fn rbsp_escape(data: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(data.len());
	let mut consecutive_zeros = 0u32;
	for &byte in data {
		if consecutive_zeros >= 2 && byte <= 0x03 {
			out.push(0x03);
			consecutive_zeros = 0;
		}
		out.push(byte);
		if byte == 0x00 {
			consecutive_zeros += 1;
		} else {
			consecutive_zeros = 0;
		}
	}
	out
}

/// Wrap an SEI payload into a complete NAL unit (Annex B format with start code).
///
/// For H.264: NAL header = 0x06 (SEI type).
/// For H.265: NAL header = 0x4E01 (PREFIX_SEI_NUT = 39, nuh_layer_id=0, nuh_temporal_id_plus1=1).
///
/// The RBSP body is escaped with emulation prevention bytes per Annex B.
fn build_sei_nal(payload_type: u8, payload: &[u8], is_hevc: bool) -> Vec<u8> {
	// SEI message: payload_type bytes + payload_size bytes + payload + RBSP trailing bits.
	let mut sei_msg = Vec::new();

	// Emit payload type (multi-byte if >= 255).
	let mut pt = payload_type as u32;
	while pt >= 255 {
		sei_msg.push(0xFF);
		pt -= 255;
	}
	sei_msg.push(pt as u8);

	// Emit payload size (multi-byte if >= 255).
	let mut ps = payload.len();
	while ps >= 255 {
		sei_msg.push(0xFF);
		ps -= 255;
	}
	sei_msg.push(ps as u8);

	// Payload data.
	sei_msg.extend_from_slice(payload);

	// RBSP trailing bits (stop bit + alignment).
	sei_msg.push(0x80);

	// Escape the RBSP body to avoid accidental start-code patterns.
	let escaped = rbsp_escape(&sei_msg);

	// Build complete NAL unit with 4-byte start code.
	let mut nal = Vec::with_capacity(4 + 2 + escaped.len());
	nal.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // Start code.

	if is_hevc {
		// H.265 PREFIX_SEI_NUT (39): nal_unit_type=39 in bits 1-6 of first byte,
		// forbidden_zero_bit=0, nuh_layer_id=0 (5 bits in byte 0 bit 0 + byte 1 bits 7-3),
		// nuh_temporal_id_plus1=1 (3 bits in byte 1 bits 2-0).
		// byte0 = 0 | (39 << 1) = 0x4E, byte1 = 0x01
		nal.extend_from_slice(&[0x4E, 0x01]);
	} else {
		// H.264 NAL type 6 = SEI.
		// forbidden_zero_bit=0, nal_ref_idc=0, nal_unit_type=6
		nal.push(0x06);
	}

	nal.extend_from_slice(&escaped);
	nal
}

/// Build AV1 OBU metadata for HDR CLL (metadata_type = 1).
fn build_av1_cll_obu(m: &HdrMetadata) -> Vec<u8> {
	// OBU payload: metadata_type (leb128) + max_cll (u16 BE) + max_fall (u16 BE).
	let mut payload = Vec::new();
	// metadata_type = 1 (METADATA_TYPE_HDR_CLL), leb128 encoded.
	payload.push(1);
	payload.extend(m.max_cll.to_be_bytes());
	payload.extend(m.max_fall.to_be_bytes());

	build_av1_obu(5, &payload) // obu_type = OBU_METADATA = 5
}

/// Build AV1 OBU metadata for HDR MDCV (metadata_type = 2).
fn build_av1_mdcv_obu(m: &HdrMetadata) -> Vec<u8> {
	let mut payload = Vec::new();
	// metadata_type = 2 (METADATA_TYPE_HDR_MDCV), leb128 encoded.
	payload.push(2);
	// Display primaries (RGB order, u16 BE each).
	for &(x, y) in &m.display_primaries {
		payload.extend(x.to_be_bytes());
		payload.extend(y.to_be_bytes());
	}
	// White point.
	payload.extend(m.white_point.0.to_be_bytes());
	payload.extend(m.white_point.1.to_be_bytes());
	// Luminance (u32 BE).
	payload.extend(m.max_luminance.to_be_bytes());
	payload.extend(m.min_luminance.to_be_bytes());

	build_av1_obu(5, &payload) // obu_type = OBU_METADATA = 5
}

/// Build a complete AV1 OBU with header and size.
fn build_av1_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
	// OBU header byte: obu_forbidden_bit (1) = 0, obu_type (4), obu_extension_flag (1) = 0,
	// obu_has_size_field (1) = 1, obu_reserved_1bit (1) = 0.
	let header_byte = (obu_type << 3) | 0x02; // has_size = 1

	let mut obu = Vec::new();
	obu.push(header_byte);

	// Size field (leb128-encoded payload length).
	leb128_encode(&mut obu, payload.len() as u64);

	obu.extend_from_slice(payload);
	obu
}

/// Encode a value as unsigned LEB128.
fn leb128_encode(buf: &mut Vec<u8>, mut value: u64) {
	loop {
		let mut byte = (value & 0x7F) as u8;
		value >>= 7;
		if value != 0 {
			byte |= 0x80;
		}
		buf.push(byte);
		if value == 0 {
			break;
		}
	}
}

/// Find the position of the first VCL NAL unit in an Annex B bitstream.
///
/// For H.264: VCL types are 1-5 (coded slice NAL units).
/// For H.265: VCL types are 0-31.
///
/// Returns the byte offset of the start code (0x00000001 or 0x000001) of
/// the first VCL NAL unit, or the end of the data if none found.
fn find_first_vcl_nal(data: &[u8], is_hevc: bool) -> usize {
	let mut i = 0;
	while i < data.len() {
		// Look for start code (3 or 4 bytes).
		let (start_code_len, nal_start) = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
			(4, i + 4)
		} else if i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1] {
			(3, i + 3)
		} else {
			i += 1;
			continue;
		};

		if nal_start >= data.len() {
			return i;
		}

		if is_hevc {
			// H.265: NAL type is bits 1-6 of the first byte.
			let nal_type = (data[nal_start] >> 1) & 0x3F;
			if nal_type <= 31 {
				return i; // VCL NAL unit.
			}
		} else {
			// H.264: NAL type is bits 0-4 of the first byte.
			let nal_type = data[nal_start] & 0x1F;
			if (1..=5).contains(&nal_type) {
				return i; // VCL NAL unit.
			}
		}

		i += start_code_len;
	}

	data.len()
}

/// Find the position of the first frame OBU in an AV1 bitstream.
///
/// OBU_FRAME = 6, OBU_FRAME_HEADER = 3.
/// Returns the byte offset of the first frame OBU, or the end of the data.
fn find_first_frame_obu(data: &[u8]) -> usize {
	let mut i = 0;
	while i < data.len() {
		let header = data[i];
		let obu_type = (header >> 3) & 0x0F;
		let has_size = (header >> 1) & 0x01;
		let has_extension = (header >> 2) & 0x01;

		if obu_type == 6 || obu_type == 3 {
			return i; // OBU_FRAME or OBU_FRAME_HEADER
		}

		// Skip this OBU.
		let mut offset = i + 1;
		if has_extension == 1 {
			offset += 1; // extension header byte
		}
		if offset > data.len() {
			break; // Truncated OBU header.
		}
		if has_size == 1 {
			// Read leb128 size.
			let (size, bytes_read) = leb128_decode(&data[offset..]);
			offset += bytes_read;
			offset = offset.saturating_add(size as usize);
		} else {
			// Without size field, remaining data is this OBU.
			return data.len();
		}

		i = offset;
	}

	data.len()
}

/// Decode an unsigned LEB128 value. Returns (value, bytes consumed).
fn leb128_decode(data: &[u8]) -> (u64, usize) {
	let mut value: u64 = 0;
	let mut shift = 0;
	for (i, &byte) in data.iter().enumerate() {
		value |= ((byte & 0x7F) as u64) << shift;
		shift += 7;
		if byte & 0x80 == 0 {
			return (value, i + 1);
		}
	}
	(value, data.len())
}

/// Inject HDR metadata into an encoded bitstream for a key frame.
///
/// Returns a new bitstream with MDCV and CLLI metadata prepended before
/// the first VCL NAL unit (H.264/H.265) or frame OBU (AV1).
pub fn inject_hdr_metadata(data: &[u8], metadata: &HdrMetadata, format: VideoFormat) -> Vec<u8> {
	match format {
		VideoFormat::H264 => inject_h264_sei(data, metadata),
		VideoFormat::Hevc => inject_h265_sei(data, metadata),
		VideoFormat::Av1 => inject_av1_metadata(data, metadata),
	}
}

fn inject_h264_sei(data: &[u8], m: &HdrMetadata) -> Vec<u8> {
	let mdcv_payload = build_mdcv_sei_payload(m, false);
	let clli_payload = build_clli_sei_payload(m);

	// SEI payload type 137 = MDCV, type 144 = CLLI.
	let mdcv_nal = build_sei_nal(137, &mdcv_payload, false);
	let clli_nal = build_sei_nal(144, &clli_payload, false);

	let insert_pos = find_first_vcl_nal(data, false);

	let mut result = Vec::with_capacity(data.len() + mdcv_nal.len() + clli_nal.len());
	result.extend_from_slice(&data[..insert_pos]);
	result.extend_from_slice(&mdcv_nal);
	result.extend_from_slice(&clli_nal);
	result.extend_from_slice(&data[insert_pos..]);
	result
}

fn inject_h265_sei(data: &[u8], m: &HdrMetadata) -> Vec<u8> {
	let mdcv_payload = build_mdcv_sei_payload(m, true);
	let clli_payload = build_clli_sei_payload(m);

	// SEI payload type 137 = MDCV, type 144 = CLLI.
	let mdcv_nal = build_sei_nal(137, &mdcv_payload, true);
	let clli_nal = build_sei_nal(144, &clli_payload, true);

	let insert_pos = find_first_vcl_nal(data, true);

	let mut result = Vec::with_capacity(data.len() + mdcv_nal.len() + clli_nal.len());
	result.extend_from_slice(&data[..insert_pos]);
	result.extend_from_slice(&mdcv_nal);
	result.extend_from_slice(&clli_nal);
	result.extend_from_slice(&data[insert_pos..]);
	result
}

fn inject_av1_metadata(data: &[u8], m: &HdrMetadata) -> Vec<u8> {
	let cll_obu = build_av1_cll_obu(m);
	let mdcv_obu = build_av1_mdcv_obu(m);

	let insert_pos = find_first_frame_obu(data);

	let mut result = Vec::with_capacity(data.len() + cll_obu.len() + mdcv_obu.len());
	result.extend_from_slice(&data[..insert_pos]);
	result.extend_from_slice(&mdcv_obu);
	result.extend_from_slice(&cll_obu);
	result.extend_from_slice(&data[insert_pos..]);
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	fn test_metadata() -> HdrMetadata {
		HdrMetadata {
			display_primaries: [
				(34000, 16000), // Red
				(13250, 34500), // Green
				(7500, 3000),   // Blue
			],
			white_point: (15635, 16450),
			max_luminance: 10_000_000, // 1000 nits
			min_luminance: 500,        // 0.05 nits
			max_cll: 1000,
			max_fall: 400,
		}
	}

	#[test]
	fn test_mdcv_sei_payload_h264_rgb_order() {
		let m = test_metadata();
		let payload = build_mdcv_sei_payload(&m, false);
		assert_eq!(payload.len(), 24);

		// H.264: RGB order. Red first.
		assert_eq!(&payload[0..2], &34000u16.to_be_bytes()); // Red x
		assert_eq!(&payload[2..4], &16000u16.to_be_bytes()); // Red y
		assert_eq!(&payload[4..6], &13250u16.to_be_bytes()); // Green x
		assert_eq!(&payload[6..8], &34500u16.to_be_bytes()); // Green y
	}

	#[test]
	fn test_mdcv_sei_payload_h265_gbr_order() {
		let m = test_metadata();
		let payload = build_mdcv_sei_payload(&m, true);
		assert_eq!(payload.len(), 24);

		// H.265: GBR order. Green first.
		assert_eq!(&payload[0..2], &13250u16.to_be_bytes()); // Green x
		assert_eq!(&payload[2..4], &34500u16.to_be_bytes()); // Green y
		assert_eq!(&payload[4..6], &7500u16.to_be_bytes()); // Blue x
		assert_eq!(&payload[6..8], &3000u16.to_be_bytes()); // Blue y
		assert_eq!(&payload[8..10], &34000u16.to_be_bytes()); // Red x
		assert_eq!(&payload[10..12], &16000u16.to_be_bytes()); // Red y
	}

	#[test]
	fn test_clli_sei_payload() {
		let m = test_metadata();
		let payload = build_clli_sei_payload(&m);
		assert_eq!(payload.len(), 4);
		assert_eq!(&payload[0..2], &1000u16.to_be_bytes()); // MaxCLL
		assert_eq!(&payload[2..4], &400u16.to_be_bytes()); // MaxFALL
	}

	#[test]
	fn test_h265_sei_nal_structure() {
		let m = test_metadata();
		let mdcv_payload = build_mdcv_sei_payload(&m, true);
		let nal = build_sei_nal(137, &mdcv_payload, true);

		// Should start with 4-byte start code.
		assert_eq!(&nal[0..4], &[0, 0, 0, 1]);
		// H.265 PREFIX_SEI NAL header.
		assert_eq!(nal[4], 0x4E); // (39 << 1) = 78 = 0x4E
		assert_eq!(nal[5], 0x01); // nuh_temporal_id_plus1 = 1
							// SEI payload type = 137.
		assert_eq!(nal[6], 137);
		// SEI payload size = 24.
		assert_eq!(nal[7], 24);
	}

	#[test]
	fn test_h264_sei_nal_structure() {
		let m = test_metadata();
		let clli_payload = build_clli_sei_payload(&m);
		let nal = build_sei_nal(144, &clli_payload, false);

		// Start code.
		assert_eq!(&nal[0..4], &[0, 0, 0, 1]);
		// H.264 SEI NAL type.
		assert_eq!(nal[4], 0x06);
		// SEI payload type = 144.
		assert_eq!(nal[5], 144);
		// SEI payload size = 4.
		assert_eq!(nal[6], 4);
	}

	#[test]
	fn test_rbsp_escape() {
		// No escaping needed.
		assert_eq!(rbsp_escape(&[0x01, 0x02, 0x03]), vec![0x01, 0x02, 0x03]);

		// 00 00 00 → 00 00 03 00
		assert_eq!(rbsp_escape(&[0x00, 0x00, 0x00]), vec![0x00, 0x00, 0x03, 0x00]);

		// 00 00 01 → 00 00 03 01
		assert_eq!(rbsp_escape(&[0x00, 0x00, 0x01]), vec![0x00, 0x00, 0x03, 0x01]);

		// 00 00 02 → 00 00 03 02
		assert_eq!(rbsp_escape(&[0x00, 0x00, 0x02]), vec![0x00, 0x00, 0x03, 0x02]);

		// 00 00 03 → 00 00 03 03
		assert_eq!(rbsp_escape(&[0x00, 0x00, 0x03]), vec![0x00, 0x00, 0x03, 0x03]);

		// 00 00 04 is NOT escaped (only 00-03 trigger it).
		assert_eq!(rbsp_escape(&[0x00, 0x00, 0x04]), vec![0x00, 0x00, 0x04]);

		// Multiple occurrences.
		assert_eq!(
			rbsp_escape(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x00]),
			vec![0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00]
		);

		// Three consecutive zeros: first two trigger escape at third, then
		// the counter resets so the later zero doesn't trigger again.
		assert_eq!(
			rbsp_escape(&[0x00, 0x00, 0x00, 0x00]),
			vec![0x00, 0x00, 0x03, 0x00, 0x00]
		);
	}

	#[test]
	fn test_leb128_encode_decode() {
		let mut buf = Vec::new();
		leb128_encode(&mut buf, 127);
		assert_eq!(buf, vec![127]);

		buf.clear();
		leb128_encode(&mut buf, 128);
		assert_eq!(buf, vec![0x80, 0x01]);

		let (val, consumed) = leb128_decode(&[127]);
		assert_eq!(val, 127);
		assert_eq!(consumed, 1);

		let (val, consumed) = leb128_decode(&[0x80, 0x01]);
		assert_eq!(val, 128);
		assert_eq!(consumed, 2);
	}

	#[test]
	fn test_find_first_vcl_nal_h265() {
		// SPS NAL (type 33) + VPS NAL (type 32) + VCL NAL (type 1).
		let mut data = Vec::new();
		// SPS: start code + type 33 = (33 << 1) | 0 = 0x42, 0x01
		data.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01, 0xAA]);
		// VCL type 1: (1 << 1) = 0x02
		let vcl_pos = data.len();
		data.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01, 0xBB, 0xCC]);

		assert_eq!(find_first_vcl_nal(&data, true), vcl_pos);
	}

	#[test]
	fn test_inject_h265_inserts_before_vcl() {
		let m = test_metadata();
		// Minimal H.265 bitstream: SPS + VCL.
		let mut data = Vec::new();
		data.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01, 0xAA]); // SPS
		data.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01, 0xBB]); // VCL

		let result = inject_hdr_metadata(&data, &m, VideoFormat::Hevc);
		assert!(result.len() > data.len());

		// The SPS should still be at the start.
		assert_eq!(&result[0..4], &[0, 0, 0, 1]);
		assert_eq!(result[4], 0x42); // SPS type

		// The VCL should still be present at the end.
		let vcl_pos = result.len() - 7;
		assert_eq!(&result[vcl_pos..vcl_pos + 4], &[0, 0, 0, 1]);
		assert_eq!(result[vcl_pos + 4], 0x02); // VCL type 1
	}

	#[test]
	fn test_av1_obu_structure() {
		let m = test_metadata();
		let cll_obu = build_av1_cll_obu(&m);

		// OBU header: type=5 (metadata), has_size=1.
		assert_eq!(cll_obu[0], (5 << 3) | 0x02);

		let mdcv_obu = build_av1_mdcv_obu(&m);
		assert_eq!(mdcv_obu[0], (5 << 3) | 0x02);
	}
}
