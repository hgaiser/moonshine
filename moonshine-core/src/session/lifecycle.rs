//! Worker ownership primitives shared by every session subsystem.
//!
//! A session's [`ShutdownManager`] is its completion boundary: a worker that
//! owns sockets, threads or GPU objects registers a [`WorkerGuard`] *before* it
//! is spawned and holds it until every resource it owns has been dropped. The
//! manager therefore treats `wait_shutdown_complete()` as "every owned worker
//! has exited and released its resources", not merely "shutdown was requested".
//!
//! Stream workers wait for the client's `StartB` through a [`StartLatch`]. The
//! latch is persistent state rather than a consumable notification: it may be
//! opened before, during or after workers start waiting, any number of workers
//! observe the same opening, and opening it twice is harmless.

use async_shutdown::{DelayShutdownToken, ShutdownManager, TriggerShutdownToken};
use tokio::sync::watch;

use crate::session::manager::SessionShutdownReason;

/// Completion registration for one session worker.
///
/// Acquire this before spawning the worker and move it into the worker so the
/// spawn itself cannot race shutdown completion. Bind it as the first local of
/// the worker body: locals drop in reverse declaration order, so the delay
/// token is released only after the worker's sockets, threads and GPU objects.
/// If the spawn fails the guard is dropped with the closure, which stops the
/// session instead of leaving it partially started.
#[must_use = "a worker guard must live as long as the worker's resources"]
pub(crate) struct WorkerGuard {
	// Field order is drop order: an exiting worker stops the session first and
	// releases its completion hold last.
	_trigger: TriggerShutdownToken<SessionShutdownReason>,
	_delay: DelayShutdownToken<SessionShutdownReason>,
}

impl WorkerGuard {
	/// Register a worker that stops the session with `reason` if it exits.
	///
	/// Fails once the session's shutdown has already completed; a worker
	/// created that late must not be started.
	pub(crate) fn register(
		stop: &ShutdownManager<SessionShutdownReason>,
		reason: SessionShutdownReason,
	) -> Result<Self, ()> {
		let delay = stop
			.delay_shutdown_token()
			.map_err(|_| tracing::debug!(?reason, "Session already stopped; not starting worker"))?;
		Ok(Self {
			_trigger: stop.trigger_shutdown_token(reason),
			_delay: delay,
		})
	}
}

/// Persistent, idempotent start signal for one stream's workers.
#[derive(Clone, Debug)]
pub(crate) struct StartLatch {
	state: std::sync::Arc<watch::Sender<bool>>,
}

impl Default for StartLatch {
	fn default() -> Self {
		Self::new()
	}
}

impl StartLatch {
	pub(crate) fn new() -> Self {
		Self {
			state: std::sync::Arc::new(watch::channel(false).0),
		}
	}

	/// Open the latch. Returns `true` only for the call that opened it, so a
	/// duplicate `StartB` is observable but has no further effect.
	pub(crate) fn open(&self) -> bool {
		!self.state.send_replace(true)
	}

	#[cfg(test)]
	pub(crate) fn is_open(&self) -> bool {
		*self.state.borrow()
	}

	/// A waiter for one worker. Create it before spawning the worker.
	pub(crate) fn waiter(&self) -> StartWaiter {
		StartWaiter(self.state.subscribe())
	}
}

/// One worker's view of a [`StartLatch`].
pub(crate) struct StartWaiter(watch::Receiver<bool>);

impl StartWaiter {
	/// Wait until the latch opens. Returns `Err` when the session stops first
	/// (including when the stop was requested before this call) or the latch
	/// owner disappears; the worker must then exit without starting.
	pub(crate) async fn wait(mut self, stop: &ShutdownManager<SessionShutdownReason>) -> Result<(), ()> {
		if stop.is_shutdown_triggered() {
			return Err(());
		}
		match stop.wrap_cancel(self.0.wait_for(|open| *open)).await {
			Ok(Ok(_)) if !stop.is_shutdown_triggered() => Ok(()),
			_ => Err(()),
		}
	}

	/// [`Self::wait`] for an OS-thread worker without its own runtime.
	pub(crate) fn wait_blocking(self, stop: &ShutdownManager<SessionShutdownReason>) -> Result<(), ()> {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.map_err(|e| tracing::error!("Failed to build start-latch runtime: {e}"))?;
		runtime.block_on(self.wait(stop))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	fn stop() -> ShutdownManager<SessionShutdownReason> {
		ShutdownManager::new()
	}

	/// A lost opening must fail the test, not hang it.
	async fn started(wait: impl std::future::Future<Output = Result<(), ()>>) {
		tokio::time::timeout(Duration::from_secs(1), wait)
			.await
			.expect("an opened latch must start its waiter")
			.unwrap();
	}

	#[tokio::test]
	async fn latch_opened_before_any_waiter_starts_every_waiter() {
		let stop = stop();
		for waiters in 0..4 {
			let latch = StartLatch::new();
			let pending: Vec<_> = (0..waiters).map(|_| latch.waiter()).collect();
			assert!(latch.open());
			for waiter in pending {
				tokio::time::timeout(Duration::from_secs(1), waiter.wait(&stop))
					.await
					.unwrap()
					.unwrap();
			}
			// A waiter created after opening also starts immediately.
			started(latch.waiter().wait(&stop)).await;
		}
	}

	#[tokio::test]
	async fn duplicate_open_is_harmless() {
		let stop = stop();
		let latch = StartLatch::new();
		let first = latch.waiter();
		assert!(latch.open());
		assert!(!latch.open(), "only the first StartB opens the latch");
		assert!(latch.is_open());
		started(first.wait(&stop)).await;
		started(latch.waiter().wait(&stop)).await;
	}

	#[tokio::test]
	async fn stop_before_or_while_waiting_cancels_without_starting() {
		let latch = StartLatch::new();
		let early = stop();
		early.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		assert!(latch.waiter().wait(&early).await.is_err());

		let late = stop();
		let waiting = tokio::spawn({
			let late = late.clone();
			let waiter = latch.waiter();
			async move { waiter.wait(&late).await }
		});
		tokio::task::yield_now().await;
		late.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		assert!(waiting.await.unwrap().is_err());
		// Opening after the stop must not resurrect a cancelled worker.
		latch.open();
		assert!(latch.waiter().wait(&late).await.is_err());
	}

	#[tokio::test]
	async fn dropped_latch_releases_waiters() {
		let latch = StartLatch::new();
		let waiter = latch.waiter();
		drop(latch);
		assert!(waiter.wait(&stop()).await.is_err());
	}

	#[test]
	fn blocking_waiter_observes_an_earlier_open() {
		let latch = StartLatch::new();
		let waiter = latch.waiter();
		latch.open();
		std::thread::spawn(move || waiter.wait_blocking(&stop()))
			.join()
			.unwrap()
			.unwrap();
	}

	#[tokio::test]
	async fn worker_guard_holds_completion_and_stops_session_on_exit() {
		let stop = stop();
		let guard = WorkerGuard::register(&stop, SessionShutdownReason::VideoPacketHandlerStopped).unwrap();
		assert!(!stop.is_shutdown_triggered());
		drop(guard);
		assert_eq!(
			stop.shutdown_reason(),
			Some(SessionShutdownReason::VideoPacketHandlerStopped)
		);
		tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		// Registration after completion is refused: the worker must not start.
		assert!(WorkerGuard::register(&stop, SessionShutdownReason::VideoPacketHandlerStopped).is_err());
	}
}
