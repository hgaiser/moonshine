use std::time::{Duration, Instant};

use mio_timerfd::{ClockId, TimerFd};
use tokio::io::unix::AsyncFd;

/// Approximate bytes added to each UDP payload on a VLAN Ethernet path:
/// IPv4 + UDP + Ethernet/VLAN/FCS + preamble/SFD + inter-packet gap.
const WIRE_OVERHEAD_BYTES: usize = 70;

/// Keep each GSO burst close to half a millisecond of on-wire data. The kernel
/// still segments several shards per send, while the pacer prevents a complete
/// encoded frame from entering the network queue at once.
const TARGET_BURST_DURATION: Duration = Duration::from_micros(500);

pub(super) struct Pacer {
	timer: AsyncFd<TimerFd>,
	target_wire_rate_bps: u64,
	frame_interval: Duration,
	frame_budget_percent: u8,
}

pub(super) struct BatchSchedule {
	pub segments_per_send: usize,
	pub deadline_clamped: bool,
	pub pacing_interval: Duration,
	next_send: Instant,
	wire_rate_bps: u64,
	wire_bytes_per_segment: usize,
}

impl Pacer {
	pub fn new(
		client_bitrate_bps: usize,
		fps: u32,
		headroom_percent: u16,
		frame_budget_percent: u8,
	) -> std::io::Result<Self> {
		let timer = TimerFd::new(ClockId::Monotonic)?;
		let timer = AsyncFd::new(timer)?;
		let target_wire_rate_bps = wire_rate_with_headroom(client_bitrate_bps, headroom_percent);
		Ok(Self {
			timer,
			target_wire_rate_bps,
			frame_interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
			frame_budget_percent: frame_budget_percent.clamp(1, 100),
		})
	}

	pub fn frame_budget(&self) -> Duration {
		self.frame_interval
			.mul_f64(f64::from(self.frame_budget_percent) / 100.0)
	}

	pub fn frame_interval(&self) -> Duration {
		self.frame_interval
	}

	pub fn schedule_batch(&self, shard_size: usize, shard_count: usize, max_segments_per_send: usize) -> BatchSchedule {
		batch_schedule(
			shard_size,
			shard_count,
			max_segments_per_send,
			self.target_wire_rate_bps,
			self.frame_budget(),
		)
	}

	pub async fn wait(&mut self, schedule: &BatchSchedule) -> std::io::Result<()> {
		loop {
			let Some(delay) = schedule.next_send.checked_duration_since(Instant::now()) else {
				return Ok(());
			};
			self.timer.get_mut().set_timeout(&delay)?;
			let mut ready = self.timer.readable_mut().await?;
			match ready.get_inner().read()? {
				0 => ready.clear_ready(),
				_ => {
					// Reading a one-shot timerfd consumes the expiration, so the
					// descriptor is no longer readable until it is rearmed.
					ready.clear_ready();
					return Ok(());
				},
			}
		}
	}
}

fn wire_rate_with_headroom(client_bitrate_bps: usize, headroom_percent: u16) -> u64 {
	(client_bitrate_bps as u128)
		.saturating_mul(u128::from(headroom_percent))
		.div_ceil(100)
		.min(u128::from(u64::MAX)) as u64
}

impl BatchSchedule {
	pub fn advance(&mut self, segment_count: usize) {
		let wire_bits = (self.wire_bytes_per_segment.saturating_mul(segment_count) as u128) * 8;
		let interval_ns = wire_bits
			.saturating_mul(1_000_000_000)
			.div_ceil(u128::from(self.wire_rate_bps))
			.min(u128::from(u64::MAX)) as u64;
		self.next_send += Duration::from_nanos(interval_ns);
	}
}

fn batch_schedule(
	shard_size: usize,
	shard_count: usize,
	max_segments_per_send: usize,
	target_wire_rate_bps: u64,
	frame_budget: Duration,
) -> BatchSchedule {
	let wire_bytes_per_segment = shard_size.saturating_add(WIRE_OVERHEAD_BYTES);
	let total_wire_bits = (wire_bytes_per_segment.saturating_mul(shard_count) as u128) * 8;
	let required_wire_rate_bps = total_wire_bits
		.saturating_mul(1_000_000_000)
		.div_ceil(frame_budget.as_nanos().max(1))
		.min(u128::from(u64::MAX)) as u64;
	let wire_rate_bps = target_wire_rate_bps.max(required_wire_rate_bps).max(1);
	let deadline_clamped = required_wire_rate_bps > target_wire_rate_bps;

	let burst_wire_bytes = (u128::from(wire_rate_bps)).saturating_mul(TARGET_BURST_DURATION.as_nanos()) / 8_000_000_000;
	let segments_per_send = (burst_wire_bytes / wire_bytes_per_segment.max(1) as u128)
		.max(1)
		.min(max_segments_per_send.max(1) as u128) as usize;
	let pacing_interval_ns = (wire_bytes_per_segment.saturating_mul(segments_per_send) as u128)
		.saturating_mul(8_000_000_000)
		.div_ceil(u128::from(wire_rate_bps))
		.min(u128::from(u64::MAX)) as u64;

	BatchSchedule {
		segments_per_send,
		deadline_clamped,
		pacing_interval: Duration::from_nanos(pacing_interval_ns),
		next_send: Instant::now(),
		wire_rate_bps,
		wire_bytes_per_segment,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn derives_rate_from_client_bitrate_with_headroom() {
		assert_eq!(wire_rate_with_headroom(60_000_000, 200), 120_000_000);
		assert_eq!(wire_rate_with_headroom(60_000_000, 0), 0);
	}

	#[test]
	fn schedules_small_gso_bursts_at_derived_rate() {
		let schedule = batch_schedule(1392, 100, 46, 120_000_000, Duration::from_millis(13));
		assert_eq!(schedule.segments_per_send, 5);
		assert!(!schedule.deadline_clamped);
		assert_eq!(schedule.pacing_interval, Duration::from_nanos(487_334));
	}

	#[test]
	fn accelerates_large_frames_to_meet_budget() {
		let schedule = batch_schedule(1392, 500, 46, 120_000_000, Duration::from_millis(13));
		assert_eq!(schedule.segments_per_send, 19);
		assert!(schedule.deadline_clamped);
		assert!(schedule.pacing_interval <= TARGET_BURST_DURATION);
	}

	#[test]
	fn respects_gso_segment_limit() {
		let schedule = batch_schedule(1392, 100, 3, 120_000_000, Duration::from_millis(13));
		assert_eq!(schedule.segments_per_send, 3);
	}
}
