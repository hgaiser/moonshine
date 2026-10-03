//! Holding client commits until their buffer is rendered.
//!
//! A client may commit a DMA-BUF while its GPU is still drawing into it. The
//! kernel tracks that work as fences on the buffer. The pre-commit hook here
//! takes a snapshot of the write fences at commit time and blocks the commit
//! until they have signaled, so a frame is only captured once the client has
//! finished drawing it.
//!
//! Fences added after the commit are left out on purpose. Drivers attach them
//! for later work that never touches the committed frame (amdgpu marks every
//! buffer a submission lists as written), so following the buffer's live state
//! holds a commit long after its own rendering finished.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{DisplayHandle, Resource};
use smithay::wayland::compositor::{
	Barrier, BufferAssignment, CompositorHandler, SurfaceAttributes, add_blocker, with_states,
};
use smithay::wayland::dmabuf::get_dmabuf;

use crate::session::compositor::state::MoonshineCompositor;

/// Pre-commit hook: hold the commit until its buffer is rendered.
pub(crate) fn hold_until_rendered(state: &mut MoonshineCompositor, _: &DisplayHandle, surface: &WlSurface) {
	let dmabuf = with_states(surface, |states| {
		match states.cached_state.get::<SurfaceAttributes>().pending().buffer.as_ref() {
			Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).ok().cloned(),
			_ => None,
		}
	});
	let Some(dmabuf) = dmabuf else {
		return;
	};

	// One blocker for each plane still being written; the commit applies once
	// all of them are released.
	for fence in dmabuf.handles().filter_map(export_pending) {
		let blocker = Barrier::new(false);
		let source = Generic::new(fence, Interest::READ, Mode::OneShot);
		let inserted = state.handle.insert_source(source, {
			let (blocker, surface) = (blocker.clone(), surface.clone());
			move |_, _, state| {
				blocker.signal();
				release(state, &surface);
				Ok(PostAction::Remove)
			}
		});
		if inserted.is_ok() {
			add_blocker(surface, blocker);
		}
	}
}

/// Apply a commit whose blocker has cleared.
fn release(state: &mut MoonshineCompositor, surface: &WlSurface) {
	let Some(client) = surface.client() else {
		return;
	};
	let dh = state.display_handle.clone();
	state.client_compositor_state(&client).blocker_cleared(state, &dh);
	// Applying the commit releases the buffer it replaced. A client waiting
	// for that buffer would otherwise idle until the next tick.
	let _ = state.display_handle.flush_clients();
}

// TODO: Smithay's `Dmabuf::generate_blocker` polls the buffer itself, so it
// also waits for the later fences. A snapshot like this one belongs there.

/// The write fences a DMA-BUF plane carries right now, as a sync file that
/// becomes readable once they have all signaled.
///
/// `None` if they already have, or if the kernel cannot export them (before
/// 6.0). The commit is then not held.
fn export_pending(plane: BorrowedFd<'_>) -> Option<OwnedFd> {
	#[repr(C)]
	struct ExportSyncFile {
		flags: u32,
		fd: i32,
	}
	/// `DMA_BUF_SYNC_READ` — not exposed by `libc`. Selects the fences a
	/// reader of the buffer has to wait for.
	const SYNC_READ: u32 = 1;
	/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`, that is
	/// `_IOWR('b', 2, struct dma_buf_export_sync_file)` — not exposed by `libc`.
	const EXPORT_SYNC_FILE: u64 = 0xC008_6202;

	// A readable buffer has no write pending. That is the usual case, and it
	// needs no export. If the poll fails, the export decides.
	let mut poll = libc::pollfd {
		fd: plane.as_raw_fd(),
		events: libc::POLLIN,
		revents: 0,
	};
	// SAFETY: `poll` points at one valid `pollfd`.
	if unsafe { libc::poll(&mut poll, 1, 0) } == 1 {
		return None;
	}

	let mut request = ExportSyncFile {
		flags: SYNC_READ,
		fd: -1,
	};
	// SAFETY: `plane` is an open fd and `request` matches the ioctl's argument layout.
	let exported = unsafe { libc::ioctl(plane.as_raw_fd(), EXPORT_SYNC_FILE as _, &mut request) } == 0;
	// SAFETY: the ioctl succeeded, so `request.fd` is a new fd owned by this process.
	exported.then(|| unsafe { OwnedFd::from_raw_fd(request.fd) })
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::fd::AsFd;
	use std::os::unix::net::UnixStream;

	#[test]
	fn plane_whose_fences_cannot_be_exported_is_not_waited_for() {
		// A socket with nothing to read polls like a buffer that is still being
		// written, and rejects the export like a kernel before 6.0 does.
		let (plane, _peer) = UnixStream::pair().unwrap();
		assert!(export_pending(plane.as_fd()).is_none());
	}
}
