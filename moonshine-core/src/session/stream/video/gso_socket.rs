use std::net::SocketAddr;
use std::time::{Duration, Instant};

use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use tokio::net::UdpSocket;

use super::shard_batch::ShardBatch;

/// Maximum payload of one UDP datagram (65535 minus IPv4/UDP headers).
const MAX_UDP_PAYLOAD: usize = 65507;
/// Use the common 64-segment GSO cadence for pacing even when GSO is off.
const MAX_PACING_SEGMENTS: usize = 64;

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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SendStats {
	pub gso_sends: u32,
	pub per_shard_sends: u32,
	pub would_block_events: u32,
	pub fallback_chunks: u32,
	pub gso_segments_per_send: usize,
	pub final_chunk_segments: usize,
	pub final_chunk_bytes: usize,
	pub wire_bytes: u64,
	pub elapsed: Duration,
	pub pacing_duration: Duration,
	pub scheduled_pacing_duration: Duration,
	pub initial_pacing_lateness: Duration,
	pub max_pacing_lateness: Duration,
	pub pacing_rebased: bool,
	pub backpressure_rebases: u32,
}

/// UDP socket for video with GSO and frame-aware super-packet pacing.
///
/// A complete intra frame arrives here at once. Submitting all GSO packets
/// back-to-back creates a several-hundred-datagram microburst. GSO is retained,
/// but its chunk boundaries are paced from encoded bytes, negotiated bitrate,
/// and actual per-frame wire bytes.
pub(crate) struct UdpGsoSocket {
	socket: UdpSocket,
	udp_state: UdpSocketState,
	disable_gso: bool,
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
			socket,
			udp_state,
			disable_gso,
		})
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
	pub async fn send_batch(&self, batch: &ShardBatch, addr: SocketAddr, bitrate_bps: Option<u64>) -> SendStats {
		let started = Instant::now();
		let shard_size = batch.shard_size();
		let shard_count = batch.shard_count();
		let mut stats = SendStats {
			wire_bytes: wire_bytes(batch.as_bytes().len(), shard_count, addr),
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
		stats.pacing_duration = pacing_duration;
		let requested_pacing_origin = batch.pacing_origin().unwrap_or(started);
		// A frame whose entire pacing window elapsed during encode is already
		// late. Rebase it instead of releasing every overdue GSO chunk as one
		// catch-up burst. The synchronous PyroWave producer then drops stale
		// captures while this complete frame is sent, bounding queue growth.
		let (mut pacing_origin, scheduled_pacing_duration, pacing_rebased, initial_lateness) =
			pacing_schedule(requested_pacing_origin, pacing_duration, started);
		stats.pacing_rebased = pacing_rebased;
		stats.initial_pacing_lateness = initial_lateness;
		stats.scheduled_pacing_duration = scheduled_pacing_duration;
		let header_size = network_header_size(addr) as u64;
		let mut preceding_wire_bytes = 0u64;

		for chunk in batch.as_bytes().chunks(chunk_bytes) {
			if !scheduled_pacing_duration.is_zero() {
				let deadline =
					pacing_origin + scaled_duration(scheduled_pacing_duration, preceding_wire_bytes, stats.wire_bytes);
				let now = Instant::now();
				if now < deadline {
					tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
				} else {
					let lateness = now.saturating_duration_since(deadline);
					if preceding_wire_bytes == 0 && !stats.pacing_rebased {
						stats.initial_pacing_lateness = lateness;
					}
					stats.max_pacing_lateness = stats.max_pacing_lateness.max(lateness);
				}
			}

			let chunk_segments = chunk.len().div_ceil(shard_size);
			stats.final_chunk_segments = chunk_segments;
			stats.final_chunk_bytes = chunk.len();
			let mut chunk_would_block = false;
			if gso_active {
				let transmit = Transmit {
					destination: addr,
					ecn: None,
					contents: chunk,
					segment_size: Some(shard_size),
					src_ip: None,
				};
				let result = loop {
					match self.udp_state.try_send(UdpSockRef::from(&self.socket), &transmit) {
						Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
							stats.would_block_events = stats.would_block_events.saturating_add(1);
							chunk_would_block = true;
							if let Err(wait_error) = self.socket.writable().await {
								break Err(wait_error);
							}
						},
						other => break other,
					}
				};
				if let Err(e) = result {
					gso_active = false;
					stats.fallback_chunks = stats.fallback_chunks.saturating_add(1);
					tracing::debug!("GSO send failed ({e}), falling back to per-shard sends for this chunk");
					stats.per_shard_sends = stats
						.per_shard_sends
						.saturating_add(self.send_shards(chunk, shard_size, addr).await);
				} else {
					stats.gso_sends = stats.gso_sends.saturating_add(1);
				}
			} else {
				stats.per_shard_sends = stats
					.per_shard_sends
					.saturating_add(self.send_shards(chunk, shard_size, addr).await);
			}

			let chunk_wire_bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX).saturating_add(
				u64::try_from(chunk_segments)
					.unwrap_or(u64::MAX)
					.saturating_mul(header_size),
			);
			// Socket backpressure may consume multiple scheduled chunk slots. Move
			// the remaining schedule forward from the completed chunk instead of
			// releasing every now-overdue GSO send as a catch-up microburst.
			if chunk_would_block && !scheduled_pacing_duration.is_zero() {
				let completed_chunk_deadline_offset =
					scaled_duration(scheduled_pacing_duration, preceding_wire_bytes, stats.wire_bytes);
				let now = Instant::now();
				pacing_origin = now.checked_sub(completed_chunk_deadline_offset).unwrap_or(now);
				stats.backpressure_rebases = stats.backpressure_rebases.saturating_add(1);
			}
			preceding_wire_bytes = preceding_wire_bytes.saturating_add(chunk_wire_bytes);
		}
		stats.elapsed = started.elapsed();
		stats
	}

	async fn send_shards(&self, bytes: &[u8], shard_size: usize, addr: SocketAddr) -> u32 {
		if shard_size == 0 {
			return 0;
		}
		let mut sent = 0u32;
		for shard in bytes.chunks(shard_size) {
			match self.socket.send_to(shard, addr).await {
				Ok(_) => sent = sent.saturating_add(1),
				Err(e) => tracing::warn!("Failed to send packet to client: {e}"),
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
				socket: raw_socket,
				udp_state,
				disable_gso,
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
		let (socket, receiver, destination) = loopback_socket(true).await;
		let batch = test_batch(47, 1408, 47 * 1376 - 8);
		let stats = socket.send_batch(&batch, destination, None).await;
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
		let (socket, receiver, destination) = loopback_socket(false).await;
		let gso_available = socket.udp_state.max_gso_segments() > 1;
		let batch = test_batch(47, 1408, 47 * 1376 - 8);
		let stats = socket.send_batch(&batch, destination, None).await;
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
	async fn runtime_gso_rejection_enters_fallback_path() {
		let (socket, _receiver, destination) = loopback_socket(false).await;
		if socket.udp_state.max_gso_segments() <= 1 {
			return;
		}
		// One byte beyond the maximum UDP payload deterministically produces
		// EMSGSIZE. The fallback also fails visibly, but the important invariant
		// here is that rejection enters the per-shard fallback path once.
		let batch = test_batch(1, MAX_UDP_PAYLOAD + 1, 1024);
		let stats = socket.send_batch(&batch, destination, None).await;
		assert_eq!(stats.gso_sends, 0);
		assert_eq!(stats.fallback_chunks, 1);
	}

	#[tokio::test]
	async fn paced_send_is_cancellation_safe() {
		let (socket, _receiver, destination) = loopback_socket(true).await;
		let mut batch = test_batch(47, 1408, 125_000_000);
		batch.set_pacing_origin(Instant::now());
		let result = tokio::time::timeout(
			Duration::from_millis(10),
			socket.send_batch(&batch, destination, Some(1_000_000_000)),
		)
		.await;
		assert!(result.is_err(), "the send should be cancelled between pacing quanta");
		// Dropping the paced future leaves the socket usable for shutdown or a
		// subsequent frame; no lock or partially-owned buffer survives it.
		socket.socket.send_to(&[1], destination).await.unwrap();
	}
}
