use async_shutdown::ShutdownManager;
use tokio::sync::watch;

use crate::session::manager::SessionShutdownReason;

/// Gate that holds a stream's workers back until streaming should begin.
///
/// Workers are spawned as soon as a stream is set up, but only start sending once the gate
/// opens. The gate is a flag and not a one-off wake-up, so it opens for every worker no matter
/// whether they were already waiting when `trigger()` ran, and triggering more than once is
/// harmless. Cloning shares the same gate.
#[derive(Clone)]
pub(crate) struct StartSignal(watch::Sender<bool>);

impl StartSignal {
	pub fn new() -> Self {
		Self(watch::channel(false).0)
	}

	/// Open the gate for all current and future waiters.
	pub fn trigger(&self) {
		self.0.send_replace(true);
	}

	/// Create a waiter for a worker.
	pub fn waiter(&self) -> StartWaiter {
		StartWaiter(self.0.subscribe())
	}
}

/// The worker's side of a [`StartSignal`].
pub(crate) struct StartWaiter(watch::Receiver<bool>);

impl StartWaiter {
	/// Wait until the gate opens.
	///
	/// Returns `false` if the session stops first, or if every [`StartSignal`] was dropped without
	/// triggering, in which case streaming can never begin. The worker should then exit.
	pub async fn wait(&mut self, stop: &ShutdownManager<SessionShutdownReason>) -> bool {
		matches!(stop.wrap_cancel(self.0.wait_for(|started| *started)).await, Ok(Ok(_)))
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	const TIMEOUT: Duration = Duration::from_secs(1);

	#[tokio::test]
	async fn trigger_before_waiting_is_not_lost() {
		let stop = ShutdownManager::new();
		let signal = StartSignal::new();
		let mut first = signal.waiter();
		let mut second = signal.waiter();

		// Both waiters see it, even though neither was waiting yet. A repeated trigger is harmless.
		signal.trigger();
		signal.trigger();
		assert!(tokio::time::timeout(TIMEOUT, first.wait(&stop)).await.unwrap());
		assert!(tokio::time::timeout(TIMEOUT, second.wait(&stop)).await.unwrap());

		// A waiter created after the trigger sees it as well.
		let mut late = signal.waiter();
		assert!(tokio::time::timeout(TIMEOUT, late.wait(&stop)).await.unwrap());
	}

	#[tokio::test]
	async fn shutdown_releases_a_waiting_worker() {
		let stop = ShutdownManager::new();
		let signal = StartSignal::new();
		let mut waiter = signal.waiter();

		stop.trigger_shutdown(SessionShutdownReason::ManagerShutdown).unwrap();
		assert!(!tokio::time::timeout(TIMEOUT, waiter.wait(&stop)).await.unwrap());
	}

	#[tokio::test]
	async fn dropping_the_signal_releases_a_waiting_worker() {
		let stop = ShutdownManager::new();
		let signal = StartSignal::new();
		let mut waiter = signal.waiter();

		drop(signal);
		assert!(!tokio::time::timeout(TIMEOUT, waiter.wait(&stop)).await.unwrap());
	}
}
