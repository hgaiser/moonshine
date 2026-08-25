use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use tokio::net::UdpSocket;

use super::shard_batch::ShardBatch;

/// Maximum payload of one UDP datagram (65535 minus IPv4/UDP headers).
/// A GSO batch is still one datagram: the kernel rejects anything larger
/// with EMSGSIZE before segmenting. IPv6 allows 20 more bytes; the IPv4
/// value is safe for both.
const MAX_UDP_PAYLOAD: usize = 65507;

/// Approximate bytes added to each UDP payload on a VLAN Ethernet path:
/// IPv4 + UDP + Ethernet/VLAN/FCS + preamble/SFD + inter-packet gap.
const WIRE_OVERHEAD_BYTES: usize = 70;

#[derive(Clone, Copy, Debug)]
enum PacingMode {
	Disabled,
	Fixed(std::time::Duration),
	Adaptive {
		wire_rate_bps: u64,
		frame_interval: std::time::Duration,
		frame_budget_percent: u8,
	},
}

#[derive(Debug, Default)]
pub(crate) struct SendBatchResult {
	pub failed_gso_chunks: u32,
	pub send_errors: u32,
	pub shard_count: usize,
	pub elapsed: std::time::Duration,
	pub pacing_interval: Option<std::time::Duration>,
	pub deadline_clamped: bool,
	pub pacing_budget_missed: bool,
	pub frame_deadline_missed: bool,
}

fn adaptive_pacing_interval(
	shard_size: usize,
	shard_count: usize,
	wire_rate_bps: u64,
	frame_interval: std::time::Duration,
	frame_budget_percent: u8,
) -> (std::time::Duration, bool) {
	if shard_size == 0 || shard_count <= 1 || wire_rate_bps == 0 {
		return (std::time::Duration::ZERO, false);
	}

	let wire_bits = (shard_size.saturating_add(WIRE_OVERHEAD_BYTES) as u128) * 8;
	let rate_interval_ns = wire_bits.saturating_mul(1_000_000_000).div_ceil(wire_rate_bps as u128);
	let gaps = (shard_count - 1) as u128;
	let budget_ns = frame_interval
		.as_nanos()
		.saturating_mul(frame_budget_percent.clamp(1, 100) as u128)
		/ 100;
	let deadline_interval_ns = budget_ns / gaps;
	let clamped = rate_interval_ns > deadline_interval_ns;
	let interval_ns = rate_interval_ns.min(deadline_interval_ns).min(u64::MAX as u128) as u64;
	(std::time::Duration::from_nanos(interval_ns), clamped)
}

/// Number of shards per GSO send: the kernel's segment-count cap or the
/// datagram-size cap, whichever binds first. Never zero, even for a shard
/// larger than a datagram (such a chunk fails at send time and falls back
/// to per-shard sends, which fail visibly instead of silently).
fn gso_segments_per_send(max_gso_segments: usize, shard_size: usize) -> usize {
	max_gso_segments
		.min(MAX_UDP_PAYLOAD.checked_div(shard_size).unwrap_or(0))
		.max(1)
}

/// UDP socket for the video stream, with Generic Segmentation Offload.
///
/// Wraps a `tokio::net::UdpSocket` and quinn-udp's `UdpSocketState` so shard
/// batches can be sent as GSO super-packets sized to the kernel's segment-count
/// and datagram-size caps. Batches exceeding a cap are split into chunks; chunks
/// the kernel rejects fall back to per-shard sends. Without GSO support, the
/// socket sends everything per-shard.
pub(crate) struct UdpGsoSocket {
	socket: UdpSocket,
	udp_state: UdpSocketState,
	disable_gso: bool,
	pacing: PacingMode,
}

impl UdpGsoSocket {
	fn pacing_deadlines(&self) -> Option<(std::time::Duration, std::time::Duration)> {
		match self.pacing {
			PacingMode::Adaptive {
				frame_interval,
				frame_budget_percent,
				..
			} => Some((
				frame_interval.mul_f64(f64::from(frame_budget_percent) / 100.0),
				frame_interval,
			)),
			PacingMode::Disabled | PacingMode::Fixed(_) => None,
		}
	}

	/// Bind a socket to `address:port` and initialize its GSO state.
	pub async fn new(address: &str, port: u16) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;
		let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
			.map_err(|e| tracing::error!("Failed to initialize UDP socket state: {e}"))?;
		let fixed_pacing = std::env::var("MOONSHINE_SHARD_PACING_US")
			.ok()
			.and_then(|value| value.parse::<u64>().ok())
			.filter(|value| *value > 0)
			.map(std::time::Duration::from_micros);
		let disable_gso = std::env::var_os("MOONSHINE_DISABLE_GSO").is_some() || fixed_pacing.is_some();
		let pacing = fixed_pacing.map_or(PacingMode::Disabled, PacingMode::Fixed);
		if let Some(interval) = fixed_pacing {
			tracing::info!(
				pacing_us = interval.as_micros() as u64,
				"GSO disabled; pacing video shards"
			);
		} else if disable_gso {
			tracing::info!("GSO disabled by MOONSHINE_DISABLE_GSO");
		} else if udp_state.max_gso_segments() > 1 {
			tracing::debug!("GSO enabled, max segments: {}", udp_state.max_gso_segments());
		} else {
			tracing::debug!("GSO not available, using per-shard sends");
		}
		Ok(Self {
			socket,
			udp_state,
			disable_gso,
			pacing,
		})
	}

	/// Configure adaptive per-shard pacing for the negotiated stream. Environment
	/// test controls take precedence so fixed-delay A/B runs remain reproducible.
	pub fn configure_adaptive_pacing(&mut self, wire_rate_mbps: u32, fps: u32, frame_budget_percent: u8) {
		if !matches!(self.pacing, PacingMode::Disabled) || wire_rate_mbps == 0 || fps == 0 {
			return;
		}

		self.disable_gso = true;
		self.pacing = PacingMode::Adaptive {
			wire_rate_bps: u64::from(wire_rate_mbps) * 1_000_000,
			frame_interval: std::time::Duration::from_secs_f64(1.0 / f64::from(fps)),
			frame_budget_percent: frame_budget_percent.clamp(1, 100),
		};
		tracing::info!(
			wire_rate_mbps,
			fps,
			frame_budget_percent = frame_budget_percent.clamp(1, 100),
			"GSO disabled; adaptive video shard pacing enabled"
		);
	}

	/// Local address the socket is bound to.
	pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
		self.socket.local_addr()
	}

	/// Apply QoS marking (IPv4 ToS) to sent packets.
	pub fn set_tos_v4(&self, tos: u32) -> std::io::Result<()> {
		self.socket.set_tos_v4(tos)
	}

	/// Receive a datagram (e.g. the client's `PING`).
	pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, std::net::SocketAddr)> {
		self.socket.recv_from(buf).await
	}

	/// Send a shard batch to `addr`.
	///
	/// Returns the number of chunks that fell back to per-shard sends because
	/// the kernel rejected the GSO send (0 on success).
	///
	/// GSO availability is re-checked on every call: quinn-udp disables it at
	/// runtime if the kernel or NIC rejects a segmented send.
	pub async fn send_batch(&self, batch: &ShardBatch, addr: std::net::SocketAddr) -> SendBatchResult {
		let started = tokio::time::Instant::now();
		let mut result = SendBatchResult {
			shard_count: batch.shard_count(),
			..SendBatchResult::default()
		};
		if !self.disable_gso && self.udp_state.max_gso_segments() > 1 {
			let (failed_gso_chunks, send_errors) = self.send_batch_gso(batch, addr).await;
			result.failed_gso_chunks = failed_gso_chunks;
			result.send_errors = send_errors;
		} else {
			let (send_errors, pacing_interval, deadline_clamped) =
				self.send_shards(batch.as_bytes(), batch.shard_size(), addr).await;
			result.send_errors = send_errors;
			result.pacing_interval = pacing_interval;
			result.deadline_clamped = deadline_clamped;
		}
		result.elapsed = started.elapsed();
		if let Some((pacing_budget, frame_deadline)) = self.pacing_deadlines() {
			result.pacing_budget_missed = result.elapsed > pacing_budget;
			result.frame_deadline_missed = result.elapsed > frame_deadline;
		}
		result
	}

	/// Send a batch as GSO chunks sized to the kernel's segment and datagram
	/// caps; a frame's batch routinely exceeds both. Uses `try_send` because
	/// `send` masks every error except `WouldBlock` as success, silently
	/// discarding the batch.
	async fn send_batch_gso(&self, batch: &ShardBatch, addr: std::net::SocketAddr) -> (u32, u32) {
		let shard_size = batch.shard_size();
		if shard_size == 0 {
			return (0, 0);
		}
		let segments_per_send = gso_segments_per_send(self.udp_state.max_gso_segments(), shard_size);

		let mut failed_chunks = 0u32;
		let mut send_errors = 0u32;
		for chunk in batch.as_bytes().chunks(segments_per_send * shard_size) {
			let transmit = Transmit {
				destination: addr,
				ecn: None,
				contents: chunk,
				segment_size: Some(shard_size),
				src_ip: None,
			};
			let result = loop {
				match self.udp_state.try_send(UdpSockRef::from(&self.socket), &transmit) {
					// A frame burst can outrun the socket buffer; wait for it to
					// drain and resend the same chunk instead of degrading.
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
						self.socket.writable().await.ok();
					},
					other => break other,
				}
			};
			if let Err(e) = result {
				failed_chunks += 1;
				tracing::debug!("GSO send failed ({e}), falling back to per-shard sends for this chunk");
				let (errors, _, _) = self.send_shards(chunk, shard_size, addr).await;
				send_errors += errors;
			}
		}
		(failed_chunks, send_errors)
	}

	/// Send a contiguous buffer of equal-sized shards as individual UDP packets.
	async fn send_shards(
		&self,
		bytes: &[u8],
		shard_size: usize,
		addr: std::net::SocketAddr,
	) -> (u32, Option<std::time::Duration>, bool) {
		if shard_size == 0 {
			return (0, None, false);
		}
		let shard_count = bytes.len().div_ceil(shard_size);
		let (pacing_interval, deadline_clamped) = match self.pacing {
			PacingMode::Disabled => (None, false),
			PacingMode::Fixed(interval) => (Some(interval), false),
			PacingMode::Adaptive {
				wire_rate_bps,
				frame_interval,
				frame_budget_percent,
			} => {
				let (interval, clamped) = adaptive_pacing_interval(
					shard_size,
					shard_count,
					wire_rate_bps,
					frame_interval,
					frame_budget_percent,
				);
				(Some(interval), clamped)
			},
		};
		let mut next_send = tokio::time::Instant::now();
		let mut send_errors = 0u32;
		for (index, shard) in bytes.chunks(shard_size).enumerate() {
			if let Err(e) = self.socket.send_to(shard, addr).await {
				tracing::warn!("Failed to send packet to client: {e}");
				send_errors += 1;
			}
			if let Some(interval) = pacing_interval.filter(|_| index + 1 < shard_count) {
				next_send += interval;
				tokio::time::sleep_until(next_send).await;
			}
		}
		(send_errors, pacing_interval, deadline_clamped)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gso_segment_count_cap_binds() {
		// Payload cap (65507 / 100 = 655) is looser than the segment cap.
		assert_eq!(gso_segments_per_send(8, 100), 8);
	}

	#[test]
	fn gso_payload_cap_binds() {
		// 65507 / 2000 = 32, tighter than the kernel's 64-segment cap.
		assert_eq!(gso_segments_per_send(64, 2000), 32);
	}

	#[test]
	fn gso_boundary_shard_sizes() {
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD), 1);
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD - 1), 1);
		// A shard that cannot fit a datagram at all still yields 1, not 0.
		assert_eq!(gso_segments_per_send(64, MAX_UDP_PAYLOAD + 1), 1);
		assert_eq!(gso_segments_per_send(64, 0), 1);
	}

	#[test]
	fn gso_chunks_respect_kernel_limits() {
		// Shard counts chosen to force a partial final chunk.
		for (shard_count, shard_size) in [(250usize, 1404usize), (47, 1404), (100, 733)] {
			let segments = gso_segments_per_send(64, shard_size);
			let data = vec![0u8; shard_count * shard_size];
			for chunk in data.chunks(segments * shard_size) {
				assert_eq!(chunk.len() % shard_size, 0, "chunk must hold whole shards");
				assert!(chunk.len() <= MAX_UDP_PAYLOAD, "chunk must fit one datagram");
				assert!(chunk.len() / shard_size <= 64, "chunk must respect segment cap");
			}
		}
	}

	#[test]
	fn adaptive_pacing_targets_configured_wire_rate() {
		let (interval, clamped) = adaptive_pacing_interval(
			1392,
			100,
			120_000_000,
			std::time::Duration::from_secs_f64(1.0 / 60.0),
			80,
		);
		assert_eq!(interval, std::time::Duration::from_nanos(97_467));
		assert!(!clamped);
	}

	#[test]
	fn adaptive_pacing_honors_frame_deadline() {
		let frame_interval = std::time::Duration::from_millis(10);
		let (interval, clamped) = adaptive_pacing_interval(1392, 101, 1_000_000, frame_interval, 80);
		assert_eq!(interval, std::time::Duration::from_micros(80));
		assert!(clamped);
	}

	#[test]
	fn adaptive_pacing_handles_degenerate_batches() {
		let frame_interval = std::time::Duration::from_millis(16);
		assert_eq!(
			adaptive_pacing_interval(1392, 1, 120_000_000, frame_interval, 80),
			(std::time::Duration::ZERO, false)
		);
		assert_eq!(
			adaptive_pacing_interval(1392, 10, 0, frame_interval, 80),
			(std::time::Duration::ZERO, false)
		);
	}
}
