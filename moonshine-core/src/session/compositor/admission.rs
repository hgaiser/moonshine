//! Receiver-driven, single-frame capture admission. The compositor's existing
//! demand wakeup coalesces in calloop; no polling thread or frame queue.
use super::frame::ExportedFrame;
use smithay::reexports::calloop::ping::{Ping, PingSource, make_ping};
use std::sync::{
	Arc,
	atomic::{AtomicU8, AtomicU64, Ordering},
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

/// Late-composition layer kinds a capture consumer can draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverlayCaps(u8);
impl OverlayCaps {
	pub(crate) const NONE: Self = Self(0);
	/// CPU texels (`OverlayContent::Pixels`).
	pub(crate) const PIXELS: Self = Self(1);
	/// Client DMA-BUFs (`OverlayContent::Dmabuf`).
	pub(crate) const DMABUF: Self = Self(2);
	pub(crate) const fn union(self, other: Self) -> Self {
		Self(self.0 | other.0)
	}
	pub(crate) fn contains(self, other: Self) -> bool {
		other.0 != 0 && self.0 & other.0 == other.0
	}
	/// Whether every layer of `overlays` can be drawn.
	pub(crate) fn draws(self, overlays: &super::frame::FrameOverlays) -> bool {
		overlays.iter().flatten().all(|layer| {
			self.contains(match layer.content {
				super::frame::OverlayContent::Pixels(_) => Self::PIXELS,
				super::frame::OverlayContent::Dmabuf { .. } => Self::DMABUF,
			})
		})
	}
}

pub(crate) struct CaptureSender {
	context: Arc<std::sync::OnceLock<pixelforge::VideoContext>>,
	overlay_caps: Arc<AtomicU8>,
	tx: mpsc::SyncSender<ExportedFrame>,
	demand_source: Option<PingSource>,
	state: Arc<AtomicU64>,
}
pub(crate) struct CaptureReceiver {
	context: Arc<std::sync::OnceLock<pixelforge::VideoContext>>,
	/// Whether the active consumer composites `ExportedFrame::overlay`.
	overlay_caps: Arc<AtomicU8>,
	rx: mpsc::Receiver<ExportedFrame>,
	demand: Option<Ping>,
	state: Arc<AtomicU64>,
}

pub(crate) fn capture_channel() -> (CaptureSender, CaptureReceiver) {
	let (tx, rx) = mpsc::sync_channel(1);
	let state = Arc::new(AtomicU64::new(IDLE));
	let context = Arc::new(std::sync::OnceLock::new());
	let overlay_caps = Arc::new(AtomicU8::new(0));
	let (demand, demand_source) = match make_ping() {
		Ok((ping, source)) => (Some(ping), Some(source)),
		Err(error) => {
			tracing::warn!(%error, "Capture demand wakeup unavailable; using refresh timer");
			(None, None)
		},
	};
	(
		CaptureSender {
			context: context.clone(),
			overlay_caps: overlay_caps.clone(),
			tx,
			demand_source,
			state: state.clone(),
		},
		CaptureReceiver {
			context,
			overlay_caps,
			rx,
			state,
			demand,
		},
	)
}
impl CaptureSender {
	pub(crate) fn set_context(&self, context: pixelforge::VideoContext) {
		assert!(
			self.context.set(context).is_ok(),
			"capture context initialized exactly once"
		);
	}
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
	/// Which late-composition layer kinds frames may carry. A frame captured
	/// before the consumer withdrew support is dropped by that consumer, never
	/// encoded without its layers.
	pub(crate) fn overlay_caps(&self) -> OverlayCaps {
		OverlayCaps(self.overlay_caps.load(Ordering::Acquire))
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
	pub(crate) fn context(&self) -> Result<pixelforge::VideoContext, String> {
		self.context
			.get()
			.cloned()
			.ok_or_else(|| "Capture GPU context not initialized; compositor must be ready before streaming".into())
	}
	/// Declare which frame overlay layers this consumer composites.
	pub(crate) fn set_overlay_caps(&self, caps: OverlayCaps) {
		self.overlay_caps.store(caps.0, Ordering::Release);
	}
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
	/// Grant capture demand without waiting, as a blocked `recv_timeout` would.
	#[cfg(test)]
	pub(crate) fn request_for_test(&self) {
		self.state.fetch_or(REQUESTED, Ordering::Release);
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
		self.overlay_caps.store(0, Ordering::Release);
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
		rx.request_for_test();
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
	fn overlay_support_is_declared_by_the_consumer_and_withdrawn_on_drop() {
		let (tx, rx) = capture_channel();
		assert_eq!(
			tx.overlay_caps(),
			OverlayCaps::NONE,
			"no consumer composites overlays by default"
		);
		rx.set_overlay_caps(OverlayCaps::PIXELS.union(OverlayCaps::DMABUF));
		assert!(tx.overlay_caps().contains(OverlayCaps::DMABUF));
		rx.set_overlay_caps(OverlayCaps::PIXELS);
		assert!(!tx.overlay_caps().contains(OverlayCaps::DMABUF));
		assert!(tx.overlay_caps().contains(OverlayCaps::PIXELS));
		drop(rx);
		assert_eq!(
			tx.overlay_caps(),
			OverlayCaps::NONE,
			"a dropped consumer cannot draw overlays"
		);
	}

	#[test]
	fn overlay_caps_require_every_layer_kind() {
		use crate::session::compositor::frame::{FrameOverlay, OverlayContent, OverlayFormat, OverlayImage};
		let pixels = FrameOverlay {
			content: OverlayContent::Pixels(Arc::new(OverlayImage {
				generation: 1,
				width: 1,
				height: 1,
				format: OverlayFormat::Rgba8,
				pixels: vec![0; 4].into_boxed_slice(),
			})),
			x: 0,
			y: 0,
			opacity: 1.0,
		};
		let none = Default::default();
		assert!(OverlayCaps::NONE.draws(&none), "a frame without layers needs nothing");
		let cursor_only = [None, Some(pixels.clone())];
		assert!(OverlayCaps::PIXELS.draws(&cursor_only));
		assert!(!OverlayCaps::NONE.draws(&cursor_only));
		assert!(!OverlayCaps::DMABUF.draws(&cursor_only));
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
		ExportedFrame::for_test()
	}
	#[test]
	fn composition_fence_completion_does_not_restart_pacing_window() {
		let mut frame = frame();
		let start = frame.created_at;
		assert_eq!(frame.pacing_origin(), start);
		frame.composition_started_at = Some(start);
		frame.created_at = start + Duration::from_millis(2);
		assert_eq!(frame.pacing_origin(), start);
		assert_eq!(frame.created_at, start + Duration::from_millis(2));
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
