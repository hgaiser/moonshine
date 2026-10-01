//! Receiver-driven, single-frame capture admission. The compositor's existing
//! demand wakeup coalesces in calloop; no polling thread or frame queue.
use super::frame::ExportedFrame;
use smithay::reexports::calloop::ping::{Ping, PingSource, make_ping};
use std::sync::{
	Arc,
	atomic::{AtomicU64, Ordering},
	mpsc,
};
use std::time::{Duration, Instant};

const IDLE: u64 = 0;
const REQUESTED: u64 = 1;
const OCCUPIED: u64 = 2;
const CLOSED: u64 = 3;
const STATE_MASK: u64 = 3;

#[derive(Debug)]
pub(crate) struct CaptureCredit {
	state: Arc<AtomicU64>,
	ticket: u64,
}
impl Drop for CaptureCredit {
	fn drop(&mut self) {
		// An old epoch's completion must never replenish a new epoch's credit.
		let _ = self.state.compare_exchange(
			self.ticket,
			self.ticket & !STATE_MASK,
			Ordering::AcqRel,
			Ordering::Acquire,
		);
	}
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CaptureSendError {
	Full,
	Disconnected,
}

pub(crate) struct CaptureSender {
	tx: mpsc::SyncSender<ExportedFrame>,
	demand_source: Option<PingSource>,
	state: Arc<AtomicU64>,
}
pub(crate) struct CaptureReceiver {
	rx: mpsc::Receiver<ExportedFrame>,
	demand: Option<Ping>,
	state: Arc<AtomicU64>,
}

pub(crate) fn capture_channel() -> (CaptureSender, CaptureReceiver) {
	let (tx, rx) = mpsc::sync_channel(1);
	let state = Arc::new(AtomicU64::new(IDLE));
	let (demand, demand_source) = match make_ping() {
		Ok((ping, source)) => (Some(ping), Some(source)),
		Err(error) => {
			tracing::warn!(%error, "Capture demand wakeup unavailable; using refresh timer");
			(None, None)
		},
	};
	(
		CaptureSender {
			tx,
			demand_source,
			state: state.clone(),
		},
		CaptureReceiver { rx, state, demand },
	)
}
impl CaptureSender {
	pub(crate) fn take_demand_source(&mut self) -> Option<PingSource> {
		self.demand_source.take()
	}
	pub(crate) fn try_acquire(&self) -> Option<CaptureCredit> {
		let requested = self.state.load(Ordering::Acquire);
		if requested & STATE_MASK != REQUESTED {
			return None;
		}
		let ticket = (requested & !STATE_MASK) | OCCUPIED;
		self.state
			.compare_exchange(requested, ticket, Ordering::AcqRel, Ordering::Acquire)
			.ok()?;
		Some(CaptureCredit {
			state: self.state.clone(),
			ticket,
		})
	}
	pub(crate) fn requested(&self) -> bool {
		self.state.load(Ordering::Acquire) & STATE_MASK == REQUESTED
	}
	pub(crate) fn occupied(&self) -> bool {
		self.state.load(Ordering::Acquire) & STATE_MASK == OCCUPIED
	}
	pub(crate) fn try_send(&self, mut frame: ExportedFrame, credit: CaptureCredit) -> Result<(), CaptureSendError> {
		if self.state.load(Ordering::Acquire) != credit.ticket {
			// Reconfiguration invalidated this capture while GLES was rendering.
			frame.consumed.store(true, Ordering::Release);
			return Err(CaptureSendError::Full);
		}
		frame.capture_credit = Some(credit);
		match self.tx.try_send(frame) {
			Ok(()) => Ok(()),
			Err(error) => {
				let (frame, error) = match error {
					mpsc::TrySendError::Full(frame) => (frame, CaptureSendError::Full),
					mpsc::TrySendError::Disconnected(frame) => (frame, CaptureSendError::Disconnected),
				};
				// No consumer GPU access began; composition's fence completed.
				frame.consumed.store(true, Ordering::Release);
				Err(error)
			},
		}
	}
}
impl CaptureReceiver {
	pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<ExportedFrame, mpsc::RecvTimeoutError> {
		self.recv_timeout_if(timeout, true)
	}
	pub(crate) fn recv_timeout_if(
		&self,
		timeout: Duration,
		useful: bool,
	) -> Result<ExportedFrame, mpsc::RecvTimeoutError> {
		let idle = self.state.load(Ordering::Acquire);
		let requested = (idle & !STATE_MASK) | REQUESTED;
		if useful
			&& idle & STATE_MASK == IDLE
			&& self
				.state
				.compare_exchange(idle, requested, Ordering::AcqRel, Ordering::Acquire)
				.is_ok() && let Some(ping) = &self.demand
		{
			ping.ping();
		}
		let started = Instant::now();
		let result = loop {
			match self.rx.recv_timeout(timeout.saturating_sub(started.elapsed())) {
				Ok(frame) => {
					let current_epoch = self.state.load(Ordering::Acquire) & !STATE_MASK;
					if frame
						.capture_credit
						.as_ref()
						.is_some_and(|c| c.ticket & !STATE_MASK == current_epoch)
					{
						break Ok(frame);
					}
					// A send may race reset's drain. Never feed an old extent or HDR
					// epoch into the newly configured encoder.
					frame.consumed.store(true, Ordering::Release);
				},
				Err(error) => break Err(error),
			}
		};
		// Revoke unclaimed demand on timeout, so IDR replay/packet sending cannot
		// accidentally trigger rendering. A claimed credit stays occupied until
		// its frame/consumer releases it, including a render finishing late.
		let _ = self
			.state
			.compare_exchange(requested, requested & !STATE_MASK, Ordering::AcqRel, Ordering::Acquire);
		result
	}
	pub(crate) fn reset(&self) {
		// Only the receiver changes epochs. A producer that raced reset fails
		// its send; already queued frames are released without touching the GPU.
		self.state
			.try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
				(old & STATE_MASK != CLOSED).then_some((old & !STATE_MASK).wrapping_add(4))
			})
			.ok();
		while let Ok(frame) = self.rx.try_recv() {
			frame.consumed.store(true, Ordering::Release);
		}
	}
}
impl Drop for CaptureReceiver {
	fn drop(&mut self) {
		self.state.fetch_or(CLOSED, Ordering::AcqRel);
		while let Ok(frame) = self.rx.try_recv() {
			frame.consumed.store(true, Ordering::Release);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn request(rx: &CaptureReceiver) {
		assert_eq!(
			rx.recv_timeout(Duration::ZERO).unwrap_err(),
			mpsc::RecvTimeoutError::Timeout
		);
	}
	fn pending(rx: &CaptureReceiver) {
		rx.state.fetch_or(REQUESTED, Ordering::Release);
	}
	#[test]
	fn demand_wakeup_is_coalesced_and_only_emitted_when_useful() {
		use smithay::reexports::calloop::EventLoop;
		let (mut tx, rx) = capture_channel();
		let mut event_loop = EventLoop::<u32>::try_new().unwrap();
		event_loop
			.handle()
			.insert_source(tx.take_demand_source().unwrap(), |_, _, events| *events += 1)
			.unwrap();
		let mut events = 0;
		for _ in 0..100 {
			request(&rx);
		}
		event_loop.dispatch(Duration::ZERO, &mut events).unwrap();
		assert_eq!(events, 1);
		assert_eq!(
			rx.recv_timeout_if(Duration::ZERO, false).unwrap_err(),
			mpsc::RecvTimeoutError::Timeout
		);
		event_loop.dispatch(Duration::ZERO, &mut events).unwrap();
		assert_eq!(events, 1);
	}
	#[test]
	fn no_capture_without_demand_and_timeouts_revoke_unused_demand() {
		let (tx, rx) = capture_channel();
		assert!(tx.try_acquire().is_none());
		request(&rx);
		assert!(tx.try_acquire().is_none());
	}
	#[test]
	fn exactly_one_credit_and_failure_returns_it() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		let credit = tx.try_acquire().unwrap();
		for _ in 0..1000 {
			assert!(tx.try_acquire().is_none());
		}
		drop(credit);
		assert!(!tx.occupied());
		pending(&rx);
		assert!(tx.try_acquire().is_some());
	}
	#[test]
	fn claimed_credit_survives_receive_timeout() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		let credit = tx.try_acquire().unwrap();
		request(&rx);
		assert!(tx.occupied());
		assert!(tx.try_acquire().is_none());
		drop(credit);
		assert!(!tx.occupied());
	}
	#[test]
	fn reset_cannot_be_replenished_by_old_credit() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		let old = tx.try_acquire().unwrap();
		rx.reset();
		pending(&rx);
		let current = tx.try_acquire().unwrap();
		drop(old);
		assert!(tx.occupied());
		drop(current);
		assert!(!tx.occupied());
	}
	#[test]
	fn disconnect_is_permanent_even_with_outstanding_credit() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		let credit = tx.try_acquire().unwrap();
		drop(rx);
		drop(credit);
		assert!(tx.try_acquire().is_none());
	}
	fn frame() -> ExportedFrame {
		ExportedFrame {
			planes: vec![],
			capture_credit: None,
			format: 0,
			modifier: 0,
			width: 1920,
			height: 1080,
			created_at: Instant::now(),
			buffer_index: 0,
			consumed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
			color_space: super::super::frame::FrameColorSpace::Srgb,
			hdr_metadata: None,
		}
	}
	#[test]
	fn processing_and_network_hold_credit_after_buffer_consumption() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		tx.try_send(frame(), tx.try_acquire().unwrap()).unwrap();
		let mut f = rx.recv_timeout(Duration::ZERO).unwrap();
		let network_credit = f.capture_credit.take();
		f.consumed.store(true, Ordering::Release);
		drop(f);
		for _ in 0..100 {
			request(&rx);
			assert!(tx.try_acquire().is_none());
		}
		drop(network_credit);
		pending(&rx);
		assert!(tx.try_acquire().is_some());
	}
	#[test]
	fn reset_and_disconnect_release_queued_buffers() {
		for reset in [false, true] {
			let (tx, rx) = capture_channel();
			pending(&rx);
			let f = frame();
			let consumed = f.consumed.clone();
			tx.try_send(f, tx.try_acquire().unwrap()).unwrap();
			if reset {
				rx.reset();
			} else {
				drop(rx);
			}
			assert!(consumed.load(Ordering::Acquire));
			assert!(!tx.occupied());
		}
	}
	#[test]
	fn old_render_cannot_publish_into_new_epoch_or_release_its_credit() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		let old = tx.try_acquire().unwrap();
		rx.reset();
		pending(&rx);
		let current = tx.try_acquire().unwrap();
		let f = frame();
		let consumed = f.consumed.clone();
		assert!(tx.try_send(f, old).is_err());
		assert!(consumed.load(Ordering::Acquire));
		assert!(tx.occupied());
		drop(current);
	}
	#[test]
	fn late_send_that_raced_reset_is_discarded_on_receive() {
		let (tx, rx) = capture_channel();
		pending(&rx);
		let mut f = frame();
		let consumed = f.consumed.clone();
		f.capture_credit = tx.try_acquire();
		rx.reset();
		tx.tx.try_send(f).unwrap();
		request(&rx);
		assert!(consumed.load(Ordering::Acquire));
		assert!(!tx.occupied());
	}
}
