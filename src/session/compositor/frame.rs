//! Frame export types for the compositor-to-encoder pipeline.
//!
//! `ExportedFrame` is the single frame-exchange type between the compositor
//! and the video pipeline. It replaces the PipeWire-based `CapturedFrame`.

use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

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
