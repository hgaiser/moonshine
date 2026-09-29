use serde::{Deserialize, Serialize};

/// Server policy for video Reed-Solomon protection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FecMode {
	/// Never emit parity, even when the client requests a minimum.
	Off,
	/// Keep the configured percentage (subject to wire representability).
	#[default]
	Fixed,
	/// Adjust protection from Moonlight's per-block FEC status reports.
	Auto,
}

/// One decoded Sunshine `SS_FRAME_FEC_STATUS` report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameFecStatus {
	pub serial: u64,
	pub frame_index: u32,
	pub highest_received_sequence_number: u16,
	pub next_contiguous_sequence_number: u16,
	pub missing_packets_before_highest: u16,
	pub total_data_packets: u16,
	pub total_parity_packets: u16,
	pub received_data_packets: u16,
	pub received_parity_packets: u16,
	pub fec_percentage: u8,
	pub block_index: u8,
	pub block_count: u8,
}

/// Deliberately small step controller. Decisions are made only after a complete
/// sample window and are consumed by the packetizer at the next frame boundary.
pub(crate) struct FecController {
	mode: FecMode,
	current: u8,
	min: u8,
	max: u8,
	window_reports: u32,
	window_missing_data: u32,
	window_recovered_blocks: u32,
	window_unrecoverable_blocks: u32,
	clean_windows: u8,
	last_serial: u64,
	pyrowave: bool,
	last_max_warning: Option<std::time::Instant>,
}

impl FecController {
	const REPORTS_PER_WINDOW: u32 = 120;

	pub(crate) fn new(mode: FecMode, configured: u8, min: u8, max: u8, pyrowave: bool) -> Self {
		let max = max.max(min);
		let current = match mode {
			FecMode::Off => 0,
			FecMode::Fixed => configured,
			FecMode::Auto => configured.clamp(min, max),
		};
		Self {
			mode,
			current,
			min,
			max,
			window_reports: 0,
			window_missing_data: 0,
			window_recovered_blocks: 0,
			window_unrecoverable_blocks: 0,
			clean_windows: 0,
			last_serial: 0,
			pyrowave,
			last_max_warning: None,
		}
	}

	pub(crate) fn percentage(&self) -> u8 {
		self.current
	}

	pub(crate) fn minimum_packets(&self, requested: u32) -> u32 {
		if self.mode == FecMode::Off { 0 } else { requested }
	}

	pub(crate) fn observe(&mut self, report: FrameFecStatus) {
		if self.mode != FecMode::Auto || report.serial == 0 || report.serial == self.last_serial {
			return;
		}
		self.last_serial = report.serial;
		let missing_data = report.total_data_packets.saturating_sub(report.received_data_packets) as u32;
		let usable_parity = report.received_parity_packets as u32;
		self.window_reports += 1;
		self.window_missing_data += missing_data;
		if missing_data > 0 {
			if missing_data <= usable_parity {
				self.window_recovered_blocks += 1;
			} else {
				self.window_unrecoverable_blocks += 1;
			}
		}
		if self.window_reports >= Self::REPORTS_PER_WINDOW {
			self.finish_window();
		}
	}

	fn finish_window(&mut self) {
		let previous = self.current;
		tracing::debug!(
			effective_fec_percentage = self.current,
			reports = self.window_reports,
			missing_data_packets = self.window_missing_data,
			recovered_blocks = self.window_recovered_blocks,
			unrecoverable_blocks = self.window_unrecoverable_blocks,
			"Video FEC feedback window"
		);
		if self.window_unrecoverable_blocks > 0 {
			self.current = self.current.saturating_add(if self.pyrowave { 5 } else { 4 });
			self.clean_windows = 0;
		} else if self.window_recovered_blocks >= 3 || self.window_missing_data >= 6 {
			self.current = self.current.saturating_add(2);
			self.clean_windows = 0;
		} else if self.window_recovered_blocks > 0 || self.window_missing_data > 0 {
			self.current = self.current.saturating_add(1);
			self.clean_windows = 0;
		} else {
			self.clean_windows = self.clean_windows.saturating_add(1);
			// Intra-only PyroWave can tolerate an isolated dropped frame, so its
			// clean-link overhead decays sooner than predictive codecs.
			let clean_windows_before_decay = if self.pyrowave { 2 } else { 5 };
			if self.clean_windows >= clean_windows_before_decay {
				self.current = self.current.saturating_sub(1);
				self.clean_windows = 0;
			}
		}
		self.current = self.current.clamp(self.min, self.max);
		if self.current != previous {
			tracing::info!(
				previous_fec_percentage = previous,
				effective_fec_percentage = self.current,
				missing_data_packets = self.window_missing_data,
				recovered_blocks = self.window_recovered_blocks,
				unrecoverable_blocks = self.window_unrecoverable_blocks,
				"Adjusted automatic video FEC at a frame boundary"
			);
		}
		if self.current == self.max
			&& self.window_unrecoverable_blocks > 0
			&& self
				.last_max_warning
				.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(30))
		{
			tracing::warn!(
				effective_fec_percentage = self.current,
				"Video FEC is at its configured maximum while blocks remain unrecoverable"
			);
			self.last_max_warning = Some(std::time::Instant::now());
		}
		self.window_reports = 0;
		self.window_missing_data = 0;
		self.window_recovered_blocks = 0;
		self.window_unrecoverable_blocks = 0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn report(serial: u64, missing: u16, parity: u16) -> FrameFecStatus {
		FrameFecStatus {
			serial,
			total_data_packets: 100,
			received_data_packets: 100 - missing,
			received_parity_packets: parity,
			..Default::default()
		}
	}

	#[test]
	fn off_overrides_client_minimum() {
		let controller = FecController::new(FecMode::Off, 20, 0, 25, true);
		assert_eq!(controller.percentage(), 0);
		assert_eq!(controller.minimum_packets(2), 0);
	}

	#[test]
	fn automatic_fec_reacts_and_decays_with_hysteresis() {
		let mut controller = FecController::new(FecMode::Auto, 5, 0, 25, true);
		for serial in 1..=120 {
			controller.observe(report(serial, 3, 1));
		}
		assert_eq!(controller.percentage(), 10);
		for serial in 121..=240 {
			controller.observe(report(serial, 0, 0));
		}
		assert_eq!(controller.percentage(), 10);
		for serial in 241..=360 {
			controller.observe(report(serial, 0, 0));
		}
		assert_eq!(controller.percentage(), 9);
	}

	#[test]
	fn automatic_fec_is_clamped() {
		let mut controller = FecController::new(FecMode::Auto, 24, 5, 25, true);
		for serial in 1..=240 {
			controller.observe(report(serial, 10, 0));
		}
		assert_eq!(controller.percentage(), 25);
	}
}
