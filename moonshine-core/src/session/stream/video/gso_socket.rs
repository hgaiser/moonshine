use std::net::SocketAddr;
use std::time::{Duration, Instant};

use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use tokio::io::Interest;
use tokio::net::UdpSocket;

use super::pacing_timer::PacingTimer;
use super::shard_batch::{ShardBatch, TransportCompletion, TransportOutcome};

/// Maximum payload of one UDP datagram (65535 minus IPv4/UDP headers).
pub(super) const MAX_UDP_PAYLOAD: usize = 65507;
/// Use the common 64-segment GSO cadence for pacing even when GSO is off.
const MAX_PACING_SEGMENTS: usize = 64;

/// Keep the uncontended raw-send path, but let Tokio observe every failed
/// retry. Quinn uses sendmsg directly and cannot clear Tokio's cached readiness.
async fn send_with_readiness(
	socket: &UdpSocket,
	would_block_events: &mut u32,
	mut send: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
	let mut send = || {
		let result = send();
		if matches!(&result, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock) {
			*would_block_events = would_block_events.saturating_add(1);
		}
		result
	};
	match send() {
		Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
			// The closure must use raw I/O only. On another WouldBlock,
			// async_io clears readiness before awaiting a fresh writable event.
			socket.async_io(Interest::WRITABLE, send).await
		},
		result => result,
	}
}

fn gso_segments_per_send(max_gso_segments: usize, shard_size: usize) -> usize {
	max_gso_segments
		.min(MAX_UDP_PAYLOAD.checked_div(shard_size).unwrap_or(0))
		.max(1)
}

fn pacing_segments_per_send(shard_size: usize) -> usize {
	gso_segments_per_send(MAX_PACING_SEGMENTS, shard_size)
}

fn network_header_size(addr: SocketAddr) -> usize {
	// UDP plus the IP header. Ethernet framing varies with VLANs and offload.
	if addr.is_ipv4() { 28 } else { 48 }
}

fn wire_bytes(payload_bytes: usize, shard_count: usize, addr: SocketAddr) -> u64 {
	let payload = u64::try_from(payload_bytes).unwrap_or(u64::MAX);
	let headers = u64::try_from(shard_count)
		.unwrap_or(u64::MAX)
		.saturating_mul(network_header_size(addr) as u64);
	payload.saturating_add(headers)
}

/// Duration represented by `encoded_bytes` at the negotiated encoder bitrate.
/// Chunk deadlines are scaled by actual wire bytes, so FEC and headers are
/// spread across this duration without changing the codec bitrate.
fn frame_pacing_duration(encoded_bytes: usize, bitrate_bps: u64) -> Duration {
	if encoded_bytes == 0 || bitrate_bps == 0 {
		return Duration::ZERO;
	}
	let nanos = (encoded_bytes as u128)
		.saturating_mul(8)
		.saturating_mul(1_000_000_000)
		.checked_div(u128::from(bitrate_bps))
		.unwrap_or_default()
		.min(u128::from(u64::MAX));
	Duration::from_nanos(nanos as u64)
}

fn scaled_duration(total: Duration, numerator: u64, denominator: u64) -> Duration {
	if numerator == 0 || denominator == 0 || total.is_zero() {
		return Duration::ZERO;
	}
	let nanos = total
		.as_nanos()
		.saturating_mul(u128::from(numerator))
		.checked_div(u128::from(denominator))
		.unwrap_or_default()
		.min(u128::from(u64::MAX));
	Duration::from_nanos(nanos as u64)
}

pub(crate) fn duration_micros_u64(duration: Duration) -> u64 {
	duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn pacing_schedule(
	requested: Instant,
	pacing_duration: Duration,
	started: Instant,
) -> (Instant, Duration, bool, Duration) {
	let lateness = started.saturating_duration_since(requested);
	if pacing_duration.is_zero() || requested >= started {
		return (requested, pacing_duration, false, lateness);
	}

	match requested.checked_add(pacing_duration) {
		Some(deadline) if deadline > started => {
			// Encoding used part of this frame's transmit window. Start now and
			// distribute every chunk across the time that remains; do not dump the
			// already-due prefix as a catch-up burst.
			(started, deadline.duration_since(started), false, lateness)
		},
		_ => {
			// The complete window elapsed while encoding. Use a fresh full window,
			// allowing the synchronous producer to discard stale captures.
			(started, pacing_duration, true, lateness)
		},
	}
}

/// Rebase after observed readiness waits or a chunk overrun that consumed the
/// next slot. Ordinary healthy send cost does not extend every pacing interval.
fn rebase_after_chunk(
	origin: Instant,
	duration: Duration,
	preceding: u64,
	chunk_wire: u64,
	total: u64,
	completed: Instant,
	waited: bool,
) -> Option<Instant> {
	if duration.is_zero() {
		return None;
	}
	let offset = scaled_duration(duration, preceding, total);
	let next = origin + scaled_duration(duration, preceding.saturating_add(chunk_wire), total);
	if waited || completed >= next {
		Some(completed.checked_sub(offset).unwrap_or(completed))
	} else {
		None
	}
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SendStats {
	pub gso_sends: u32,
	pub per_shard_sends: u32,
	pub would_block_events: u32,
	pub fallback_chunks: u32,
	pub gso_segments_per_send: usize,
	pub final_chunk_segments: usize,
	pub final_chunk_bytes: usize,
	/// Successfully submitted IP/UDP bytes, not confirmed receiver delivery.
	pub wire_bytes: u64,
	pub planned_wire_bytes: u64,
	pub outcome: TransportOutcome,
	pub discarded_datagrams: usize,
	pub discarded_payload_bytes: usize,
	pub elapsed: Duration,
	pub pacing_duration: Duration,
	pub scheduled_pacing_duration: Duration,
	pub initial_pacing_lateness: Duration,
	pub max_pacing_lateness: Duration,
	pub pacing_rebased: bool,
	pub backpressure_rebases: u32,
}

impl SendStats {
	pub fn released(completion: TransportCompletion) -> Self {
		Self {
			outcome: completion.outcome,
			discarded_datagrams: completion.discarded_datagrams,
			discarded_payload_bytes: completion.discarded_payload_bytes,
			elapsed: completion
				.send_started_at
				.map(|start| completion.finished_at.saturating_duration_since(start))
				.unwrap_or_default(),
			..Self::default()
		}
	}
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct SendFaults {
	pub reject_gso: bool,
	pub fail_after: Option<usize>,
	pub stall_after: Option<usize>,
	pub stall_duration: Option<Duration>,
}

/// UDP socket for video with GSO and frame-aware super-packet pacing.
///
/// A complete intra frame arrives here at once. Submitting all GSO packets
/// back-to-back creates a several-hundred-datagram microburst. GSO is retained,
/// but its chunk boundaries are paced from encoded bytes, negotiated bitrate,
/// and actual per-frame wire bytes.
pub(crate) struct UdpGsoSocket {
	#[cfg(test)]
	pub(super) faults: SendFaults,
	#[cfg(test)]
	chunk_starts: Vec<Instant>,
	socket: UdpSocket,
	udp_state: UdpSocketState,
	disable_gso: bool,
	pacing_timer: Option<PacingTimer>,
	pacing_timer_initialized: bool,
}

#[cfg(test)]
impl std::os::fd::AsRawFd for UdpGsoSocket {
	fn as_raw_fd(&self) -> std::os::fd::RawFd {
		self.socket.as_raw_fd()
	}
}

impl UdpGsoSocket {
	pub async fn new(address: &str, port: u16) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;
		let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
			.map_err(|e| tracing::error!("Failed to initialize UDP socket state: {e}"))?;
		// Diagnostic-only bypass; intentionally not a user-facing tuning knob.
		let disable_gso = std::env::var_os("MOONSHINE_VIDEO_DISABLE_GSO").is_some();
		if disable_gso {
			tracing::warn!("Video UDP GSO disabled by MOONSHINE_VIDEO_DISABLE_GSO; using per-shard sends");
		} else if udp_state.max_gso_segments() > 1 {
			tracing::debug!("GSO enabled, max segments: {}", udp_state.max_gso_segments());
		} else {
			tracing::debug!("GSO not available, using per-shard sends");
		}
		Ok(Self {
			#[cfg(test)]
			faults: SendFaults::default(),
			#[cfg(test)]
			chunk_starts: Vec::new(),
			socket,
			udp_state,
			disable_gso,
			pacing_timer: None,
			pacing_timer_initialized: false,
		})
	}

	#[cfg(test)]
	pub(super) fn force_no_gso_for_test(&mut self) {
		self.disable_gso = true;
	}

	pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
		self.socket.local_addr()
	}

	pub fn set_tos_v4(&self, tos: u32) -> std::io::Result<()> {
		self.socket.set_tos_v4(tos)
	}

	pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
		self.socket.recv_from(buf).await
	}

	/// `bitrate_bps` is set for the intra-only PyroWave path. Conventional
	/// inter-frame codecs retain their established immediate-send behavior.
	pub async fn send_batch(
		&mut self,
		batch: &mut ShardBatch,
		addr: SocketAddr,
		bitrate_bps: Option<u64>,
	) -> SendStats {
		let started = Instant::now();
		*batch.transport_parts().1 = TransportOutcome::default();
		let shard_size = batch.shard_size();
		let shard_count = batch.shard_count();
		let mut stats = SendStats {
			planned_wire_bytes: wire_bytes(batch.as_bytes().len(), shard_count, addr),
			..Default::default()
		};
		if shard_size == 0 || shard_count == 0 {
			return stats;
		}

		let gso_available = !self.disable_gso && self.udp_state.max_gso_segments() > 1;
		let mut gso_active = gso_available;
		let segments_per_send = if gso_available {
			gso_segments_per_send(self.udp_state.max_gso_segments(), shard_size)
		} else {
			pacing_segments_per_send(shard_size)
		};
		stats.gso_segments_per_send = if gso_available { segments_per_send } else { 0 };
		let chunk_bytes = segments_per_send.saturating_mul(shard_size).max(shard_size);
		let pacing_duration = bitrate_bps
			.map(|rate| frame_pacing_duration(batch.encoded_size(), rate))
			.unwrap_or_default();
		if !pacing_duration.is_zero() && !self.pacing_timer_initialized {
			self.pacing_timer_initialized = true;
			match PacingTimer::new() {
				Ok(timer) => self.pacing_timer = Some(timer),
				Err(error) => tracing::warn!(%error, "High-resolution video pacing unavailable; using Tokio timer"),
			}
		}
		stats.pacing_duration = pacing_duration;
		let requested_pacing_origin = batch.pacing_origin().unwrap_or(started);
		// A frame whose entire pacing window elapsed during encode is already
		// late. Rebase it instead of releasing every overdue GSO chunk as one
		// catch-up burst. PyroWave capture admission remains occupied until
		// this complete frame is sent, so no rendered backlog can accumulate.
		let (mut pacing_origin, scheduled_pacing_duration, pacing_rebased, initial_lateness) =
			pacing_schedule(requested_pacing_origin, pacing_duration, started);
		stats.pacing_rebased = pacing_rebased;
		stats.initial_pacing_lateness = initial_lateness;
		stats.scheduled_pacing_duration = scheduled_pacing_duration;
		let header_size = network_header_size(addr) as u64;
		let mut preceding_wire_bytes = 0u64;

		let (bytes, outcome) = batch.transport_parts();
		for chunk in bytes.chunks(chunk_bytes) {
			if !scheduled_pacing_duration.is_zero() {
				let deadline = pacing_origin
					+ scaled_duration(
						scheduled_pacing_duration,
						preceding_wire_bytes,
						stats.planned_wire_bytes,
					);
				let now = Instant::now();
				if now < deadline {
					if let Some(timer) = &mut self.pacing_timer {
						if let Err(error) = timer.sleep_until(deadline).await {
							tracing::warn!(%error, "High-resolution video pacing failed; using Tokio timer");
							self.pacing_timer = None;
							tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
						}
					} else {
						tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
					}
				} else {
					let lateness = now.saturating_duration_since(deadline);
					if preceding_wire_bytes == 0 && !stats.pacing_rebased {
						stats.initial_pacing_lateness = lateness;
					}
				}
				// Include timer wakeup overshoot, not only arrival after a deadline.
				stats.max_pacing_lateness = stats
					.max_pacing_lateness
					.max(Instant::now().saturating_duration_since(deadline));
			}

			#[cfg(test)]
			self.chunk_starts.push(Instant::now());
			let chunk_segments = chunk.len().div_ceil(shard_size);
			stats.final_chunk_segments = chunk_segments;
			stats.final_chunk_bytes = chunk.len();
			let before_would_block = stats.would_block_events;
			let mut chunk_would_block = false;
			if gso_active {
				outcome.attempted_datagrams += chunk_segments;
				outcome.attempted_payload_bytes += chunk.len();
				let transmit = Transmit {
					destination: addr,
					ecn: None,
					contents: chunk,
					segment_size: Some(shard_size),
					src_ip: None,
				};
				let result = send_with_readiness(&self.socket, &mut stats.would_block_events, || {
					#[cfg(test)]
					if self.faults.reject_gso {
						return Err(std::io::ErrorKind::InvalidInput.into());
					}
					let result = self.udp_state.try_send(UdpSockRef::from(&self.socket), &transmit);
					chunk_would_block |= matches!(&result, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock);
					result
				})
				.await;
				if let Err(e) = result {
					gso_active = false;
					stats.fallback_chunks = stats.fallback_chunks.saturating_add(1);
					outcome.last_error = Some(e.kind());
					stats.per_shard_sends = stats.per_shard_sends.saturating_add(
						self.send_shards(chunk, shard_size, addr, &mut stats.would_block_events, outcome, true)
							.await,
					);
				} else {
					stats.gso_sends = stats.gso_sends.saturating_add(1);
					outcome.submitted_datagrams += chunk_segments;
					outcome.submitted_payload_bytes += chunk.len();
				}
			} else {
				stats.per_shard_sends = stats.per_shard_sends.saturating_add(
					self.send_shards(chunk, shard_size, addr, &mut stats.would_block_events, outcome, false)
						.await,
				);
			}

			let chunk_wire_bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX).saturating_add(
				u64::try_from(chunk_segments)
					.unwrap_or(u64::MAX)
					.saturating_mul(header_size),
			);
			// Socket backpressure may consume multiple scheduled chunk slots. Move
			// the remaining schedule forward from the completed chunk instead of
			// releasing every now-overdue GSO send as a catch-up microburst.
			if let Some(rebased) = rebase_after_chunk(
				pacing_origin,
				scheduled_pacing_duration,
				preceding_wire_bytes,
				chunk_wire_bytes,
				stats.planned_wire_bytes,
				Instant::now(),
				chunk_would_block || stats.would_block_events > before_would_block,
			) {
				pacing_origin = rebased;
				stats.backpressure_rebases = stats.backpressure_rebases.saturating_add(1);
			}
			preceding_wire_bytes = preceding_wire_bytes.saturating_add(chunk_wire_bytes);
		}
		stats.outcome = *outcome;
		stats.wire_bytes = wire_bytes(outcome.submitted_payload_bytes, outcome.submitted_datagrams, addr);
		stats.elapsed = started.elapsed();
		stats
	}

	async fn send_shards(
		&self,
		bytes: &[u8],
		shard_size: usize,
		addr: SocketAddr,
		would_block: &mut u32,
		outcome: &mut TransportOutcome,
		already_attempted: bool,
	) -> u32 {
		if shard_size == 0 {
			return 0;
		}
		let mut sent = 0;
		for shard in bytes.chunks(shard_size) {
			if !already_attempted {
				outcome.attempted_datagrams += 1;
				outcome.attempted_payload_bytes += shard.len();
			}
			#[cfg(test)]
			if self.faults.stall_after == Some(outcome.submitted_datagrams) {
				*would_block += 1;
				match self.faults.stall_duration {
					Some(delay) => tokio::time::sleep(delay).await,
					None => std::future::pending::<()>().await,
				}
			}
			// Use the same readiness and stall observation as raw GSO sends.
			let result = send_with_readiness(&self.socket, would_block, || {
				#[cfg(test)]
				if self.faults.fail_after.is_some_and(|n| outcome.submitted_datagrams >= n) {
					return Err(std::io::ErrorKind::NetworkUnreachable.into());
				}
				socket2::SockRef::from(&self.socket)
					.send_to(shard, &addr.into())
					.and_then(|n| {
						if n == shard.len() {
							Ok(())
						} else {
							Err(std::io::ErrorKind::WriteZero.into())
						}
					})
			})
			.await;
			match result {
				Ok(()) => {
					sent += 1;
					outcome.submitted_datagrams += 1;
					outcome.submitted_payload_bytes += shard.len();
				},
				Err(error) => {
					outcome.last_error = Some(error.kind());
					outcome.failed_datagrams += 1;
				},
			}
		}
		sent
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::video::shard_batch::ShardBuf;

	async fn loopback_socket(disable_gso: bool) -> (UdpGsoSocket, UdpSocket, SocketAddr) {
		let receiver = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
		let destination = receiver.local_addr().unwrap();
		let raw_socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
		let udp_state = UdpSocketState::new(UdpSockRef::from(&raw_socket)).unwrap();
		(
			UdpGsoSocket {
				faults: SendFaults::default(),
				chunk_starts: Vec::new(),
				socket: raw_socket,
				udp_state,
				disable_gso,
				pacing_timer: None,
				pacing_timer_initialized: false,
			},
			receiver,
			destination,
		)
	}

	fn test_batch(shards: usize, shard_size: usize, encoded_size: usize) -> ShardBatch {
		let mut batch = ShardBuf::new(shards, shard_size, 0).into_batch();
		batch.set_frame_metadata(1, encoded_size, shards, 0, 1);
		batch
	}

	#[test]
	fn chunk_overruns_and_readiness_waits_rebase_without_catch_up() {
		let origin = Instant::now();
		let duration = Duration::from_millis(30);
		for waited in [false, true] {
			for stall in [100, 500] {
				let completed = origin + Duration::from_millis(stall);
				let rebased = rebase_after_chunk(origin, duration, 100, 100, 300, completed, waited).unwrap();
				let next = rebased + scaled_duration(duration, 200, 300);
				assert_eq!(next.duration_since(completed), Duration::from_millis(10));
			}
		}
		assert!(
			rebase_after_chunk(
				origin,
				duration,
				100,
				100,
				300,
				origin + Duration::from_millis(11),
				false
			)
			.is_none()
		);
	}

	#[tokio::test]
	async fn partial_and_permanent_failures_count_only_kernel_submissions() {
		for after in [0, 3] {
			let (mut socket, _receiver, destination) = loopback_socket(true).await;
			socket.faults.fail_after = Some(after);
			let mut batch = test_batch(7, 64, 448);
			let stats = socket.send_batch(&mut batch, destination, None).await;
			assert_eq!(stats.outcome.attempted_datagrams, 7);
			assert_eq!(stats.outcome.attempted_payload_bytes, 448);
			assert_eq!(stats.outcome.submitted_datagrams, after);
			assert_eq!(stats.outcome.submitted_payload_bytes, after * 64);
			assert_eq!(stats.outcome.failed_datagrams, 7 - after);
			assert_eq!(stats.wire_bytes, (after * (64 + 28)) as u64);
			let (tx, rx) = std::sync::mpsc::sync_channel(1);
			batch.set_send_completion(tx);
			batch.notify_sent();
			assert_eq!(
				rx.try_recv().unwrap().disposition,
				super::super::shard_batch::CompletionDisposition::Failed
			);
		}
	}

	#[tokio::test]
	async fn actual_fallback_submits_each_datagram_once() {
		let (mut socket, receiver, destination) = loopback_socket(false).await;
		if socket.udp_state.max_gso_segments() <= 1 {
			return;
		}
		socket.faults.reject_gso = true;
		let mut batch = test_batch(47, 1408, 47 * 1376);
		let stats = socket.send_batch(&mut batch, destination, None).await;
		assert_eq!(stats.fallback_chunks, 1);
		assert_eq!(stats.outcome.attempted_datagrams, 47);
		assert_eq!(stats.outcome.submitted_datagrams, 47);
		assert_eq!(stats.outcome.failed_datagrams, 0);
		for _ in 0..47 {
			assert_eq!(receiver.recv_from(&mut [0; 2048]).await.unwrap().0, 1408);
		}
	}

	#[tokio::test]
	async fn fallback_and_no_gso_rebase_after_100_and_500_ms_stalls() {
		for fallback in [false, true] {
			for stall in [100, 500] {
				let (mut socket, _receiver, destination) = loopback_socket(!fallback).await;
				socket.faults.reject_gso = fallback;
				socket.faults.stall_after = Some(10);
				socket.faults.stall_duration = Some(Duration::from_millis(stall));
				let mut batch = test_batch(100, 1408, 100 * 1376);
				let stats = socket.send_batch(&mut batch, destination, Some(100_000_000)).await;
				assert_eq!(stats.outcome.submitted_datagrams, 100);
				assert!(stats.backpressure_rebases >= 1);
				assert_eq!(socket.chunk_starts.len(), 3);
				// An overdue third chunk cannot follow the second immediately.
				assert!(socket.chunk_starts[2].duration_since(socket.chunk_starts[1]) >= Duration::from_millis(4));
			}
		}
	}

	#[tokio::test]
	async fn raw_would_block_clears_stale_readiness_and_waits_until_success() {
		use std::cell::Cell;
		use std::future::Future;
		use std::os::fd::OwnedFd;
		use std::os::unix::net::UnixDatagram;
		use std::task::{Context, Poll, Waker};

		// UDP loopback normally drops packets instead of filling the send queue.
		// A Unix datagram pair supplies deterministic kernel backpressure; Tokio's
		// fd readiness tracking is the same. Use only raw I/O on this test fd.
		let (sender, receiver) = UnixDatagram::pair().unwrap();
		sender.set_nonblocking(true).unwrap();
		receiver.set_nonblocking(true).unwrap();
		let socket =
			UdpSocket::from_std(std::net::UdpSocket::from(OwnedFd::from(sender.try_clone().unwrap()))).unwrap();
		let fill = || loop {
			match sender.send(&[0; 1024]) {
				Ok(_) => {},
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
				Err(e) => panic!("filling datagram queue: {e}"),
			}
		};
		let drain = || loop {
			match receiver.recv(&mut [0; 1024]) {
				Ok(_) => {},
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
				Err(e) => panic!("draining datagram queue: {e}"),
			}
		};
		socket.writable().await.unwrap();
		fill();
		let attempts = Cell::new(0);
		let mut would_block = 0;
		let mut send = Box::pin(send_with_readiness(&socket, &mut would_block, || {
			attempts.set(attempts.get() + 1);
			// Fail promptly if a regression starts spinning inside a single poll.
			assert!(attempts.get() <= 16, "raw send spun on stale readiness");
			sender.send(&[1]).map(|_| ())
		}));
		let mut cx = Context::from_waker(Waker::noop());
		for expected_attempts in 2..=4 {
			assert!(send.as_mut().poll(&mut cx).is_pending());
			assert_eq!(attempts.get(), expected_attempts);
			for _ in 0..1000 {
				assert!(send.as_mut().poll(&mut cx).is_pending());
			}
			assert_eq!(
				attempts.get(),
				expected_attempts,
				"retry must await a new readiness event"
			);
			// The failed raw retry cleared Tokio's readiness, not just yielded
			// through cooperative task budgeting.
			let mut invoked = false;
			assert_eq!(
				socket
					.try_io(Interest::WRITABLE, || {
						invoked = true;
						Ok(())
					})
					.unwrap_err()
					.kind(),
				std::io::ErrorKind::WouldBlock
			);
			assert!(!invoked);
			drain();
			socket.writable().await.unwrap();
			if expected_attempts < 4 {
				fill();
			}
		}
		assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
		drop(send);
		assert_eq!(attempts.get(), 5);
		assert_eq!(would_block, 4);
	}

	#[tokio::test]
	async fn cancelling_a_blocked_raw_send_leaves_the_socket_usable() {
		use std::future::Future;
		use std::task::{Context, Waker};

		let (mut socket, receiver, destination) = loopback_socket(false).await;
		socket.socket.writable().await.unwrap();
		let mut would_block = 0;
		let mut send = Box::pin(send_with_readiness(&socket.socket, &mut would_block, || {
			// Inject a false-positive writable event without depending on UDP
			// buffer sizes. The kernel-backpressure test above covers real I/O.
			Err(std::io::ErrorKind::WouldBlock.into())
		}));
		assert!(send.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
		drop(send);
		assert_eq!(would_block, 2);
		let stats = socket.send_batch(&mut test_batch(1, 64, 64), destination, None).await;
		assert_eq!(stats.gso_sends + stats.per_shard_sends, 1);
		let mut buf = [1; 64];
		receiver.recv_from(&mut buf).await.unwrap();
		assert_eq!(buf, [0; 64]);
	}

	#[tokio::test]
	async fn raw_send_fast_path_and_errors_do_not_wait_for_readiness() {
		let (socket, _, _) = loopback_socket(false).await;
		let mut would_block = 0;
		send_with_readiness(&socket.socket, &mut would_block, || Ok(()))
			.await
			.unwrap();
		let error = send_with_readiness(&socket.socket, &mut would_block, || {
			Err(std::io::ErrorKind::InvalidInput.into())
		})
		.await
		.unwrap_err();
		assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
		assert_eq!(would_block, 0);
	}

	#[test]
	fn gso_segment_count_cap_binds() {
		assert_eq!(gso_segments_per_send(8, 100), 8);
	}

	#[test]
	fn gso_payload_cap_binds_at_real_moonlight_size() {
		let shard_size = 1408;
		let segments = gso_segments_per_send(64, shard_size);
		assert_eq!(segments, 46);
		assert_eq!(segments * shard_size, 64768);
		assert!((segments + 1) * shard_size > MAX_UDP_PAYLOAD);
	}

	#[test]
	fn gso_boundary_shard_sizes() {
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD), 1);
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD - 1), 1);
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD + 1), 1);
		assert_eq!(gso_segments_per_send(64, 0), 1);
	}

	#[test]
	fn gso_frame_boundaries_cover_six_through_eight_chunks() {
		let segments = gso_segments_per_send(64, 1408);
		for (shards, chunks, final_segments) in [
			(segments * 6, 6, segments),
			(segments * 7, 7, segments),
			(segments * 7 + 1, 8, 1),
			(segments * 8, 8, segments),
			(segments * 8 - 7, 8, segments - 7),
		] {
			let bytes = vec![0u8; shards * 1408];
			let actual: Vec<_> = bytes.chunks(segments * 1408).map(|c| c.len() / 1408).collect();
			assert_eq!(actual.len(), chunks);
			assert_eq!(*actual.last().unwrap(), final_segments);
			assert!(actual.iter().all(|&count| count <= segments));
		}
	}

	#[test]
	fn packet_sizes_always_respect_udp_payload_cap() {
		for shard_size in [528usize, 1040, 1200, 1408, 1440, 2000] {
			let segments = gso_segments_per_send(64, shard_size);
			assert!(segments * shard_size <= MAX_UDP_PAYLOAD || segments == 1);
		}
	}

	#[test]
	fn pacing_scales_with_fps_without_a_chunk_discontinuity() {
		for fps in [60u64, 90, 120, 144] {
			for bitrate in [
				400_000_000u64,
				424_000_000,
				425_000_000,
				426_000_000,
				500_000_000,
				750_000_000,
				1_000_000_000,
				2_000_000_000,
			] {
				let bytes = (bitrate / fps / 8) as usize;
				let duration = frame_pacing_duration(bytes, bitrate);
				let frame_interval = Duration::from_nanos(1_000_000_000 / fps);
				assert!(duration <= frame_interval);
				assert!(frame_interval - duration < Duration::from_micros(1));
			}
		}
	}

	#[test]
	fn configured_bitrate_crosses_the_same_shard_boundary_at_each_fps() {
		const SHARD_PAYLOAD: u64 = 1376;
		const FRAME_HEADER: u64 = 8;
		const SEVEN_GSO_CHUNKS: u64 = 7 * 46;
		let max_encoded_for_seven = SEVEN_GSO_CHUNKS * SHARD_PAYLOAD - FRAME_HEADER;
		for fps in [60u64, 90, 120, 144] {
			let divisor = fps * 8;
			let last_seven_chunk_bitrate = max_encoded_for_seven * divisor;
			let first_eight_chunk_bitrate = (max_encoded_for_seven + 1) * divisor;
			let shards_below = (last_seven_chunk_bitrate / divisor + FRAME_HEADER).div_ceil(SHARD_PAYLOAD);
			let shards_above = (first_eight_chunk_bitrate / divisor + FRAME_HEADER).div_ceil(SHARD_PAYLOAD);
			assert_eq!(shards_below, 322);
			assert_eq!(shards_above, 323);
		}

		let shards_at_425 = (425_000_000 / (120 * 8) + FRAME_HEADER).div_ceil(SHARD_PAYLOAD);
		let shards_at_426 = (426_000_000 / (120 * 8) + FRAME_HEADER).div_ceil(SHARD_PAYLOAD);
		assert_eq!(shards_at_425, 322);
		assert_eq!(shards_at_426, 323);
	}

	#[test]
	fn pacing_arithmetic_saturates_extreme_inputs() {
		let duration = frame_pacing_duration(usize::MAX, 1);
		assert_eq!(duration, Duration::from_nanos(u64::MAX));
		assert_eq!(frame_pacing_duration(1, 0), Duration::ZERO);
		assert_eq!(scaled_duration(duration, u64::MAX, 1), duration);
	}

	#[test]
	fn encoding_lateness_is_spread_without_an_unbounded_catch_up() {
		let started = Instant::now();
		let requested = started - Duration::from_millis(20);
		let (origin, duration, rebased, lateness) = pacing_schedule(requested, Duration::from_millis(10), started);
		assert_eq!(origin, started);
		assert_eq!(duration, Duration::from_millis(10));
		assert!(rebased);
		assert_eq!(lateness, Duration::from_millis(20));

		let requested = started - Duration::from_millis(5);
		let (origin, duration, rebased, lateness) = pacing_schedule(requested, Duration::from_millis(10), started);
		assert_eq!(origin, started);
		assert_eq!(duration, Duration::from_millis(5));
		assert!(!rebased);
		assert_eq!(lateness, Duration::from_millis(5));
	}

	#[tokio::test]
	async fn gso_bypass_sends_every_shard_individually() {
		let (mut socket, receiver, destination) = loopback_socket(true).await;
		let mut batch = test_batch(47, 1408, 47 * 1376 - 8);
		let stats = socket.send_batch(&mut batch, destination, None).await;
		assert_eq!(stats.gso_sends, 0);
		assert_eq!(stats.per_shard_sends, 47);
		assert_eq!(stats.gso_segments_per_send, 0);

		let mut received = 0;
		let mut buffer = [0u8; 2048];
		while received < 47 {
			tokio::time::timeout(Duration::from_secs(1), receiver.recv_from(&mut buffer))
				.await
				.unwrap()
				.unwrap();
			received += 1;
		}
	}

	#[tokio::test]
	async fn available_gso_segments_a_real_loopback_batch() {
		let (mut socket, receiver, destination) = loopback_socket(false).await;
		let gso_available = socket.udp_state.max_gso_segments() > 1;
		let mut batch = test_batch(47, 1408, 47 * 1376 - 8);
		let stats = socket.send_batch(&mut batch, destination, None).await;
		if gso_available {
			assert_eq!(stats.gso_sends + stats.fallback_chunks, 2);
		} else {
			assert_eq!(stats.per_shard_sends, 47);
		}

		let mut buffer = [0u8; 2048];
		for _ in 0..47 {
			tokio::time::timeout(Duration::from_secs(1), receiver.recv_from(&mut buffer))
				.await
				.unwrap()
				.unwrap();
		}
	}

	#[tokio::test]
	async fn repeated_frame_sends_preserve_every_shard() {
		let (mut socket, receiver, destination) = loopback_socket(false).await;
		let mut batch = test_batch(47, 1408, 47 * 1376 - 8);
		let mut buffer = [0u8; 2048];
		// Drain each frame so UDP receive-buffer overflow cannot make the test
		// depend on scheduling speed. Exercise repeated use of one GSO socket.
		for _ in 0..1024 {
			let stats = socket.send_batch(&mut batch, destination, None).await;
			assert!(stats.gso_sends > 0 || stats.per_shard_sends > 0);
			for _ in 0..47 {
				let (size, _) = tokio::time::timeout(Duration::from_secs(1), receiver.recv_from(&mut buffer))
					.await
					.unwrap()
					.unwrap();
				assert_eq!(size, 1408);
			}
		}
	}

	#[tokio::test]
	async fn runtime_gso_rejection_enters_fallback_path() {
		let (mut socket, _receiver, destination) = loopback_socket(false).await;
		if socket.udp_state.max_gso_segments() <= 1 {
			return;
		}
		// One byte beyond the maximum UDP payload deterministically produces
		// EMSGSIZE. The fallback also fails visibly, but the important invariant
		// here is that rejection enters the per-shard fallback path once.
		let mut batch = test_batch(1, MAX_UDP_PAYLOAD + 1, 1024);
		let stats = socket.send_batch(&mut batch, destination, None).await;
		assert_eq!(stats.gso_sends, 0);
		assert_eq!(stats.fallback_chunks, 1);
	}

	#[tokio::test]
	async fn paced_send_is_cancellation_safe() {
		let (mut socket, _receiver, destination) = loopback_socket(true).await;
		let mut batch = test_batch(47, 1408, 125_000_000);
		batch.set_pacing_origin(Instant::now());
		let result = tokio::time::timeout(
			Duration::from_millis(10),
			socket.send_batch(&mut batch, destination, Some(1_000_000_000)),
		)
		.await;
		assert!(result.is_err(), "the send should be cancelled between pacing quanta");
		// Dropping the paced future leaves the socket usable for shutdown or a
		// subsequent frame; no lock or partially-owned buffer survives it.
		socket.socket.send_to(&[1], destination).await.unwrap();
	}
}
