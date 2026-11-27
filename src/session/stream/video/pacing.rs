//! Userspace UDP packet pacing and bounded GSO bursts.
//!
//! Without pacing, Moonshine sends a frame's shards in up to 64-segment (~94 KiB on wire) UDP GSO
//! super-packets at NIC line rate (e.g. 2.5 GbE). When a faster host streams to a slower or
//! shallow-buffered link (such as a 2.5 GbE -> 1 GbE switch without 802.3x flow control, a
//! built-in 100 Mb/s TV port, or a USB 2.0 Ethernet adapter with a small on-chip RX FIFO),
//! frame bursts can overflow buffers and drop tail packets well below the link's average
//! throughput.
//!
//! When enabled for a paired client, [`PacketPacer`] limits GSO bursts to
//! [`PACED_MAX_GSO_SEGMENTS`] datagrams and asynchronously paces chunks at 1.5x the stream's
//! target network bitrate using a Linux `timerfd` wrapped in Tokio's `AsyncFd` (zero
//! busy-spinning, works with any egress qdisc, and cancels cleanly on session shutdown).
//!
//! ## Interaction with the pipeline's send queue
//!
//! Pacing runs in the async packet handler before each socket send. Because each queued
//! [`super::pipeline::PacketBatch`] holds its in-flight slot until dequeued, any sustained pacing
//! backlog fills the bounded `MAX_FRAMES_IN_FLIGHT` channel and causes the encoding thread to drop
//! new captures pre-encode (preserving the P-frame reference chain) rather than accumulating
//! unbounded latency.

use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::time::{
	Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create, timerfd_settime,
};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Pacing headroom multiplier (`3 / 2 = 1.5x`) applied to the stream's target bitrate.
///
/// Leaves 50% headroom so normal frames finish transmitting well inside one frame interval while
/// keeping burst rates safely below shallow-buffered links (for example, 150 Mb/s -> 225 Mb/s on
/// a ~250 Mb/s USB 2.0 GbE adapter, or 65 Mb/s -> 97.5 Mb/s on a 100 Mb/s TV Ethernet port).
const PACING_MULTIPLIER_NUM: u128 = 3;
const PACING_MULTIPLIER_DEN: u128 = 2;

/// Minimum pacing rate (10 Mb/s = 1,250,000 bytes/s) so very low bitrate streams do not stall
/// keyframes unnecessarily.
const MIN_PACING_BYTES_PER_SEC: u64 = 1_250_000;

/// Maximum UDP segments per GSO `sendmsg` while pacing is active (~5.8 KiB on the wire at
/// 1408-byte payloads + 66-byte Ethernet/IPv4/UDP framing), staying well below typical 16–32 KiB
/// USB Ethernet RX FIFOs while keeping syscall overhead low.
pub(crate) const PACED_MAX_GSO_SEGMENTS: usize = 4;

/// Skip arming `timerfd` for tiny waits; the remainder carries into the next chunk's deadline.
const MIN_TIMER_WAIT: Duration = Duration::from_micros(20);

/// Maximum burst of wire bytes allowed when catching up after kernel timer slack or OS
/// scheduling jitter. Stays below typical USB 2.0 Ethernet RX FIFOs (~16–32 KiB) while letting
/// the pacer hold its target average rate under normal timer slack.
const MAX_BURST_WIRE_BYTES: usize = 12 * 1024;

/// Minimum catch-up window so larger chunks still absorb normal Linux timer slack (~50 us).
const MIN_CATCHUP_WINDOW: Duration = Duration::from_micros(250);

/// Ethernet L1 + L2 wire overhead per frame: preamble/SFD (8) + inter-frame gap (12) + header (14)
/// + FCS (4) = 38 bytes.
const ETHERNET_WIRE_OVERHEAD_BYTES: usize = 38;

/// Ethernet (38) + IPv4 (20) + UDP (8) wire bytes per datagram.
const IPV4_WIRE_OVERHEAD_BYTES: usize = ETHERNET_WIRE_OVERHEAD_BYTES + 20 + 8;

/// Ethernet (38) + IPv6 (40) + UDP (8) wire bytes per datagram.
const IPV6_WIRE_OVERHEAD_BYTES: usize = ETHERNET_WIRE_OVERHEAD_BYTES + 40 + 8;

/// Target wire pacing rate in bytes/sec derived automatically from the stream's configured
/// bitrate, encoded video bitrate, and FEC percentage (`1.5x`, floored at 10 Mb/s).
///
/// Normally `configured_bitrate_bps` already budgets for video, FEC, audio, and packet overhead
/// (see [`super::video_bitrate_within`]). Above 80% FEC, `video_bitrate_within` stops subtracting
/// parity from the video encoder bitrate, so the actual video + FEC wire rate exceeds
/// `configured_bitrate_bps`. Taking the maximum of `configured_bitrate_bps` and
/// `encoded_bitrate_bps * (1 + fec_percentage / 100)` keeps 50% pacing headroom over what the
/// stream actually sends across the full 0–100% FEC range.
pub(crate) fn pacing_bytes_per_sec(
	configured_bitrate_bps: Option<usize>,
	encoded_bitrate_bps: usize,
	fec_percentage: u8,
) -> u64 {
	let video_with_fec_bps = (encoded_bitrate_bps as u128).saturating_mul(100 + u128::from(fec_percentage)) / 100;
	let base_bps = (configured_bitrate_bps.unwrap_or(0) as u128).max(video_with_fec_bps);
	let bytes_per_sec = base_bps.saturating_mul(PACING_MULTIPLIER_NUM) / (PACING_MULTIPLIER_DEN * 8);
	bytes_per_sec.clamp(u128::from(MIN_PACING_BYTES_PER_SEC), u128::from(u64::MAX)) as u64
}

/// Whether packets to `addr` travel on the wire as IPv6 (dual-stack IPv4-mapped IPv6 addresses
/// like `::ffff:192.168.1.2` go out as IPv4 packets).
pub(crate) fn is_ipv6_wire(addr: SocketAddr) -> bool {
	match addr {
		SocketAddr::V4(_) => false,
		SocketAddr::V6(v6) => v6.ip().to_ipv4_mapped().is_none(),
	}
}

/// Wire bytes sent for a chunk of `chunk_len` UDP payload bytes split into `shard_size` datagrams,
/// including Ethernet L1/L2, IP, and UDP framing.
pub(crate) fn chunk_wire_bytes(chunk_len: usize, shard_size: usize, is_ipv6: bool) -> usize {
	let overhead = if is_ipv6 {
		IPV6_WIRE_OVERHEAD_BYTES
	} else {
		IPV4_WIRE_OVERHEAD_BYTES
	};
	let shard_count = chunk_len.div_ceil(shard_size.max(1));
	chunk_len.saturating_add(shard_count.saturating_mul(overhead))
}

/// Duration required to transmit `wire_bytes` at `bytes_per_sec`.
fn tx_duration(wire_bytes: usize, bytes_per_sec: NonZeroU64) -> Duration {
	let nanos = (wire_bytes as u128).saturating_mul(1_000_000_000) / u128::from(bytes_per_sec.get());
	Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

/// Maximum catch-up lag allowed after a late timer wake so the resulting burst never exceeds
/// [`MAX_BURST_WIRE_BYTES`] while still absorbing normal Linux timer slack.
fn max_catchup_window(wire_bytes: usize, bytes_per_sec: NonZeroU64, tx_dur: Duration) -> Duration {
	tx_duration(MAX_BURST_WIRE_BYTES.saturating_sub(wire_bytes), bytes_per_sec).max(tx_dur.min(MIN_CATCHUP_WINDOW))
}

/// Pure, clock-injected pacing schedule shared by [`PacketPacer`] and deterministic unit tests.
struct PacingSchedule {
	bytes_per_sec: NonZeroU64,
	next_send_at: Option<Instant>,
	/// Remaining bounded catch-up credit `(recorded_at, credit)` from the last late timer wake.
	/// Decays 1:1 with wall time elapsed since `recorded_at`, so an idle gap between frames never
	/// banks burst credit into the next frame.
	catchup: Option<(Instant, Duration)>,
}

impl PacingSchedule {
	fn new(bytes_per_sec: NonZeroU64) -> Self {
		Self {
			bytes_per_sec,
			next_send_at: None,
			catchup: None,
		}
	}

	fn bytes_per_sec(&self) -> u64 {
		self.bytes_per_sec.get()
	}

	/// Decide whether the caller must sleep before sending `wire_bytes` at `now`.
	///
	/// Returns `Some(wait)` when the caller should sleep for `wait` and then call
	/// [`Self::complete_delayed_send`] (or [`Self::complete_failed_wait`] if the timer failed).
	/// Returns `None` when the chunk can be sent immediately (the schedule is already advanced).
	fn plan_send(&mut self, now: Instant, wire_bytes: usize) -> Option<Duration> {
		let tx_dur = tx_duration(wire_bytes, self.bytes_per_sec);

		if let Some(deadline) = self.next_send_at
			&& deadline > now
		{
			let wait = deadline - now;
			if wait >= MIN_TIMER_WAIT {
				return Some(wait);
			}
			// Tiny wait: send immediately and let the next chunk absorb the remainder.
			self.catchup = None;
			self.next_send_at = Some(deadline + tx_dur);
			return None;
		}

		if let Some((recorded_at, credit)) = self.catchup {
			let remaining = credit.saturating_sub(now.saturating_duration_since(recorded_at));
			if !remaining.is_zero() {
				// Drain remaining catch-up credit from the last late timer wake without banking
				// any elapsed wall time.
				let base = now.checked_sub(remaining).unwrap_or(now);
				let next = base + tx_dur;
				self.next_send_at = Some(next);
				self.catchup = (next <= now).then(|| (now, now.saturating_duration_since(next)));
				return None;
			}
		}

		// First chunk or idle gap between frames: send immediately without banking idle time as
		// burst credit for the next keyframe.
		self.catchup = None;
		self.next_send_at = Some(now + tx_dur);
		None
	}

	/// Advance the schedule after a timer wait returned at `after`.
	fn complete_delayed_send(&mut self, after: Instant, wire_bytes: usize) {
		let tx_dur = tx_duration(wire_bytes, self.bytes_per_sec);
		let max_catchup = max_catchup_window(wire_bytes, self.bytes_per_sec, tx_dur);
		let deadline = self.next_send_at.unwrap_or(after);
		// Advance from `deadline` so normal Linux timer slack and wake jitter do not permanently
		// erode average throughput, while capping catch-up so a long OS scheduling stall cannot
		// trigger an unbounded burst.
		let base = deadline.max(after.checked_sub(max_catchup).unwrap_or(deadline));
		let next = base + tx_dur;
		self.next_send_at = Some(next);
		self.catchup = (next <= after).then(|| (after, after.saturating_duration_since(next)));
	}

	/// Reset the schedule relative to `after` when the OS timer failed.
	fn complete_failed_wait(&mut self, after: Instant, wire_bytes: usize) {
		let tx_dur = tx_duration(wire_bytes, self.bytes_per_sec);
		self.catchup = None;
		self.next_send_at = Some(after + tx_dur);
	}
}

/// Async Linux `timerfd` wrapper backed by Tokio's `AsyncFd` for sub-millisecond,
/// zero-spin, cancellable pacing waits.
struct AsyncTimerFd {
	fd: AsyncFd<OwnedFd>,
}

impl AsyncTimerFd {
	fn new() -> std::io::Result<Self> {
		let owned = timerfd_create(
			TimerfdClockId::Monotonic,
			TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
		)
		.map_err(std::io::Error::from)?;
		let fd = AsyncFd::with_interest(owned, Interest::READABLE)?;
		Ok(Self { fd })
	}

	async fn sleep(&self, duration: Duration) -> std::io::Result<()> {
		// In `timerfd_settime`, a zero `it_value` disarms the timer instead of firing immediately.
		let duration = duration.max(Duration::from_nanos(1));
		let spec = Itimerspec {
			it_interval: Timespec { tv_sec: 0, tv_nsec: 0 },
			it_value: Timespec {
				tv_sec: duration.as_secs().try_into().unwrap_or(i64::MAX),
				tv_nsec: duration.subsec_nanos().into(),
			},
		};
		timerfd_settime(self.fd.get_ref(), TimerfdTimerFlags::empty(), &spec).map_err(std::io::Error::from)?;

		loop {
			let mut guard = self.fd.readable().await?;
			match guard.try_io(|inner| read_timerfd(inner.get_ref())) {
				Ok(Ok(_expirations)) => return Ok(()),
				Ok(Err(e)) => return Err(e),
				Err(_would_block) => continue,
			}
		}
	}
}

fn read_timerfd(fd: &OwnedFd) -> std::io::Result<u64> {
	let mut buf = [0u8; std::mem::size_of::<u64>()];
	rustix::io::read(fd, &mut buf).map_err(std::io::Error::from)?;
	Ok(u64::from_ne_bytes(buf))
}

/// Active per-session UDP packet pacer attached to a [`super::gso_socket::UdpGsoSocket`].
pub(crate) struct PacketPacer {
	schedule: PacingSchedule,
	timer: AsyncTimerFd,
}

impl PacketPacer {
	/// Create a pacer targeting `bytes_per_sec` wire bytes per second.
	pub fn new(bytes_per_sec: u64) -> std::io::Result<Self> {
		let bytes_per_sec = NonZeroU64::new(bytes_per_sec.max(1)).expect("max(1) is non-zero");
		Ok(Self {
			schedule: PacingSchedule::new(bytes_per_sec),
			timer: AsyncTimerFd::new()?,
		})
	}

	pub fn bytes_per_sec(&self) -> u64 {
		self.schedule.bytes_per_sec()
	}

	/// Wait until the next send slot for a chunk of `wire_bytes`, then advance the schedule.
	pub async fn pace(&mut self, wire_bytes: usize) {
		let Some(wait) = self.schedule.plan_send(Instant::now(), wire_bytes) else {
			return;
		};
		if let Err(e) = self.timer.sleep(wait).await {
			tracing::warn!("Video pacer timer wait failed: {e}");
			self.schedule.complete_failed_wait(Instant::now(), wire_bytes);
			return;
		}
		self.schedule.complete_delayed_send(Instant::now(), wire_bytes);
	}
}

#[cfg(test)]
mod tests {
	use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

	use super::*;
	use crate::session::stream::video::video_bitrate_within;

	#[test]
	fn pacing_rate_scales_from_configured_bitrate() {
		// Standard FEC (20%): 1.5x configured bitrate.
		let enc_150 = video_bitrate_within(150_000_000, 20, 512_000);
		assert_eq!(pacing_bytes_per_sec(Some(150_000_000), enc_150, 20), 28_125_000); // 225 Mb/s

		let enc_100 = video_bitrate_within(100_000_000, 20, 512_000);
		assert_eq!(pacing_bytes_per_sec(Some(100_000_000), enc_100, 20), 18_750_000); // 150 Mb/s

		let enc_65 = video_bitrate_within(65_000_000, 20, 512_000);
		assert_eq!(pacing_bytes_per_sec(Some(65_000_000), enc_65, 20), 12_187_500); // 97.5 Mb/s

		// FEC at 80% still subtracts parity in `video_bitrate_within`, so configured bitrate binds.
		let enc_80 = video_bitrate_within(100_000_000, 80, 512_000);
		assert_eq!(pacing_bytes_per_sec(Some(100_000_000), enc_80, 80), 18_750_000);

		// Low and zero bitrates floor at 10 Mb/s.
		assert_eq!(
			pacing_bytes_per_sec(Some(5_000_000), 3_500_000, 20),
			MIN_PACING_BYTES_PER_SEC
		);
		assert_eq!(pacing_bytes_per_sec(None, 0, 20), MIN_PACING_BYTES_PER_SEC);
	}

	#[test]
	fn pacing_rate_accounts_for_high_fec_above_80_percent() {
		// Above 80% FEC, `video_bitrate_within` leaves video near `configured`, so video + FEC
		// reaches ~1.8–2.0x `configured`. The pacer must scale to 1.5x that actual stream rate
		// instead of starving the sender below its own wire bitrate.
		for fec in [81u8, 90, 100] {
			let configured = 100_000_000;
			let encoded = video_bitrate_within(configured, fec, 512_000);
			let actual_video_plus_fec_bytes = (encoded as u128 * (100 + u128::from(fec)) / 100 / 8) as u64;
			let paced = pacing_bytes_per_sec(Some(configured), encoded, fec);
			assert!(
				paced >= actual_video_plus_fec_bytes * 3 / 2 - 1,
				"fec={fec}%: paced {paced} B/s should keep ~1.5x headroom above {actual_video_plus_fec_bytes} B/s"
			);
		}
	}

	#[test]
	fn chunk_wire_bytes_includes_ethernet_ip_udp_headers() {
		assert_eq!(chunk_wire_bytes(4 * 1408, 1408, false), 4 * (1408 + 66));
		assert_eq!(chunk_wire_bytes(4 * 1408, 1408, true), 4 * (1408 + 86));
		assert_eq!(chunk_wire_bytes(100, 1408, false), 166);
	}

	#[test]
	fn ipv4_mapped_ipv6_uses_ipv4_wire_overhead() {
		let v4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 10), 47998));
		let mapped = SocketAddr::V6(SocketAddrV6::new(
			Ipv4Addr::new(192, 168, 1, 10).to_ipv6_mapped(),
			47998,
			0,
			0,
		));
		let native_v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 47998, 0, 0));

		assert!(!is_ipv6_wire(v4));
		assert!(!is_ipv6_wire(mapped));
		assert!(is_ipv6_wire(native_v6));
	}

	#[test]
	fn schedule_holds_target_rate_with_timer_slack() {
		// 180 Mb/s = 22.5 MB/s; 40 chunks of 4 * 1408B payload with 60 us timer slack on every wait.
		let bytes_per_sec = NonZeroU64::new(22_500_000).unwrap();
		let mut schedule = PacingSchedule::new(bytes_per_sec);
		let wire_chunk = chunk_wire_bytes(4 * 1408, 1408, false);
		let chunks = 40usize;
		let slack = Duration::from_micros(60);

		let origin = Instant::now();
		let mut now = origin;
		for _ in 0..chunks {
			if let Some(wait) = schedule.plan_send(now, wire_chunk) {
				now += wait + slack;
				schedule.complete_delayed_send(now, wire_chunk);
			}
		}

		let expected_ms = ((chunks - 1) * wire_chunk) as f64 / bytes_per_sec.get() as f64 * 1000.0;
		let elapsed_ms = now.duration_since(origin).as_secs_f64() * 1000.0;
		assert!(
			elapsed_ms >= expected_ms * 0.98 && elapsed_ms <= expected_ms * 1.03,
			"expected ~{expected_ms:.3} ms with 60 us timer slack, got {elapsed_ms:.3} ms"
		);
	}

	#[test]
	fn schedule_holds_target_rate_with_single_segment_chunks() {
		// 220 Mb/s with 1-shard chunks (~53.6 us/shard) and 120 us timer slack (> 2x shard interval).
		let bytes_per_sec = NonZeroU64::new(27_500_000).unwrap();
		let mut schedule = PacingSchedule::new(bytes_per_sec);
		let wire_chunk = chunk_wire_bytes(1408, 1408, false);
		let chunks = 200usize;
		let slack = Duration::from_micros(120);

		let origin = Instant::now();
		let mut now = origin;
		for _ in 0..chunks {
			if let Some(wait) = schedule.plan_send(now, wire_chunk) {
				now += wait + slack;
				schedule.complete_delayed_send(now, wire_chunk);
			}
		}

		let expected_ms = ((chunks - 1) * wire_chunk) as f64 / bytes_per_sec.get() as f64 * 1000.0;
		let elapsed_ms = now.duration_since(origin).as_secs_f64() * 1000.0;
		assert!(
			elapsed_ms >= expected_ms * 0.98 && elapsed_ms <= expected_ms * 1.03,
			"expected ~{expected_ms:.3} ms for 1-shard chunks with 120 us slack, got {elapsed_ms:.3} ms"
		);
	}

	#[test]
	fn schedule_caps_catchup_burst_at_max_burst_wire_bytes() {
		let bytes_per_sec = NonZeroU64::new(28_125_000).unwrap(); // 225 Mb/s
		for segments in [1usize, PACED_MAX_GSO_SEGMENTS] {
			let mut schedule = PacingSchedule::new(bytes_per_sec);
			let wire_chunk = chunk_wire_bytes(segments * 1408, 1408, false);

			let mut now = Instant::now();
			assert_eq!(schedule.plan_send(now, wire_chunk), None);

			// Next chunk must wait; simulate a 5 ms OS scheduling stall.
			let wait = schedule.plan_send(now, wire_chunk).expect("second chunk should wait");
			now += wait + Duration::from_millis(5);
			schedule.complete_delayed_send(now, wire_chunk);

			// Count how many wire bytes leave at the exact same instant `now` before the next wait.
			let mut burst_bytes = wire_chunk;
			while schedule.plan_send(now, wire_chunk).is_none() {
				burst_bytes += wire_chunk;
				assert!(
					burst_bytes <= MAX_BURST_WIRE_BYTES * 2,
					"catch-up burst did not terminate for {segments}-segment chunks"
				);
			}
			assert!(
				burst_bytes <= MAX_BURST_WIRE_BYTES,
				"segments={segments}: catch-up burst was {burst_bytes} B, exceeding {MAX_BURST_WIRE_BYTES} B cap"
			);
		}
	}

	#[test]
	fn schedule_does_not_bank_idle_credit_even_after_catchup() {
		let bytes_per_sec = NonZeroU64::new(22_500_000).unwrap(); // 180 Mb/s
		let mut schedule = PacingSchedule::new(bytes_per_sec);
		let wire_chunk = chunk_wire_bytes(PACED_MAX_GSO_SEGMENTS * 1408, 1408, false);
		let expected_tx = tx_duration(wire_chunk, bytes_per_sec);

		let mut now = Instant::now();
		assert_eq!(schedule.plan_send(now, wire_chunk), None);

		// End the previous frame with a late wake so `catchup` credit is actively armed.
		let wait = schedule.plan_send(now, wire_chunk).expect("second chunk should wait");
		now += wait + Duration::from_millis(2);
		schedule.complete_delayed_send(now, wire_chunk);
		assert!(
			schedule.catchup.is_some(),
			"expected late wake to leave catch-up credit"
		);

		// Simulate an idle gap between frames.
		now += Duration::from_millis(10);

		// First chunk of the new frame sends immediately, and the second chunk must wait the full
		// inter-chunk interval (no two-chunk burst from stale catch-up state).
		assert_eq!(schedule.plan_send(now, wire_chunk), None);
		let second_wait = schedule
			.plan_send(now, wire_chunk)
			.expect("second chunk after idle gap must be paced, not burst");
		assert_eq!(second_wait, expected_tx);
	}

	#[tokio::test]
	async fn pacer_holds_target_rate() {
		// 50 Mb/s = 6.25 MB/s; 24 chunks of 4 * 1408B payload (+ 66B wire framing/shard)
		// span 23 inter-chunk gaps (~943 us each) = ~21.7 ms.
		let bytes_per_sec = 6_250_000;
		let mut pacer = PacketPacer::new(bytes_per_sec).unwrap();
		let wire_chunk = chunk_wire_bytes(PACED_MAX_GSO_SEGMENTS * 1408, 1408, false);
		let chunks = 24usize;

		let start = Instant::now();
		for _ in 0..chunks {
			pacer.pace(wire_chunk).await;
		}
		let elapsed = start.elapsed();

		let expected_ms = ((chunks - 1) * wire_chunk) as f64 / bytes_per_sec as f64 * 1000.0;
		let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
		assert!(
			elapsed_ms >= expected_ms * 0.85 && elapsed_ms <= expected_ms * 2.0,
			"expected ~{expected_ms:.2} ms for {chunks} chunks at 50 Mb/s, got {elapsed_ms:.2} ms"
		);
	}

	#[tokio::test]
	async fn pacer_does_not_bank_idle_credit() {
		let bytes_per_sec = 12_500_000; // 100 Mb/s
		let mut pacer = PacketPacer::new(bytes_per_sec).unwrap();
		let wire_chunk = chunk_wire_bytes(PACED_MAX_GSO_SEGMENTS * 1408, 1408, false); // 5896 B -> ~472 us/chunk

		pacer.pace(wire_chunk).await;
		// Simulate an idle gap between frames.
		tokio::time::sleep(Duration::from_millis(10)).await;

		let start = Instant::now();
		for _ in 0..15 {
			pacer.pace(wire_chunk).await;
		}
		let elapsed = start.elapsed();
		// 14 inter-chunk gaps * 472 us = ~6.6 ms; banked credit would finish in ~0 ms.
		assert!(
			elapsed >= Duration::from_millis(5),
			"idle gap banked burst credit: 15 chunks took only {elapsed:?}"
		);
	}
}
