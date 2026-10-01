//! Linux high-resolution packet deadlines without spinning or a pacing thread.
//! Tokio's timer wheel rounds short waits to milliseconds, material at 120 Hz.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;
use tokio::io::unix::AsyncFd;

pub(super) struct PacingTimer {
	fd: AsyncFd<OwnedFd>,
}

impl PacingTimer {
	pub fn new() -> io::Result<Self> {
		// SAFETY: scalar flags; a successful call returns an exclusively owned fd.
		let raw = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
		if raw < 0 {
			return Err(io::Error::last_os_error());
		}
		let fd = unsafe { OwnedFd::from_raw_fd(raw) };
		Ok(Self { fd: AsyncFd::new(fd)? })
	}

	/// One outstanding wait per timer, enforced by the mutable borrow. Re-arming
	/// clears a cancelled wait's pending expiry. Cached reactor readiness is
	/// cleared by try_io on WouldBlock, so cancellation cannot cause early sends.
	pub async fn sleep_until(&mut self, deadline: Instant) -> io::Result<()> {
		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			return Ok(());
		}
		let setting = libc::itimerspec {
			it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
			it_value: libc::timespec {
				tv_sec: remaining
					.as_secs()
					.try_into()
					.map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
				tv_nsec: remaining.subsec_nanos().into(),
			},
		};
		// SAFETY: the owned fd and initialized setting remain valid for this call.
		if unsafe { libc::timerfd_settime(self.fd.get_ref().as_raw_fd(), 0, &setting, std::ptr::null_mut()) } < 0 {
			return Err(io::Error::last_os_error());
		}
		loop {
			let mut ready = self.fd.readable().await?;
			match ready.try_io(|fd| {
				let mut expirations = 0u64;
				// SAFETY: timerfd reads exactly eight bytes into initialized storage.
				let count = unsafe { libc::read(fd.get_ref().as_raw_fd(), (&mut expirations as *mut u64).cast(), 8) };
				if count == 8 {
					Ok(())
				} else if count < 0 {
					Err(io::Error::last_os_error())
				} else {
					Err(io::Error::from(io::ErrorKind::UnexpectedEof))
				}
			}) {
				Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
				Ok(result) => return result,
				Err(_) => continue, // Readiness was stale; await the next kernel event.
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::future::Future;
	use std::time::Duration;

	#[tokio::test]
	async fn deadlines_do_not_fire_early_and_can_be_reused() {
		let mut timer = PacingTimer::new().unwrap();
		for _ in 0..3 {
			let deadline = Instant::now() + Duration::from_micros(300);
			timer.sleep_until(deadline).await.unwrap();
			assert!(Instant::now() >= deadline);
		}
		timer.sleep_until(Instant::now()).await.unwrap();
	}

	#[tokio::test]
	async fn cancelled_expiry_cannot_complete_a_new_wait_early() {
		let mut timer = PacingTimer::new().unwrap();
		let mut wait = Box::pin(timer.sleep_until(Instant::now() + Duration::from_millis(10)));
		// Poll to arm the fd, then cancel. Let that old expiry become readable.
		assert!(matches!(
			std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx))).await,
			std::task::Poll::Pending
		));
		drop(wait);
		tokio::time::sleep(Duration::from_millis(15)).await;
		let deadline = Instant::now() + Duration::from_millis(5);
		timer.sleep_until(deadline).await.unwrap();
		assert!(Instant::now() >= deadline);
	}
}
