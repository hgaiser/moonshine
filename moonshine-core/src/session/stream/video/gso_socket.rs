use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use tokio::net::UdpSocket;

use super::pacer::Pacer;
use super::shard_batch::ShardBatch;

/// Maximum payload of one UDP datagram (65535 minus IPv4/UDP headers).
/// A GSO batch is still one datagram: the kernel rejects anything larger
/// with EMSGSIZE before segmenting. IPv6 allows 20 more bytes; the IPv4
/// value is safe for both.
const MAX_UDP_PAYLOAD: usize = 65507;

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
pub(crate) struct UdpGsoSocket {
	socket: UdpSocket,
	udp_state: UdpSocketState,
	disable_gso: bool,
	pacer: Option<Pacer>,
}

impl UdpGsoSocket {
	/// Bind a socket to `address:port` and initialize its GSO state.
	pub async fn new(address: &str, port: u16) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;
		let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
			.map_err(|e| tracing::error!("Failed to initialize UDP socket state: {e}"))?;
		let disable_gso = std::env::var_os("MOONSHINE_DISABLE_GSO").is_some();
		if disable_gso {
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
			pacer: None,
		})
	}

	/// Configure pacing from the bitrate and frame rate negotiated with the
	/// client. A zero headroom percentage leaves pacing disabled.
	pub fn configure_pacing(
		&mut self,
		client_bitrate_bps: usize,
		fps: u32,
		headroom_percent: u16,
		frame_budget_percent: u8,
	) {
		if client_bitrate_bps == 0 || fps == 0 || headroom_percent == 0 {
			tracing::info!("Video packet pacing disabled");
			return;
		}

		match Pacer::new(client_bitrate_bps, fps, headroom_percent, frame_budget_percent) {
			Ok(pacer) => {
				self.pacer = Some(pacer);
				tracing::info!(
					client_bitrate_mbps = client_bitrate_bps / 1_000_000,
					headroom_percent,
					frame_budget_percent = frame_budget_percent.clamp(1, 100),
					"Adaptive video packet pacing enabled"
				);
			},
			Err(e) => tracing::warn!("Failed to initialize high-resolution video pacer: {e}"),
		}
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
	/// GSO availability is re-checked on every call: quinn-udp disables it at
	/// runtime if the kernel or NIC rejects a segmented send.
	pub async fn send_batch(&mut self, batch: &ShardBatch, addr: std::net::SocketAddr) -> SendBatchResult {
		let started = std::time::Instant::now();
		let shard_size = batch.shard_size();
		let shard_count = batch.shard_count();
		let mut result = SendBatchResult {
			shard_count,
			..SendBatchResult::default()
		};
		if shard_size == 0 || shard_count == 0 {
			return result;
		}

		let max_gso_segments = if self.disable_gso {
			1
		} else {
			self.udp_state.max_gso_segments()
		};
		let max_segments_per_send = gso_segments_per_send(max_gso_segments, shard_size);
		let pacing_deadlines = self
			.pacer
			.as_ref()
			.map(|pacer| (pacer.frame_budget(), pacer.frame_interval()));
		let mut schedule = self
			.pacer
			.as_ref()
			.map(|pacer| pacer.schedule_batch(shard_size, shard_count, max_segments_per_send));
		let segments_per_send = schedule
			.as_ref()
			.map_or(max_segments_per_send, |schedule| schedule.segments_per_send);
		if let Some(schedule) = &schedule {
			result.pacing_interval = Some(schedule.pacing_interval);
			result.deadline_clamped = schedule.deadline_clamped;
		}

		let mut pacing_failed = false;
		for chunk in batch.as_bytes().chunks(segments_per_send * shard_size) {
			if !pacing_failed && let (Some(pacer), Some(schedule)) = (&mut self.pacer, &mut schedule) {
				if let Err(e) = pacer.wait(schedule).await {
					tracing::warn!("High-resolution video pacer failed; disabling pacing: {e}");
					pacing_failed = true;
				}
				schedule.advance(chunk.len().div_ceil(shard_size));
			}

			let (failed_gso, send_errors) =
				send_gso_chunk(&self.socket, &self.udp_state, chunk, shard_size, addr).await;
			result.failed_gso_chunks += u32::from(failed_gso);
			result.send_errors += send_errors;
		}

		if pacing_failed {
			self.pacer = None;
		}
		result.elapsed = started.elapsed();
		if let Some((pacing_budget, frame_deadline)) = pacing_deadlines {
			result.pacing_budget_missed = result.elapsed > pacing_budget;
			result.frame_deadline_missed = result.elapsed > frame_deadline;
		}
		result
	}
}

/// Send one GSO chunk. If the kernel rejects segmentation, retry each shard so
/// the frame is not silently discarded.
async fn send_gso_chunk(
	socket: &UdpSocket,
	udp_state: &UdpSocketState,
	chunk: &[u8],
	shard_size: usize,
	addr: std::net::SocketAddr,
) -> (bool, u32) {
	let transmit = Transmit {
		destination: addr,
		ecn: None,
		contents: chunk,
		segment_size: Some(shard_size),
		src_ip: None,
	};
	let send_result = loop {
		match udp_state.try_send(UdpSockRef::from(socket), &transmit) {
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
				socket.writable().await.ok();
			},
			other => break other,
		}
	};
	match send_result {
		Ok(()) => (false, 0),
		Err(e) => {
			tracing::debug!("GSO send failed ({e}), falling back to per-shard sends for this chunk");
			(true, send_shards(socket, chunk, shard_size, addr).await)
		},
	}
}

async fn send_shards(socket: &UdpSocket, bytes: &[u8], shard_size: usize, addr: std::net::SocketAddr) -> u32 {
	let mut send_errors = 0u32;
	for shard in bytes.chunks(shard_size) {
		if let Err(e) = socket.send_to(shard, addr).await {
			tracing::warn!("Failed to send packet to client: {e}");
			send_errors += 1;
		}
	}
	send_errors
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
}
