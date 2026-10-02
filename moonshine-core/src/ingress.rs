//! Admission and ownership of accepted TCP connections (HTTP, HTTPS, RTSP).
//!
//! Each accepted connection runs in its own task so a stalled peer (for
//! example one that never sends a TLS ClientHello) cannot block the accept
//! loop. A task holds:
//!
//! - a slot from a per-listener semaphore, bounding concurrent connections and
//!   their buffers; connections beyond the limit are closed immediately;
//! - a per-address share of those slots, so one host holding stalled
//!   connections cannot occupy the whole listener;
//! - a global [`DelayShutdownToken`], so global shutdown completes only after
//!   every handler has dropped its sockets and shared state (for example the
//!   session manager, which itself delays shutdown);
//! - global cancellation: when shutdown is triggered the handler future is
//!   dropped at its next await point, so shutdown is not delayed by idle peers.
//!
//! Protocol-specific deadlines (TLS handshake, request headers, RTSP framing)
//! are applied by the owning server inside the handler future.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_shutdown::{DelayShutdownToken, ShutdownManager};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::ShutdownReason;

/// Minimum interval between "connection limit reached" warnings per listener.
const SATURATION_WARNING_INTERVAL: Duration = Duration::from_secs(10);

/// Admission control for one listener.
#[derive(Clone)]
pub(crate) struct Ingress {
	name: &'static str,
	slots: Arc<Semaphore>,
	max_per_peer: usize,
	per_peer: Arc<Mutex<HashMap<IpAddr, usize>>>,
	shutdown: ShutdownManager<ShutdownReason>,
	last_saturation_warning: Arc<Mutex<Option<Instant>>>,
}

impl Ingress {
	/// Admit at most `max_connections` concurrent connections, a quarter of
	/// which (at least one) may come from any single address.
	pub(crate) fn new(name: &'static str, max_connections: usize, shutdown: ShutdownManager<ShutdownReason>) -> Self {
		Self {
			name,
			slots: Arc::new(Semaphore::new(max_connections)),
			max_per_peer: (max_connections / 4).max(1),
			per_peer: Default::default(),
			shutdown,
			last_saturation_warning: Default::default(),
		}
	}

	/// Admit a newly accepted connection, or `None` to close it immediately
	/// because the listener is saturated or the server is shutting down.
	pub(crate) fn admit(&self, peer: SocketAddr) -> Option<IngressPermit> {
		if self.shutdown.is_shutdown_triggered() {
			return None;
		}
		let ip = peer.ip().to_canonical();
		let peer_slot = {
			let Ok(mut per_peer) = self.per_peer.lock() else {
				return None;
			};
			let count = per_peer.entry(ip).or_default();
			if *count >= self.max_per_peer {
				drop(per_peer);
				self.warn_saturated(peer);
				return None;
			}
			*count += 1;
			PeerSlot {
				per_peer: self.per_peer.clone(),
				ip,
			}
		};
		let Ok(slot) = self.slots.clone().try_acquire_owned() else {
			self.warn_saturated(peer);
			return None;
		};
		let delay = self.shutdown.delay_shutdown_token().ok()?;
		Some(IngressPermit {
			_slot: slot,
			_peer_slot: peer_slot,
			_delay: delay,
			shutdown: self.shutdown.clone(),
		})
	}

	fn warn_saturated(&self, peer: SocketAddr) {
		let Ok(mut last) = self.last_saturation_warning.lock() else {
			return;
		};
		if last.is_none_or(|at| at.elapsed() >= SATURATION_WARNING_INTERVAL) {
			*last = Some(Instant::now());
			tracing::warn!(
				listener = self.name,
				%peer,
				"Connection limit reached; closing new connections until existing ones finish"
			);
		}
	}

	#[cfg(test)]
	pub(crate) fn available(&self) -> usize {
		self.slots.available_permits()
	}
}

/// One address's share of a listener's connections.
struct PeerSlot {
	per_peer: Arc<Mutex<HashMap<IpAddr, usize>>>,
	ip: IpAddr,
}

impl Drop for PeerSlot {
	fn drop(&mut self) {
		if let Ok(mut per_peer) = self.per_peer.lock()
			&& let Some(count) = per_peer.get_mut(&self.ip)
		{
			*count -= 1;
			if *count == 0 {
				per_peer.remove(&self.ip);
			}
		}
	}
}

/// Ownership of one admitted connection. Dropping it releases the slot and
/// allows global shutdown to complete.
pub(crate) struct IngressPermit {
	_slot: OwnedSemaphorePermit,
	_peer_slot: PeerSlot,
	_delay: DelayShutdownToken<ShutdownReason>,
	shutdown: ShutdownManager<ShutdownReason>,
}

impl IngressPermit {
	/// Run a connection handler that is cancelled by global shutdown.
	pub(crate) fn spawn<F>(self, handler: F) -> tokio::task::JoinHandle<()>
	where
		F: Future<Output = ()> + Send + 'static,
	{
		tokio::spawn(async move {
			// The handler (and everything it owns) is dropped before the permit.
			let _ = self.shutdown.wrap_cancel(handler).await;
			drop(self);
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn peer(host: u8) -> SocketAddr {
		SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, host)), 1)
	}

	#[tokio::test]
	async fn one_address_cannot_occupy_the_listener() {
		let shutdown = ShutdownManager::new();
		let ingress = Ingress::new("test", 8, shutdown.clone());
		let attacker: SocketAddr = "192.168.1.66:1000".parse().unwrap();
		let mapped: SocketAddr = "[::ffff:192.168.1.66]:1001".parse().unwrap();
		// Eight slots allow two per address; IPv4-mapped IPv6 is the same address.
		let held = [ingress.admit(attacker).unwrap(), ingress.admit(mapped).unwrap()];
		assert!(ingress.admit(attacker).is_none());
		assert!(ingress.admit(mapped).is_none());
		let client = ingress.admit("192.168.1.20:2000".parse().unwrap());
		assert!(client.is_some(), "other clients are still admitted");
		drop(held);
		assert!(ingress.admit(attacker).is_some(), "released with its connections");
		assert!(ingress.per_peer.lock().unwrap().len() <= 1);
	}

	#[tokio::test]
	async fn admission_is_bounded_and_released_with_the_handler() {
		let shutdown = ShutdownManager::new();
		let ingress = Ingress::new("test", 2, shutdown.clone());
		let (release, released) = tokio::sync::oneshot::channel::<()>();
		let first = ingress.admit(peer(1)).unwrap().spawn(async move {
			let _ = released.await;
		});
		let second = ingress.admit(peer(2)).unwrap();
		assert!(ingress.admit(peer(3)).is_none(), "third connection exceeds the limit");
		drop(second);
		assert_eq!(ingress.available(), 1);
		release.send(()).unwrap();
		first.await.unwrap();
		assert_eq!(ingress.available(), 2);
	}

	#[tokio::test]
	async fn shutdown_cancels_stalled_handlers_and_waits_for_them() {
		let shutdown = ShutdownManager::new();
		let ingress = Ingress::new("test", 4, shutdown.clone());
		let owned = Arc::new(());
		for host in 1..=3 {
			let owned = owned.clone();
			ingress.admit(peer(host)).unwrap().spawn(async move {
				let _owned = owned;
				std::future::pending::<()>().await;
			});
		}
		assert_eq!(Arc::strong_count(&owned), 4);
		shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
		tokio::time::timeout(Duration::from_secs(1), shutdown.wait_shutdown_complete())
			.await
			.expect("stalled handlers must not delay shutdown");
		assert_eq!(Arc::strong_count(&owned), 1, "handlers released their state");
		assert!(ingress.admit(peer(4)).is_none(), "no admission after shutdown");
	}
}
