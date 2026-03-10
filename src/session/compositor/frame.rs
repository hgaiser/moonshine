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
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
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
	#[allow(dead_code)]
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
