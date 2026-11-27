use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use tokio::net::UdpSocket;

use super::pacing::{PACED_MAX_GSO_SEGMENTS, PacketPacer, chunk_wire_bytes, is_ipv6_wire};
use super::shard_batch::ShardBatch;

/// Maximum payload of one UDP datagram (65535 minus IPv4/UDP headers).
/// A GSO batch is still one datagram: the kernel rejects anything larger
/// with EMSGSIZE before segmenting. IPv6 allows 20 more bytes; the IPv4
/// value is safe for both.
const MAX_UDP_PAYLOAD: usize = 65507;

/// Number of shards per GSO send: the kernel's segment-count cap or the
/// datagram-size cap, whichever binds first. Never zero, even for a shard
/// larger than a datagram (such a chunk fails at send time and falls back
/// to per-shard sends, which fail visibly instead of silently).
fn gso_segments_per_send(max_gso_segments: usize, shard_size: usize) -> usize {
	max_gso_segments
		.min(MAX_UDP_PAYLOAD.checked_div(shard_size).unwrap_or(0))
		.max(1)
}

/// The usable size of a send buffer whose `SO_SNDBUF` reads as `reported`:
/// the kernel reports double, keeping the other half for bookkeeping.
fn usable_send_buffer(reported: usize) -> usize {
	reported / 2
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
	pacer: Option<PacketPacer>,
}

impl UdpGsoSocket {
	/// Bind a socket to `address:port` and initialize its GSO state.
	pub async fn new(address: &str, port: u16) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port))
			.await
			.map_err(|e| tracing::error!("Failed to bind to UDP socket: {e}"))?;
		let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
			.map_err(|e| tracing::error!("Failed to initialize UDP socket state: {e}"))?;
		if udp_state.max_gso_segments() > 1 {
			tracing::debug!("GSO enabled, max segments: {}", udp_state.max_gso_segments());
		} else {
			tracing::debug!("GSO not available, using per-shard sends");
		}
		Ok(Self {
			socket,
			udp_state,
			pacer: None,
		})
	}

	/// Configure or disable packet pacing on this socket.
	///
	/// Returns the active wire pacing rate in bytes/sec, or `None` when pacing is off.
	pub fn set_pacing(&mut self, bytes_per_sec: Option<u64>) -> Option<u64> {
		if self.pacer.as_ref().map(PacketPacer::bytes_per_sec) == bytes_per_sec {
			return bytes_per_sec;
		}

		self.pacer = match bytes_per_sec {
			Some(rate) => match PacketPacer::new(rate) {
				Ok(pacer) => Some(pacer),
				Err(e) => {
					tracing::warn!("Failed to initialize video packet pacer: {e}");
					None
				},
			},
			None => None,
		};

		self.pacer.as_ref().map(PacketPacer::bytes_per_sec)
	}

	/// Local address the socket is bound to.
	pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
		self.socket.local_addr()
	}

	/// Apply QoS marking (IPv4 ToS) to sent packets.
	pub fn set_tos_v4(&self, tos: u32) -> std::io::Result<()> {
		self.socket.set_tos_v4(tos)
	}

	/// Grow the send buffer to `size` bytes if it is smaller, so a frame burst
	/// fits without the sender waiting for the socket to drain.
	///
	/// Best effort: a buffer the host already made larger is kept, and the
	/// kernel caps the request at `net.core.wmem_max`. Logs what it grants.
	pub fn grow_send_buffer(&self, size: usize) {
		let socket = socket2::SockRef::from(&self.socket);
		let current = match socket.send_buffer_size() {
			Ok(reported) => usable_send_buffer(reported),
			Err(e) => {
				tracing::warn!("Failed to read video socket send buffer size: {e}");
				return;
			},
		};
		if current >= size {
			tracing::debug!("Video socket send buffer: {} KiB", current / 1024);
			return;
		}
		self.set_send_buffer(size);
	}

	/// Set the send buffer to `size` bytes, even below the host's default.
	///
	/// Best effort: the kernel caps the request at `net.core.wmem_max`. Logs what
	/// it grants.
	pub fn set_send_buffer(&self, size: usize) {
		let socket = socket2::SockRef::from(&self.socket);
		if let Err(e) = socket.set_send_buffer_size(size) {
			tracing::warn!("Failed to set video socket send buffer size: {e}");
			return;
		}
		match socket.send_buffer_size().map(usable_send_buffer) {
			Ok(granted) if granted < size => tracing::info!(
				"Video socket send buffer capped at {} KiB (requested {} KiB); raise net.core.wmem_max so \
				 large frames don't wait for the socket to drain.",
				granted / 1024,
				size / 1024
			),
			Ok(granted) => tracing::debug!("Video socket send buffer: {} KiB", granted / 1024),
			Err(e) => tracing::warn!("Failed to read video socket send buffer size: {e}"),
		}
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
	pub async fn send_batch(&mut self, batch: &ShardBatch, addr: std::net::SocketAddr) -> u32 {
		if self.udp_state.max_gso_segments() > 1 {
			self.send_batch_gso(batch, addr).await
		} else {
			self.send_shards_paced(batch.as_bytes(), batch.shard_size(), addr).await;
			0
		}
	}

	/// Pace a chunk of `chunk_len` UDP payload bytes when packet pacing is active.
	async fn pace_chunk(&mut self, chunk_len: usize, shard_size: usize, addr: std::net::SocketAddr) {
		if let Some(pacer) = self.pacer.as_mut() {
			let wire_bytes = chunk_wire_bytes(chunk_len, shard_size, is_ipv6_wire(addr));
			pacer.pace(wire_bytes).await;
		}
	}

	/// Send a batch as GSO chunks sized to the kernel's segment and datagram
	/// caps (and the active pacer's segment cap, if any); a frame's batch
	/// routinely exceeds these caps. Uses `try_send` because `send` masks every
	/// error except `WouldBlock` as success, silently discarding the batch.
	async fn send_batch_gso(&mut self, batch: &ShardBatch, addr: std::net::SocketAddr) -> u32 {
		let shard_size = batch.shard_size();
		if shard_size == 0 {
			return 0;
		}
		let max_gso = self.udp_state.max_gso_segments();
		let effective_max_gso = if self.pacer.is_some() {
			max_gso.min(PACED_MAX_GSO_SEGMENTS)
		} else {
			max_gso
		};
		let segments_per_send = gso_segments_per_send(effective_max_gso, shard_size);

		let mut failed_chunks = 0u32;
		for chunk in batch.as_bytes().chunks(segments_per_send * shard_size) {
			self.pace_chunk(chunk.len(), shard_size, addr).await;
			let transmit = Transmit {
				destination: addr,
				ecn: None,
				contents: chunk,
				segment_size: Some(shard_size),
				src_ip: None,
			};
			// Use Tokio's readiness-aware raw-I/O wrapper: a GSO send can
			// return WouldBlock when the socket buffer is full, and async_io
			// clears stale writable readiness before waiting again. Simply
			// awaiting writable() after a raw try_send would spin on that flag.
			let result = self
				.socket
				.async_io(tokio::io::Interest::WRITABLE, || {
					self.udp_state.try_send(UdpSockRef::from(&self.socket), &transmit)
				})
				.await;
			if let Err(e) = result {
				failed_chunks += 1;
				tracing::debug!("GSO send failed ({e}), falling back to per-shard sends for this chunk");
				// This chunk was already paced above; send its shards directly.
				self.send_shards_unpaced(chunk, shard_size, addr).await;
			}
		}
		failed_chunks
	}

	/// Send a contiguous buffer of equal-sized shards as individual UDP packets,
	/// applying userspace pacing in bounded groups when a pacer is active.
	async fn send_shards_paced(&mut self, bytes: &[u8], shard_size: usize, addr: std::net::SocketAddr) {
		if shard_size == 0 {
			return;
		}
		if self.pacer.is_some() {
			for chunk in bytes.chunks(PACED_MAX_GSO_SEGMENTS * shard_size) {
				self.pace_chunk(chunk.len(), shard_size, addr).await;
				self.send_shards_unpaced(chunk, shard_size, addr).await;
			}
		} else {
			self.send_shards_unpaced(bytes, shard_size, addr).await;
		}
	}

	/// Send a contiguous buffer of equal-sized shards as individual UDP packets.
	async fn send_shards_unpaced(&self, bytes: &[u8], shard_size: usize, addr: std::net::SocketAddr) {
		if shard_size == 0 {
			return;
		}
		for shard in bytes.chunks(shard_size) {
			if let Err(e) = self.socket.send_to(shard, addr).await {
				tracing::warn!("Failed to send packet to client: {e}");
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use super::*;

	fn usable_send_buffer_size(socket: &UdpGsoSocket) -> usize {
		usable_send_buffer(socket2::SockRef::from(&socket.socket).send_buffer_size().unwrap())
	}

	fn synthetic_batch(shard_count: usize, shard_size: usize) -> ShardBatch {
		let mut data = vec![0u8; shard_count * shard_size];
		for i in 0..shard_count {
			let offset = i * shard_size;
			data[offset..offset + 4].copy_from_slice(&(i as u32).to_le_bytes());
		}
		ShardBatch::from_raw(data, shard_size)
	}

	#[tokio::test]
	async fn grow_send_buffer_never_shrinks() {
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let initial = usable_send_buffer_size(&socket);

		socket.grow_send_buffer(initial / 2);
		assert_eq!(usable_send_buffer_size(&socket), initial);

		// The kernel may cap the growth, but never below what we had.
		socket.grow_send_buffer(initial * 4);
		assert!(usable_send_buffer_size(&socket) >= initial);
	}

	#[tokio::test]
	async fn set_send_buffer_can_shrink() {
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let initial = usable_send_buffer_size(&socket);

		socket.set_send_buffer(initial / 4);
		assert!(usable_send_buffer_size(&socket) < initial);
	}

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

	#[tokio::test]
	async fn paced_send_batch_delivers_all_shards_at_target_rate() {
		let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let dest = receiver.local_addr().unwrap();
		// Grow receiver buffer so loopback never drops shards during the test.
		let _ = socket2::SockRef::from(&receiver).set_recv_buffer_size(4 * 1024 * 1024);

		let mut sender = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let bytes_per_sec = 6_250_000; // 50 Mb/s
		assert_eq!(sender.set_pacing(Some(bytes_per_sec)), Some(bytes_per_sec));

		let shard_count = 96usize; // 24 chunks of 4 shards (~135 KiB payload)
		let shard_size = 1408usize;
		let batch = synthetic_batch(shard_count, shard_size);

		let recv_task = tokio::spawn(async move {
			let mut buf = vec![0u8; shard_size + 64];
			let mut received = Vec::with_capacity(shard_count);
			for _ in 0..shard_count {
				let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
					.await
					.expect("timed out waiting for paced shard")
					.unwrap();
				assert_eq!(len, shard_size);
				let idx = u32::from_le_bytes(buf[..4].try_into().unwrap());
				received.push(idx);
			}
			received
		});

		let start = Instant::now();
		let failed = sender.send_batch(&batch, dest).await;
		let elapsed = start.elapsed();

		assert_eq!(failed, 0);
		let received = recv_task.await.unwrap();
		let expected_indices: Vec<u32> = (0..shard_count as u32).collect();
		assert_eq!(received, expected_indices);

		// 23 inter-chunk gaps * 4 * (1408 + 66) = 135,608 wire bytes at 6.25 MB/s ≈ 21.7 ms.
		let wire_chunk = chunk_wire_bytes(PACED_MAX_GSO_SEGMENTS * shard_size, shard_size, false);
		let expected_ms = (23 * wire_chunk) as f64 / bytes_per_sec as f64 * 1000.0;
		let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
		assert!(
			elapsed_ms >= expected_ms * 0.85 && elapsed_ms <= expected_ms * 2.0,
			"expected ~{expected_ms:.2} ms for paced batch, got {elapsed_ms:.2} ms"
		);
	}

	#[tokio::test]
	async fn paced_send_batch_cancels_promptly() {
		let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let dest = receiver.local_addr().unwrap();

		let mut sender = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		sender.set_pacing(Some(1_250_000)); // 10 Mb/s -> ~180 ms for 160 shards
		let batch = synthetic_batch(160, 1408);

		let start = Instant::now();
		let result = tokio::time::timeout(Duration::from_millis(20), sender.send_batch(&batch, dest)).await;
		let elapsed = start.elapsed();

		assert!(result.is_err(), "slow paced batch should be cancelled by timeout");
		assert!(
			elapsed < Duration::from_millis(60),
			"cancellation took too long: {elapsed:?}"
		);
	}
}
