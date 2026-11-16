//! Waiting for the compositor's acquire fence before reading a frame.

use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;

/// How long the pipeline waits for the compositor's acquire fence. A frame whose
/// render has not finished after this is a stuck GPU, not a slow frame.
pub(super) const FENCE_TIMEOUT: Duration = Duration::from_secs(1);

/// Wait until a sync file fd signals (becomes readable).
///
/// Returns `Err` if it did not signal within `timeout`, `poll(2)` failed, or
/// `poll(2)` reported an error or hangup instead of `POLLIN`. A sync file only
/// ever reports `POLLIN`, also for a fence that signalled with an error, so
/// this guards against descriptors that are not sync files.
pub(super) fn wait_sync_file(fd: BorrowedFd<'_>, timeout: Duration) -> Result<(), String> {
	let deadline = Instant::now() + timeout;
	loop {
		let remaining = deadline.saturating_duration_since(Instant::now());
		let remaining = Timespec {
			tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
			tv_nsec: remaining.subsec_nanos().into(),
		};
		let mut fds = [PollFd::new(&fd, PollFlags::IN)];
		match poll(&mut fds, Some(&remaining)) {
			Ok(0) => return Err(format!("acquire fence did not signal within {timeout:?}")),
			Ok(_) => {
				// poll() counts a descriptor with an error or hangup as ready.
				let revents = fds[0].revents();
				if revents.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
					return Err(format!("poll on acquire fence reported {revents:?}"));
				}
				if revents.contains(PollFlags::IN) {
					return Ok(());
				}
				return Err(format!("poll on acquire fence reported {revents:?}"));
			},
			Err(Errno::INTR) => {},
			Err(error) => return Err(format!("poll on acquire fence failed: {error}")),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::fd::AsFd;

	#[test]
	fn wait_sync_file_returns_once_readable() {
		// A pipe with data is "signalled"; an empty one is not.
		let (read, write) = rustix::pipe::pipe().unwrap();
		assert!(wait_sync_file(read.as_fd(), Duration::from_millis(20)).is_err());
		assert_eq!(rustix::io::write(&write, &[1u8]).unwrap(), 1);
		assert!(wait_sync_file(read.as_fd(), Duration::from_millis(20)).is_ok());
	}

	#[test]
	fn wait_sync_file_rejects_hangup_without_signal() {
		// A pipe with no writer reports POLLHUP, not POLLIN.
		let (read, write) = rustix::pipe::pipe().unwrap();
		drop(write);
		let error = wait_sync_file(read.as_fd(), Duration::from_millis(20)).unwrap_err();
		assert!(error.contains("HUP"), "{error}");
	}
}
