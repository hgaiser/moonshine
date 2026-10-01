//! PyroWave setup contract. Codec API and transport dialect are independent.
/// Shared block format, verified against Nonary's vendored codec.
pub const BITSTREAM_ID: &str = "186f0393";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PyroWaveDialect {
	NativeWireV1,
	RecordFramed,
}
impl PyroWaveDialect {
	/// Older native peers echo version 1. Nonary advertises record support by
	/// feature bit 1 or its documented adaptive-FEC attribute. No UA heuristics.
	pub fn negotiate(
		version: Option<&str>,
		dialect: Option<&str>,
		bitstream: Option<&str>,
		features: Option<&str>,
		adaptive_fec: Option<&str>,
	) -> Result<Self, &'static str> {
		if bitstream.is_some_and(|id| id != BITSTREAM_ID) {
			return Err("PyroWave bitstream revision incompatible: host=186f0393");
		}
		let features = features
			.map(|s| s.parse::<u32>().map_err(|_| "Malformed PyroWave feature bits"))
			.transpose()?;
		if adaptive_fec.is_some_and(|s| s != "0" && s != "1") {
			return Err("Malformed PyroWave adaptive FEC attribute");
		}
		let record = features.is_some_and(|f| f & 1 != 0) || adaptive_fec.is_some();
		if let Some(version) = version {
			if version != "1" {
				return Err("Unsupported PyroWave native wire version; expected 1");
			}
			if record || dialect.is_some_and(|d| d != "native-wire-v1") {
				return Err("Contradictory PyroWave transport attributes");
			}
			return Ok(Self::NativeWireV1);
		}
		if record && dialect.is_none_or(|d| d == "record-framed") {
			return Ok(Self::RecordFramed);
		}
		Err("Unsupported PyroWave protocol dialect; native requires version 1, records require feature bit 1")
	}
}
/// Nonary accepts unpadded and straddling records. Prefix completeness permits
/// partial recovery; ordinary GameStream FEC and encryption remain unchanged.
pub(crate) fn record_boundaries(frame: &[u8], payload: usize) -> Result<(Vec<usize>, u16), ()> {
	if frame.len() < 8 || payload < 16 {
		return Err(());
	}
	let word = |at| u32::from_le_bytes(frame[at..at + 4].try_into().unwrap());
	let a = word(0);
	if a >> 31 == 0 || (word(4) >> 24) & 3 != 0 {
		return Err(());
	}
	let blocks = |extent: usize| ((extent.div_ceil(32) * 32).max(128) >> 5).div_ceil(32);
	let coarse = blocks((a & 0x3fff) as usize + 1) * blocks(((a >> 14) & 0x3fff) as usize + 1) * 12;
	let mut starts = vec![0];
	let mut offset = 8;
	let mut critical_end = 8;
	let mut detail_seen = false;
	while offset < frame.len() {
		if frame.len() - offset < 8 {
			return Err(());
		}
		let bytes = ((word(offset) >> 16) & 0xfff) as usize * 4;
		if word(offset) >> 31 != 0 || bytes < 8 || bytes > frame.len() - offset {
			return Err(());
		}
		if (word(offset + 4) >> 8) as usize >= coarse {
			detail_seen = true;
		} else if detail_seen {
			return Err(());
		} else {
			critical_end = offset + bytes;
		}
		starts.push(offset);
		offset += bytes;
	}
	Ok((
		starts,
		u16::try_from((8 + critical_end).div_ceil(payload)).map_err(|_| ())?,
	))
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn native_and_nonary_are_explicit() {
		assert_eq!(
			PyroWaveDialect::negotiate(Some("1"), None, None, None, None),
			Ok(PyroWaveDialect::NativeWireV1)
		);
		assert_eq!(
			PyroWaveDialect::negotiate(None, None, None, Some("1"), Some("0")),
			Ok(PyroWaveDialect::RecordFramed)
		);
		assert!(PyroWaveDialect::negotiate(None, None, None, None, None).is_err());
		assert!(PyroWaveDialect::negotiate(Some("2"), None, None, None, None).is_err());
		assert!(PyroWaveDialect::negotiate(Some("1"), None, None, Some("1"), None).is_err());
		assert!(PyroWaveDialect::negotiate(None, None, Some("unknown"), Some("1"), None).is_err());
		assert!(PyroWaveDialect::negotiate(None, None, None, Some("junk"), None).is_err());
	}
	#[test]
	fn prefix_and_record_starts_are_bounded() {
		let mut frame = Vec::new();
		frame.extend_from_slice(&(0x80000000u32 | 127 | (127 << 14)).to_le_bytes());
		frame.extend_from_slice(&2u32.to_le_bytes());
		frame.extend_from_slice(&(2u32 << 16).to_le_bytes());
		frame.extend_from_slice(&0u32.to_le_bytes());
		frame.extend_from_slice(&(2u32 << 16).to_le_bytes());
		frame.extend_from_slice(&(12u32 << 8).to_le_bytes());
		assert_eq!(record_boundaries(&frame, 16).unwrap(), (vec![0, 8, 16], 2));
		assert!(record_boundaries(&frame[..23], 16).is_err());
	}
}
