//! Codec-independent negotiated video format.
//!
//! Keep every property which affects pixels or the bitstream explicit.  In
//! particular, HDR is not used as a proxy for bit depth and codecs are not used
//! as proxies for chroma sampling.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VideoCodec {
	#[default]
	H264,
	Hevc,
	Av1,
	PyroWave,
}

impl TryFrom<u32> for VideoCodec {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::H264),
			1 => Ok(Self::Hevc),
			2 => Ok(Self::Av1),
			// Pyroshine extension. This value is only sent by a client which
			// advertised the PyroWave extension; it never impersonates AV1/HEVC.
			3 => Ok(Self::PyroWave),
			_ => Err(()),
		}
	}
}

impl fmt::Display for VideoCodec {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::H264 => "H.264",
			Self::Hevc => "HEVC",
			Self::Av1 => "AV1",
			Self::PyroWave => "PyroWave",
		})
	}
}

/// Compatibility alias retained for downstream users of the public module.
pub type VideoFormat = VideoCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChromaFormat {
	#[default]
	Yuv420,
	Yuv444,
}

impl TryFrom<u32> for ChromaFormat {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::Yuv420),
			1 => Ok(Self::Yuv444),
			_ => Err(()),
		}
	}
}

impl fmt::Display for ChromaFormat {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Yuv420 => "4:2:0",
			Self::Yuv444 => "4:4:4",
		})
	}
}

/// Compatibility alias retained for the GameStream naming used previously.
pub type VideoChromaSampling = ChromaFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BitDepth {
	#[default]
	Eight,
	Ten,
}

impl BitDepth {
	pub const fn bits(self) -> u8 {
		match self {
			Self::Eight => 8,
			Self::Ten => 10,
		}
	}
}

impl TryFrom<u32> for BitDepth {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			8 => Ok(Self::Eight),
			10 => Ok(Self::Ten),
			_ => Err(()),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TransferFunction {
	#[default]
	Bt709,
	Pq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorPrimaries {
	#[default]
	Bt709,
	Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MatrixCoefficients {
	#[default]
	Bt709,
	Bt2020Ncl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorRange {
	#[default]
	Limited,
	Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VideoDynamicRange {
	#[default]
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

/// The complete, immutable format selected for a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NegotiatedVideoFormat {
	pub codec: VideoCodec,
	pub chroma: ChromaFormat,
	pub bit_depth: BitDepth,
	pub primaries: ColorPrimaries,
	pub transfer: TransferFunction,
	pub matrix: MatrixCoefficients,
	pub range: ColorRange,
	pub hdr: bool,
}

impl NegotiatedVideoFormat {
	pub const fn sdr(codec: VideoCodec, chroma: ChromaFormat, bit_depth: BitDepth, range: ColorRange) -> Self {
		Self {
			codec,
			chroma,
			bit_depth,
			primaries: ColorPrimaries::Bt709,
			transfer: TransferFunction::Bt709,
			matrix: MatrixCoefficients::Bt709,
			range,
			hdr: false,
		}
	}

	pub const fn hdr10(codec: VideoCodec, chroma: ChromaFormat, range: ColorRange) -> Self {
		Self {
			codec,
			chroma,
			bit_depth: BitDepth::Ten,
			primaries: ColorPrimaries::Bt2020,
			transfer: TransferFunction::Pq,
			matrix: MatrixCoefficients::Bt2020Ncl,
			range,
			hdr: true,
		}
	}

	pub const fn dynamic_range(self) -> VideoDynamicRange {
		if self.hdr {
			VideoDynamicRange::Hdr
		} else {
			VideoDynamicRange::Sdr
		}
	}

	/// Reject internally contradictory formats before GPU initialization.
	pub fn validate(self) -> Result<(), &'static str> {
		if self.hdr
			&& (self.bit_depth != BitDepth::Ten
				|| self.primaries != ColorPrimaries::Bt2020
				|| self.transfer != TransferFunction::Pq
				|| self.matrix != MatrixCoefficients::Bt2020Ncl)
		{
			return Err("HDR10 requires 10-bit BT.2020 primaries, PQ transfer, and BT.2020 NCL matrix");
		}
		if !self.hdr && self.transfer == TransferFunction::Pq {
			return Err("PQ transfer must be marked as HDR");
		}
		Ok(())
	}
}

impl Default for NegotiatedVideoFormat {
	fn default() -> Self {
		Self::sdr(
			VideoCodec::H264,
			ChromaFormat::Yuv420,
			BitDepth::Eight,
			ColorRange::Limited,
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn codec_and_format_are_independent() {
		for codec in [
			VideoCodec::H264,
			VideoCodec::Hevc,
			VideoCodec::Av1,
			VideoCodec::PyroWave,
		] {
			for chroma in [ChromaFormat::Yuv420, ChromaFormat::Yuv444] {
				assert!(
					NegotiatedVideoFormat::sdr(codec, chroma, BitDepth::Eight, ColorRange::Limited)
						.validate()
						.is_ok()
				);
				assert!(
					NegotiatedVideoFormat::hdr10(codec, chroma, ColorRange::Limited)
						.validate()
						.is_ok()
				);
			}
		}
	}

	#[test]
	fn contradictory_hdr_is_rejected() {
		let mut mode = NegotiatedVideoFormat::hdr10(VideoCodec::Av1, ChromaFormat::Yuv444, ColorRange::Full);
		mode.bit_depth = BitDepth::Eight;
		assert!(mode.validate().is_err());
	}
}
