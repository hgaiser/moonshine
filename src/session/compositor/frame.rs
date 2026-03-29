//! Frame export types for the compositor-to-encoder pipeline.
//!
//! `ExportedFrame` is the single frame-exchange type between the compositor
//! and the video pipeline. It replaces the PipeWire-based `CapturedFrame`.

use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

/// Color space of the compositor output frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameColorSpace {
	/// sRGB (BT.709 primaries, sRGB EOTF, BT.709 matrix).
	#[default]
	Srgb,
	/// HDR10 (BT.2020 primaries, PQ EOTF, BT.2020 NCL matrix).
	Bt2020Pq,
}

/// Static HDR metadata (HDR10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrMetadata {
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
pub struct HdrModeState {
	/// Whether the client's display should be in HDR mode.
	pub enabled: bool,
	/// HDR10 static metadata from the composited content.
	pub metadata: Option<HdrMetadata>,
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

/// A compositor frame exported for encoding.
///
/// Plane file descriptors are borrowed references to the compositor's
/// pre-allocated GBM buffer pool.  The pool lives for the entire streaming
/// session, so the fds remain valid.  The `consumed` flag prevents the
/// compositor from recycling a buffer before the encoder finishes reading.
#[derive(Debug, Clone)]
pub struct ExportedFrame {
	/// Per-plane DMA-BUF metadata.
	pub planes: Vec<ExportedPlane>,
	/// DRM format (e.g. Argb8888, Abgr2101010).
	pub format: u32,
	/// DRM modifier (e.g. Linear, tiled).
	pub modifier: u64,
	/// Frame width in pixels.
	pub width: u32,
	/// Frame height in pixels.
	pub height: u32,
	/// Timestamp when the frame was produced by the compositor.
	pub created_at: Instant,
	/// Index of the pre-allocated GBM buffer in the compositor's pool.
	pub buffer_index: usize,
	/// Shared flag set to `true` by the encoder after color conversion
	/// completes, signalling the compositor that this GBM buffer may be
	/// reused for rendering.
	pub consumed: Arc<AtomicBool>,
	/// Color space of the rendered frame.
	pub color_space: FrameColorSpace,
	/// Optional HDR metadata from the composited content.
	pub hdr_metadata: Option<HdrMetadata>,
}

/// Metadata for a single DMA-BUF plane.
///
/// The fd is a borrowed reference (raw fd number) into the compositor's
/// buffer pool.  It is NOT owned — do not close it.
#[derive(Debug, Clone, Copy)]
pub struct ExportedPlane {
	/// Raw file descriptor — borrowed from the compositor's buffer pool.
	pub fd: RawFd,
	/// Byte offset into the DMA-BUF for this plane.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
}
