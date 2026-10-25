use std::time::{Duration, Instant};

/// Share of the send rate a cut keeps, in percent.
const DECREASE_PERCENT: u64 = 70;

/// Least time between two cuts: one burst often loses several frames, and the
/// client's reports for all of them describe the same congestion.
const HOLDOFF: Duration = Duration::from_millis(100);

/// How fast the send rate starts probing back up after a cut, in percent of
/// the bitrate per second. The probe speeds up by this much again for every
/// half second without a cut.
const INCREASE_PERCENT: u64 = 20;

/// Send rate, in percent of the bitrate, below which captures are skipped to
/// fit.
const SKIP_BELOW_PERCENT: u64 = 110;

/// Lowest send rate, in percent of the bitrate.
const MIN_PERCENT: u64 = 25;

/// Adapts the stream's frame rate to the client's loss reports.
///
/// The encoder's bitrate is fixed for the session, and nothing below the
/// sender sees a link further away get slower than the stream: its packets
/// are dropped there, and every frame breaks. The client's reports (IDR and
/// reference-frame-invalidation requests) are the only signal of that, so
/// this keeps an estimate of the rate the link carries: each report cuts it
/// (at most once per [`HOLDOFF`]), and it probes back up while the client
/// stays quiet. While the estimate is below the bitrate, captures are skipped
/// in proportion; the encoder's rate control budgets per frame, so skipping
/// frames lowers the bitrate to fit.
///
/// The client's FEC status reports refine this: a frame it repaired using
/// more than half its parity is as good as lost and cuts the estimate, while
/// lighter repair only pauses probing, so the estimate backs off before
/// frames break without reacting to random loss.
pub(super) struct LossAdaptation {
	/// Estimated rate the link carries, in bits per second.
	rate: u64,
	min_rate: u64,
	max_rate: u64,
	skip_below: u64,
	increase_per_second: u64,
	last_decrease: Option<Instant>,
	/// When light repair last paused probing.
	last_warning: Option<Instant>,
	last_update: Instant,
	/// Thousandths of a capture the rate allows; an admitted capture spends
	/// 1000.
	credit: u64,
}

impl LossAdaptation {
	/// Start with the link carrying `max_percent` of `bitrate` (bits per
	/// second).
	pub(super) fn new(bitrate: usize, max_percent: u32, now: Instant) -> Self {
		let bitrate = u64::try_from(bitrate).unwrap_or(u64::MAX);
		let max_rate = bitrate / 100 * u64::from(max_percent.max(1));
		Self {
			rate: max_rate,
			min_rate: bitrate / 100 * MIN_PERCENT,
			max_rate,
			skip_below: (bitrate / 100 * SKIP_BELOW_PERCENT).max(1),
			increase_per_second: bitrate / 100 * INCREASE_PERCENT,
			last_decrease: None,
			last_warning: None,
			last_update: now,
			credit: 0,
		}
	}

	/// Cut the rate for a loss the client reported at `now`.
	pub(super) fn on_loss(&mut self, now: Instant) {
		self.update(now);
		if self
			.last_decrease
			.is_some_and(|last| now.saturating_duration_since(last) < HOLDOFF)
		{
			return;
		}
		self.rate = (self.rate / 100 * DECREASE_PERCENT).max(self.min_rate);
		self.last_decrease = Some(now);
	}

	/// React to the client repairing `missing` packets of a frame with
	/// `parity` parity packets at `now` (or losing it): more than half the
	/// parity is a cut, less pauses probing for [`HOLDOFF`].
	pub(super) fn on_fec_status(&mut self, now: Instant, missing: u16, parity: u16) {
		if u32::from(missing) * 2 > u32::from(parity) {
			self.on_loss(now);
		} else {
			self.update(now);
			self.last_warning = Some(now);
		}
	}

	/// Whether to encode a capture at `now`, or skip it to fit the rate.
	pub(super) fn admit(&mut self, now: Instant) -> bool {
		self.update(now);
		let allowed = (self.rate.saturating_mul(1000) / self.skip_below).min(1000);
		self.credit = (self.credit + allowed).min(2000);
		if self.credit < 1000 {
			return false;
		}
		self.credit -= 1000;
		true
	}

	/// Share of captures the rate allows, in percent.
	pub(super) fn admitted_percent(&self) -> u64 {
		(self.rate.saturating_mul(100) / self.skip_below).min(100)
	}

	/// Raise the rate for the time since the last update.
	fn update(&mut self, now: Instant) {
		let warned = self
			.last_warning
			.is_some_and(|last| now.saturating_duration_since(last) < HOLDOFF);
		let elapsed = if warned {
			Duration::ZERO
		} else {
			now.saturating_duration_since(self.last_update)
		};
		let calm = self
			.last_decrease
			.max(self.last_warning)
			.map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
		// 1 + 2 per second of calm, in thousandths.
		let speedup = 1000 + 2 * calm.as_millis();
		let increase = u128::from(self.increase_per_second) * elapsed.as_micros() * speedup / 1_000_000_000;
		self.rate = self
			.rate
			.saturating_add(u64::try_from(increase).unwrap_or(u64::MAX))
			.min(self.max_rate);
		self.last_update = self.last_update.max(now);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const BITRATE: usize = 20_000_000;

	fn admitted(adaptation: &mut LossAdaptation, now: Instant, captures: usize) -> usize {
		(0..captures).filter(|_| adaptation.admit(now)).count()
	}

	#[test]
	fn admits_every_capture_without_loss() {
		let now = Instant::now();
		let mut adaptation = LossAdaptation::new(BITRATE, 400, now);
		assert_eq!(admitted(&mut adaptation, now, 60), 60);
	}

	#[test]
	fn reports_in_one_burst_cut_once() {
		let now = Instant::now();
		let mut adaptation = LossAdaptation::new(BITRATE, 400, now);
		adaptation.on_loss(now);
		adaptation.on_loss(now + Duration::from_millis(50));
		// One cut to 70%, plus 50 ms of probing back up.
		assert!((56_000_000..57_000_000).contains(&adaptation.rate));
	}

	#[test]
	fn sustained_loss_skips_captures_to_fit() {
		let start = Instant::now();
		let mut adaptation = LossAdaptation::new(BITRATE, 400, start);
		// A report every 150 ms for 2 s, as on a link slower than the stream.
		for i in 0..14 {
			adaptation.on_loss(start + Duration::from_millis(150 * i));
		}
		let now = start + Duration::from_millis(1950);
		assert!(adaptation.admitted_percent() < 100);
		let admitted = admitted(&mut adaptation, now, 60);
		assert!(admitted < 60, "admitted {admitted} of 60");
		// Never below the floor: a quarter of the bitrate, under 1.1x.
		assert!(admitted >= 60 * 25 / 110);
	}

	#[test]
	fn heavy_fec_repair_cuts_and_light_repair_holds() {
		let start = Instant::now();
		let mut adaptation = LossAdaptation::new(BITRATE, 200, start);
		adaptation.on_loss(start);
		let later = start + Duration::from_secs(1);
		adaptation.update(later);
		let probed = adaptation.rate;
		// Repairing 2 of 10 parity packets holds the estimate...
		adaptation.on_fec_status(later, 2, 10);
		adaptation.update(later + Duration::from_millis(50));
		assert_eq!(adaptation.rate, probed);
		// ...repairing 6 of 10 cuts it.
		adaptation.on_fec_status(later + Duration::from_millis(60), 6, 10);
		assert!(adaptation.rate < probed);
	}

	#[test]
	fn probes_back_up_once_the_client_is_quiet() {
		let start = Instant::now();
		let mut adaptation = LossAdaptation::new(BITRATE, 400, start);
		for i in 0..14 {
			adaptation.on_loss(start + Duration::from_millis(150 * i));
		}
		let quiet = start + Duration::from_millis(1950) + Duration::from_secs(2);
		assert_eq!(admitted(&mut adaptation, quiet, 60), 60);
	}
}
