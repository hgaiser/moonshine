//! Userspace UDP packet pacing and bounded GSO bursts.
//!
//! Without pacing, Moonshine sends a frame's shards in up to 64-segment (~64 KiB) UDP GSO
//! super-packets at NIC line rate (e.g. 2.5 GbE). When a faster host streams to a slower or
//! shallow-buffered link (such as a 2.5 GbE -> 1 GbE switch without 802.3x flow control, a
//! built-in 100 Mb/s TV port, or a USB 2.0 Ethernet adapter with a small on-chip RX FIFO),
//! frame bursts can overflow buffers and drop tail packets well below the link's average
//! throughput.
//!
//! When enabled for a paired client, [`PacketPacer`] limits GSO bursts to
//! [`PACED_MAX_GSO_SEGMENTS`] datagrams and asynchronously paces chunks at 1.5x the client's
//! configured stream bitrate using a Linux `timerfd` wrapped in Tokio's `AsyncFd` (zero
//! busy-spinning, works with any egress qdisc, and cancels cleanly on session shutdown).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Pacing headroom multiplier (`3 / 2 = 1.5x`) applied to the client's configured stream bitrate.
///
/// Leaves 50% headroom so normal frames finish transmitting well inside one frame interval while
/// keeping burst rates safely below shallow-buffered links (for example, 150 Mb/s -> 225 Mb/s on
/// a ~250 Mb/s USB 2.0 GbE adapter, or 65 Mb/s -> 97.5 Mb/s on a 100 Mb/s TV Ethernet port).
const PACING_MULTIPLIER_NUM: u128 = 3;
const PACING_MULTIPLIER_DEN: u128 = 2;

/// Minimum pacing rate (10 Mb/s = 1,250,000 bytes/s) so very low bitrate streams do not stall
/// keyframes unnecessarily.
const MIN_PACING_BYTES_PER_SEC: u64 = 1_250_000;

/// Maximum UDP segments per GSO `sendmsg` while pacing is active (~5.8 KiB at 1450-byte wire
/// packets), staying well below typical 16–32 KiB USB Ethernet RX FIFOs while keeping syscall
/// overhead low.
pub(crate) const PACED_MAX_GSO_SEGMENTS: usize = 4;

/// Skip arming `timerfd` for tiny waits; the remainder carries into the next chunk's deadline.
const MIN_TIMER_WAIT: Duration = Duration::from_micros(20);

/// Maximum burst of wire bytes allowed when catching up after kernel timer slack or OS
/// scheduling jitter. Stays below typical USB 2.0 Ethernet RX FIFOs (~16–32 KiB) while letting
/// the pacer hold its target average rate under CPU load.
const MAX_BURST_WIRE_BYTES: usize = 12 * 1024;

/// Minimum catch-up window so larger chunks still absorb normal Linux timer slack (~50 us).
const MIN_CATCHUP_WINDOW: Duration = Duration::from_micros(250);

/// Ethernet (14) + IPv4 (20) + UDP (8) header bytes per datagram on the wire.
const IPV4_WIRE_OVERHEAD_BYTES: usize = 42;

/// Ethernet (14) + IPv6 (40) + UDP (8) header bytes per datagram on the wire.
const IPV6_WIRE_OVERHEAD_BYTES: usize = 62;

/// Target wire pacing rate in bytes/sec derived automatically from the client's configured
/// stream bitrate (`1.5x`, floored at 10 Mb/s).
pub(crate) fn pacing_bytes_per_sec(configured_bitrate_bps: usize) -> u64 {
	let bytes_per_sec =
		(configured_bitrate_bps as u128).saturating_mul(PACING_MULTIPLIER_NUM) / (PACING_MULTIPLIER_DEN * 8);
	bytes_per_sec.clamp(u128::from(MIN_PACING_BYTES_PER_SEC), u128::from(u64::MAX)) as u64
}

/// Wire bytes sent for a chunk of `chunk_len` UDP payload bytes split into `shard_size` datagrams,
/// including L2/L3/L4 (Ethernet + IP + UDP) framing.
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
fn tx_duration(wire_bytes: usize, bytes_per_sec: u64) -> Duration {
	let nanos = (wire_bytes as u128).saturating_mul(1_000_000_000) / u128::from(bytes_per_sec.max(1));
	Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

/// Async Linux `timerfd` wrapper backed by Tokio's `AsyncFd` for sub-millisecond,
/// zero-spin, cancellable pacing waits.
struct AsyncTimerFd {
	fd: AsyncFd<OwnedFd>,
}

impl AsyncTimerFd {
	fn new() -> std::io::Result<Self> {
		let raw = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
		if raw < 0 {
			return Err(std::io::Error::last_os_error());
		}
		// SAFETY: `raw` is a valid, freshly created file descriptor owned exclusively by us.
		let owned = unsafe { OwnedFd::from_raw_fd(raw) };
		let fd = AsyncFd::with_interest(owned, Interest::READABLE)?;
		Ok(Self { fd })
	}

	async fn sleep(&self, duration: Duration) -> std::io::Result<()> {
		// In `timerfd_settime`, a zero `it_value` disarms the timer instead of firing immediately.
		let duration = duration.max(Duration::from_nanos(1));
		let spec = libc::itimerspec {
			it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
			it_value: libc::timespec {
				tv_sec: duration.as_secs() as libc::time_t,
				tv_nsec: duration.subsec_nanos() as libc::c_long,
			},
		};
		let ret = unsafe { libc::timerfd_settime(self.fd.get_ref().as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
		if ret != 0 {
			return Err(std::io::Error::last_os_error());
		}

		loop {
			let mut guard = self.fd.readable().await?;
			match guard.try_io(|inner| read_timerfd(inner.get_ref().as_raw_fd())) {
				Ok(Ok(_expirations)) => return Ok(()),
				Ok(Err(e)) => return Err(e),
				Err(_would_block) => continue,
			}
		}
	}
}

fn read_timerfd(fd: RawFd) -> std::io::Result<u64> {
	let mut expirations = 0u64;
	let ret = unsafe {
		libc::read(
			fd,
			(&mut expirations as *mut u64).cast::<libc::c_void>(),
			std::mem::size_of::<u64>(),
		)
	};
	if ret < 0 {
		Err(std::io::Error::last_os_error())
	} else {
		Ok(expirations)
	}
}

/// Active per-session UDP packet pacer attached to a [`super::gso_socket::UdpGsoSocket`].
pub(crate) struct PacketPacer {
	bytes_per_sec: u64,
	timer: AsyncTimerFd,
	next_send_at: Option<Instant>,
	/// True only while draining bounded catch-up credit immediately after a late timer wake.
	catching_up: bool,
}

impl PacketPacer {
	/// Create a pacer targeting `bytes_per_sec` wire bytes per second.
	pub fn new(bytes_per_sec: u64) -> std::io::Result<Self> {
		Ok(Self {
			bytes_per_sec: bytes_per_sec.max(1),
			timer: AsyncTimerFd::new()?,
			next_send_at: None,
			catching_up: false,
		})
	}

	pub fn bytes_per_sec(&self) -> u64 {
		self.bytes_per_sec
	}

	/// Wait until the next send slot for a chunk of `wire_bytes`, then advance the schedule.
	pub async fn pace(&mut self, wire_bytes: usize) {
		let tx_dur = tx_duration(wire_bytes, self.bytes_per_sec);
		let max_catchup = tx_duration(MAX_BURST_WIRE_BYTES.saturating_sub(wire_bytes), self.bytes_per_sec)
			.max(tx_dur.min(MIN_CATCHUP_WINDOW));
		let now = Instant::now();

		match self.next_send_at {
			Some(deadline) if deadline > now => {
				let wait = deadline - now;
				if wait >= MIN_TIMER_WAIT {
					if let Err(e) = self.timer.sleep(wait).await {
						tracing::warn!("Video pacer timer wait failed: {e}");
						self.next_send_at = Some(Instant::now() + tx_dur);
						self.catching_up = false;
						return;
					}
					let after = Instant::now();
					// Advance from `deadline` so normal Linux timer slack and wake jitter do not
					// permanently erode average throughput, while capping catch-up so a long OS
					// scheduling stall cannot trigger an unbounded burst.
					let base = deadline.max(after.checked_sub(max_catchup).unwrap_or(deadline));
					let next = base + tx_dur;
					self.catching_up = next <= after;
					self.next_send_at = Some(next);
				} else {
					// Tiny wait: send immediately and let the next chunk absorb the remainder.
					self.catching_up = false;
					self.next_send_at = Some(deadline + tx_dur);
				}
			},
			Some(deadline) if self.catching_up => {
				// Still draining bounded catch-up immediately after a late timer wake.
				let base = deadline.max(now.checked_sub(max_catchup).unwrap_or(deadline));
				let next = base + tx_dur;
				self.catching_up = next <= now;
				self.next_send_at = Some(next);
			},
			_ => {
				// First chunk or idle gap between frames: send immediately without banking
				// idle time as burst credit for the next keyframe.
				self.catching_up = false;
				self.next_send_at = Some(now + tx_dur);
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pacing_rate_scales_from_configured_bitrate() {
		// 150 Mb/s -> 225 Mb/s = 28,125,000 B/s
		assert_eq!(pacing_bytes_per_sec(150_000_000), 28_125_000);
		// 100 Mb/s -> 150 Mb/s = 18,750,000 B/s
		assert_eq!(pacing_bytes_per_sec(100_000_000), 18_750_000);
		// 65 Mb/s -> 97.5 Mb/s = 12,187,500 B/s
		assert_eq!(pacing_bytes_per_sec(65_000_000), 12_187_500);
		// Low and zero bitrates floor at 10 Mb/s.
		assert_eq!(pacing_bytes_per_sec(5_000_000), MIN_PACING_BYTES_PER_SEC);
		assert_eq!(pacing_bytes_per_sec(0), MIN_PACING_BYTES_PER_SEC);
	}

	#[test]
	fn chunk_wire_bytes_includes_per_datagram_headers() {
		assert_eq!(chunk_wire_bytes(4 * 1408, 1408, false), 4 * (1408 + 42));
		assert_eq!(chunk_wire_bytes(4 * 1408, 1408, true), 4 * (1408 + 62));
		assert_eq!(chunk_wire_bytes(100, 1408, false), 142);
	}

	#[tokio::test]
	async fn pacer_holds_target_rate() {
		// 180 Mb/s = 22.5 MB/s; 40 chunks of 4 * 1408B payload (+ 42B header/shard)
		// span 39 inter-chunk gaps = ~10.05 ms.
		let bytes_per_sec = 22_500_000;
		let mut pacer = PacketPacer::new(bytes_per_sec).unwrap();
		let wire_chunk = chunk_wire_bytes(4 * 1408, 1408, false);
		let chunks = 40usize;

		let start = Instant::now();
		for _ in 0..chunks {
			pacer.pace(wire_chunk).await;
		}
		let elapsed = start.elapsed();

		let expected_ms = ((chunks - 1) * wire_chunk) as f64 / bytes_per_sec as f64 * 1000.0;
		let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
		assert!(
			elapsed_ms >= expected_ms * 0.85 && elapsed_ms <= expected_ms * 1.4,
			"expected ~{expected_ms:.2} ms for {chunks} chunks at 180 Mb/s, got {elapsed_ms:.2} ms"
		);
	}

	#[tokio::test]
	async fn pacer_does_not_bank_idle_credit() {
		let bytes_per_sec = 12_500_000; // 100 Mb/s
		let mut pacer = PacketPacer::new(bytes_per_sec).unwrap();
		let wire_chunk = chunk_wire_bytes(4 * 1408, 1408, false); // 5800 B -> ~464 us/chunk

		pacer.pace(wire_chunk).await;
		// Simulate an idle gap between frames.
		tokio::time::sleep(Duration::from_millis(10)).await;

		let start = Instant::now();
		for _ in 0..15 {
			pacer.pace(wire_chunk).await;
		}
		let elapsed = start.elapsed();
		// 14 inter-chunk gaps * 464 us = ~6.5 ms; banked credit would finish in ~0 ms.
		assert!(
			elapsed >= Duration::from_millis(5),
			"idle gap banked burst credit: 15 chunks took only {elapsed:?}"
		);
	}

	#[tokio::test]
	async fn pacer_holds_target_rate_with_single_segment_chunks() {
		// 220 Mb/s with 1-shard (1450B wire) chunks: ~52.7 us per shard.
		// Bounded catch-up should preserve target throughput even when kernel timer slack
		// exceeds one shard interval.
		let bytes_per_sec = 27_500_000;
		let mut pacer = PacketPacer::new(bytes_per_sec).unwrap();
		let wire_chunk = chunk_wire_bytes(1408, 1408, false);
		let chunks = 200usize; // ~290 KB wire -> ~10.5 ms expected

		let start = Instant::now();
		for _ in 0..chunks {
			pacer.pace(wire_chunk).await;
		}
		let elapsed = start.elapsed();

		let expected_ms = ((chunks - 1) * wire_chunk) as f64 / bytes_per_sec as f64 * 1000.0;
		let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
		assert!(
			elapsed_ms >= expected_ms * 0.80 && elapsed_ms <= expected_ms * 1.45,
			"expected ~{expected_ms:.2} ms for {chunks} 1-shard chunks at 220 Mb/s, got {elapsed_ms:.2} ms"
		);
	}
}
