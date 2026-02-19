//! Frame export types for the compositor-to-encoder pipeline.
//!
//! `ExportedFrame` is the single frame-exchange type between the compositor
//! and the video pipeline. It replaces the PipeWire-based `CapturedFrame`.

use std::os::unix::io::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// A compositor frame exported for encoding.
///
/// Each plane's file descriptor is a *duplicate* of the GBM buffer's fd,
/// ensuring the buffer can be recycled independently of the encoder's
/// consumption timeline.
#[derive(Debug)]
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

impl Clone for ExportedFrame {
	fn clone(&self) -> Self {
		Self {
			planes: self.planes.iter().map(|p| p.clone()).collect(),
			format: self.format,
			modifier: self.modifier,
			width: self.width,
			height: self.height,
			created_at: self.created_at,
			buffer_index: self.buffer_index,
			consumed: self.consumed.clone(),
		}
	}
}

/// Metadata for a single DMA-BUF plane.
#[derive(Debug)]
pub struct ExportedPlane {
	/// Owned file descriptor — the encoder takes ownership.
	pub fd: OwnedFd,
	/// Byte offset into the DMA-BUF for this plane.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
}

impl Clone for ExportedPlane {
	fn clone(&self) -> Self {
		// Duplicate the fd so each clone has independent ownership.
		let dup_fd = self.fd.as_fd()
			.try_clone_to_owned()
			.expect("Failed to duplicate ExportedPlane fd during clone");
		Self {
			fd: dup_fd,
			offset: self.offset,
			stride: self.stride,
		}
	}
}
