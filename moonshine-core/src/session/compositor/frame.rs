//! The colour of a compositor frame, in the encoder's terms.

/// Color space of the compositor output frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FrameColorSpace {
	/// sRGB (BT.709 primaries, sRGB EOTF, BT.709 matrix).
	#[default]
	Srgb,
	/// HDR10 (BT.2020 primaries, PQ EOTF, BT.2020 NCL matrix).
	///
	/// The pixel data is already BT.2020+PQ encoded; the encoder converts it
	/// straight to YUV (passthrough).
	Bt2020Pq,
	/// scRGB (BT.709 primaries, linear light, typically FP16 with values > 1.0
	/// for HDR highlights).
	///
	/// Used by DXGI HDR games whose swapchain negotiates
	/// `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`. The encoder must apply a
	/// BT.709→BT.2020 gamut mapping and PQ OETF before YUV conversion (it is
	/// *not* a passthrough), so it is tracked separately from `Bt2020Pq`.
	ScrgbLinear,
}

/// Static HDR metadata (HDR10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HdrMetadata {
	/// Mastering display color primaries (CIE 1931 xy, in 0.00002 units).
	pub display_primaries: [(u16, u16); 3],
	/// White point (CIE 1931 xy, in 0.00002 units).
	pub white_point: (u16, u16),
	/// Maximum luminance in 0.0001 cd/m² units.
	pub max_luminance: u32,
	/// Minimum luminance in 0.0001 cd/m² units.
	pub min_luminance: u32,
	/// Maximum content light level in cd/m² (nits).
	pub max_cll: u16,
	/// Maximum frame-average light level in cd/m² (nits).
	pub max_fall: u16,
}

/// HDR mode state sent from the video pipeline to the control stream.
///
/// Combines the `enabled` flag (whether the client should be in HDR mode)
/// with optional HDR metadata. The `enabled` flag toggles based on actual
/// frame content — SDR frames set it to false, HDR frames set it to true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HdrModeState {
	/// Whether the client's display should be in HDR mode.
	pub enabled: bool,
	/// HDR10 static metadata from the composited content.
	pub metadata: Option<HdrMetadata>,
}

impl HdrModeState {
	/// Create an initial HDR mode state with the given enabled flag.
	/// Metadata is `None` until the video pipeline provides it.
	pub fn new(enabled: bool) -> Self {
		Self {
			enabled,
			metadata: None,
		}
	}
}

impl HdrMetadata {
	/// Reasonable fallback metadata for HDR10 when applications don't provide
	/// their own. Uses BT.2020 primaries, D65 white point, and a conservative
	/// 1000 nit peak luminance.
	pub fn fallback() -> Self {
		Self {
			// BT.2020 display primaries in 0.00002 units.
			display_primaries: [
				(34000, 16000), // Red:   0.680, 0.320
				(13250, 34500), // Green: 0.265, 0.690
				(7500, 3000),   // Blue:  0.150, 0.060
			],
			// D65 white point in 0.00002 units.
			white_point: (15635, 16450), // 0.3127, 0.3290
			// 1000 nits max luminance in 0.0001 cd/m².
			max_luminance: 10_000_000,
			// 0.001 nits min luminance in 0.0001 cd/m².
			min_luminance: 10,
			// Unknown content light levels.
			max_cll: 0,
			max_fall: 0,
		}
	}
}

/// The encoder colour space for ferriscope's frame colour description.
pub(crate) fn color_space(colour: &ferriscope_core::ColourMetadata) -> FrameColorSpace {
	use ferriscope_core::{ColourPrimaries, TransferFunction};
	match (colour.transfer, colour.primaries) {
		(TransferFunction::Pq, ColourPrimaries::Bt2020) => FrameColorSpace::Bt2020Pq,
		(TransferFunction::Linear, ColourPrimaries::Bt709) => FrameColorSpace::ScrgbLinear,
		_ => FrameColorSpace::Srgb,
	}
}

/// HDR10 static metadata for an HDR frame, in the encoder's SEI units.
///
/// An HDR frame without mastering metadata gets [`HdrMetadata::fallback`],
/// keeping any content light level the application sent.
pub(crate) fn hdr_metadata(colour: &ferriscope_core::ColourMetadata) -> Option<HdrMetadata> {
	if color_space(colour) == FrameColorSpace::Srgb {
		return None;
	}
	// CIE xy in 0.00002 units, luminance in 0.0001 cd/m².
	let xy = |c: ferriscope_core::Chromaticity| ((c.x * 50_000.0).round() as u16, (c.y * 50_000.0).round() as u16);
	let mut metadata = match colour.mastering {
		Some(mastering) => HdrMetadata {
			display_primaries: [xy(mastering.red), xy(mastering.green), xy(mastering.blue)],
			white_point: xy(mastering.white_point),
			max_luminance: (mastering.max_luminance * 10_000.0).round() as u32,
			min_luminance: (mastering.min_luminance * 10_000.0).round() as u32,
			max_cll: 0,
			max_fall: 0,
		},
		None => HdrMetadata::fallback(),
	};
	if let Some(light) = colour.content_light {
		metadata.max_cll = light.max_cll;
		metadata.max_fall = light.max_fall;
	}
	Some(metadata)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn frame_colour_maps_to_the_encoder_colour_space_and_metadata() {
		use ferriscope_core::{
			Chromaticity, ColourMetadata, ColourPrimaries, ContentLightLevel, MasteringDisplay, TransferFunction,
		};
		let sdr = ColourMetadata::default();
		assert_eq!(color_space(&sdr), FrameColorSpace::Srgb);
		assert_eq!(hdr_metadata(&sdr), None);

		let xy = |x, y| Chromaticity { x, y };
		let pq = ColourMetadata {
			primaries: ColourPrimaries::Bt2020,
			transfer: TransferFunction::Pq,
			mastering: Some(MasteringDisplay {
				red: xy(0.68, 0.32),
				green: xy(0.265, 0.69),
				blue: xy(0.15, 0.06),
				white_point: xy(0.3127, 0.329),
				min_luminance: 0.005,
				max_luminance: 1000.0,
			}),
			content_light: Some(ContentLightLevel {
				max_cll: 800,
				max_fall: 400,
			}),
			..sdr
		};
		assert_eq!(color_space(&pq), FrameColorSpace::Bt2020Pq);
		let metadata = hdr_metadata(&pq).expect("HDR frame has metadata");
		assert_eq!(
			metadata.display_primaries,
			[(34000, 16000), (13250, 34500), (7500, 3000)]
		);
		assert_eq!(metadata.white_point, (15635, 16450));
		assert_eq!(metadata.max_luminance, 10_000_000);
		assert_eq!(metadata.min_luminance, 50);
		assert_eq!((metadata.max_cll, metadata.max_fall), (800, 400));

		let scrgb = ColourMetadata {
			transfer: TransferFunction::Linear,
			..sdr
		};
		assert_eq!(color_space(&scrgb), FrameColorSpace::ScrgbLinear);
		assert_eq!(hdr_metadata(&scrgb), Some(HdrMetadata::fallback()));
	}
}
