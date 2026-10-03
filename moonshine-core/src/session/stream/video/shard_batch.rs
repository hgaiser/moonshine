use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

/// One encoder submission, held until transport release (or any earlier error).
pub(crate) struct NetworkCredit(pub Arc<AtomicUsize>);
impl Drop for NetworkCredit {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::Relaxed);
	}
}

use std::time::Instant;

/// Output queue storage, including the batch currently borrowed by the sender.
#[derive(Default)]
pub(crate) struct QueueDepth {
	frames: AtomicUsize,
	bytes: AtomicUsize,
	high_frames: AtomicUsize,
	high_bytes: AtomicUsize,
}

struct QueueStorage {
	depth: Arc<QueueDepth>,
	bytes: usize,
}
impl Drop for QueueStorage {
	fn drop(&mut self) {
		self.depth.frames.fetch_sub(1, Ordering::Relaxed);
		self.depth.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
	}
}

/// UDP kernel submission is not receiver delivery. Attempts count logical
/// datagrams once, excluding readiness retries and GSO fallback duplication.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TransportOutcome {
	pub attempted_datagrams: usize,
	pub attempted_payload_bytes: usize,
	pub submitted_datagrams: usize,
	pub submitted_payload_bytes: usize,
	pub failed_datagrams: usize,
	pub last_error: Option<std::io::ErrorKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionDisposition {
	Submitted,
	Failed,
	Discarded,
	Cancelled,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TransportCompletion {
	pub finished_at: Instant,
	pub send_started_at: Option<Instant>,
	pub outcome: TransportOutcome,
	pub disposition: CompletionDisposition,
	pub discarded_datagrams: usize,
	pub discarded_payload_bytes: usize,
}

/// A batch of equal-sized network shards stored in a single contiguous buffer.
///
/// Replaces `Vec<Vec<u8>>` to avoid one heap allocation per shard
/// (~50–100 per frame). All shards are packed into a single `Vec<u8>`
/// and accessed via `chunks_exact(shard_size)`.
pub(crate) struct ShardBatch {
	/// Contiguous buffer holding `shard_count * shard_size` bytes.
	data: Vec<u8>,
	/// Size of each shard in bytes.
	shard_size: usize,
	/// Transport metadata used for pacing and low-overhead frame diagnostics.
	frame_number: u32,
	encoded_size: usize,
	data_shards: usize,
	parity_shards: usize,
	fec_blocks: usize,
	/// Capture timestamp used as the origin of frame-aware transport pacing.
	pacing_origin: Option<std::time::Instant>,
	/// Optional low-latency completion signal used by synchronous producers.
	send_started_at: Option<Instant>,
	outcome: TransportOutcome,
	disposition: CompletionDisposition,
	release: Option<NetworkCredit>,
	storage: Option<QueueStorage>,
	observer: Option<Box<dyn FnOnce(TransportCompletion) + Send>>,
	send_completion: Option<std::sync::mpsc::SyncSender<TransportCompletion>>,
}

impl ShardBatch {
	/// Create an empty batch (no allocation).
	#[cfg(test)]
	pub fn empty() -> Self {
		Self {
			data: Vec::new(),
			shard_size: 0,
			frame_number: 0,
			encoded_size: 0,
			data_shards: 0,
			parity_shards: 0,
			fec_blocks: 0,
			pacing_origin: None,
			send_completion: None,
			send_started_at: None,
			outcome: TransportOutcome::default(),
			disposition: CompletionDisposition::Cancelled,
			release: None,
			storage: None,
			observer: None,
		}
	}

	/// Raw contiguous buffer for GSO sends.
	pub fn as_bytes(&self) -> &[u8] {
		&self.data
	}

	/// Size of each individual shard.
	pub fn shard_size(&self) -> usize {
		self.shard_size
	}

	/// Number of shards in this batch.
	pub fn shard_count(&self) -> usize {
		self.data.len().checked_div(self.shard_size).unwrap_or(0)
	}

	pub fn frame_number(&self) -> u32 {
		self.frame_number
	}

	pub fn encoded_size(&self) -> usize {
		self.encoded_size
	}

	pub fn data_shards(&self) -> usize {
		self.data_shards
	}

	pub fn parity_shards(&self) -> usize {
		self.parity_shards
	}

	pub fn fec_blocks(&self) -> usize {
		self.fec_blocks
	}

	pub fn pacing_origin(&self) -> Option<std::time::Instant> {
		self.pacing_origin
	}

	pub fn set_pacing_origin(&mut self, origin: std::time::Instant) {
		self.pacing_origin = Some(origin);
	}

	pub fn set_frame_metadata(
		&mut self,
		frame_number: u32,
		encoded_size: usize,
		data_shards: usize,
		parity_shards: usize,
		fec_blocks: usize,
	) {
		self.frame_number = frame_number;
		self.encoded_size = encoded_size;
		self.data_shards = data_shards;
		self.parity_shards = parity_shards;
		self.fec_blocks = fec_blocks;
	}

	pub fn set_send_completion(&mut self, completion: std::sync::mpsc::SyncSender<TransportCompletion>) {
		debug_assert!(self.send_completion.is_none());
		self.send_completion = Some(completion);
	}

	pub fn track_queue(&mut self, depth: Arc<QueueDepth>) {
		let bytes = self.data.len();
		let frames = depth.frames.fetch_add(1, Ordering::Relaxed) + 1;
		let total = depth.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
		depth.high_frames.fetch_max(frames, Ordering::Relaxed);
		depth.high_bytes.fetch_max(total, Ordering::Relaxed);
		self.storage = Some(QueueStorage { depth, bytes });
	}

	pub fn hold_until_release(&mut self, credit: NetworkCredit) {
		self.release = Some(credit);
	}

	pub fn observe_completion(&mut self, observer: impl FnOnce(TransportCompletion) + Send + 'static) {
		self.observer = Some(Box::new(observer));
	}

	pub fn mark_send_started(&mut self) {
		self.send_started_at = Some(Instant::now());
	}

	pub fn transport_parts(&mut self) -> (&[u8], &mut TransportOutcome) {
		(&self.data, &mut self.outcome)
	}

	pub fn finish(&mut self, disposition: CompletionDisposition) -> TransportCompletion {
		self.disposition = disposition;
		if let Some(storage) = &self.storage {
			tracing::trace!(
				queue_frames = storage.depth.frames.load(Ordering::Relaxed),
				queue_bytes = storage.depth.bytes.load(Ordering::Relaxed),
				queue_high_frames = storage.depth.high_frames.load(Ordering::Relaxed),
				queue_high_bytes = storage.depth.high_bytes.load(Ordering::Relaxed),
				"Video queue storage"
			);
		}
		if self.send_completion.is_some() || self.observer.is_some() || self.release.is_some() {
			tracing::trace!(
				frame_number = self.frame_number,
				?disposition,
				attempted_datagrams = self.outcome.attempted_datagrams,
				attempted_payload_bytes = self.outcome.attempted_payload_bytes,
				submitted_datagrams = self.outcome.submitted_datagrams,
				submitted_payload_bytes = self.outcome.submitted_payload_bytes,
				failed_datagrams = self.outcome.failed_datagrams,
				discarded_datagrams = self
					.shard_count()
					.saturating_sub(self.outcome.submitted_datagrams + self.outcome.failed_datagrams),
				resource_release = true,
				"Video batch completion"
			);
		}
		let completion = TransportCompletion {
			finished_at: Instant::now(),
			send_started_at: self.send_started_at,
			outcome: self.outcome,
			disposition,
			discarded_datagrams: self
				.shard_count()
				.saturating_sub(self.outcome.submitted_datagrams + self.outcome.failed_datagrams),
			discarded_payload_bytes: self
				.data
				.len()
				.saturating_sub(self.outcome.submitted_payload_bytes + self.outcome.failed_datagrams * self.shard_size),
		};
		self.storage.take();
		self.release.take();
		if let Some(signal) = self.send_completion.take() {
			let _ = signal.try_send(completion);
		}
		if let Some(observer) = self.observer.take() {
			observer(completion);
		}
		completion
	}

	pub fn notify_sent(&mut self) {
		self.finish(if self.outcome.failed_datagrams == 0 {
			CompletionDisposition::Submitted
		} else {
			CompletionDisposition::Failed
		});
	}
}

impl Drop for ShardBatch {
	fn drop(&mut self) {
		self.finish(self.disposition);
	}
}

/// A mutable view of equal-sized shards backed by a contiguous buffer.
///
/// Used during packetization to build data shards and run FEC encoding.
/// Can be converted into a `ShardBatch` when done.
///
/// Each shard slot in the buffer has the layout:
///   `[prefix (prefix_size bytes)] [data (data_size bytes)]`
///
/// The prefix region is reserved for per-shard metadata (e.g. encryption
/// headers) and is **not** included in FEC encoding.
pub(crate) struct ShardBuf {
	data: Vec<u8>,
	block_start: usize,
	/// Total bytes per shard slot (prefix_size + data_size).
	stride: usize,
	/// Bytes reserved before each shard for per-shard metadata.
	prefix_size: usize,
	/// Bytes of actual shard data (RTP + padding + NvVideoPacket + payload).
	data_size: usize,
	shard_count: usize,
}

impl ShardBuf {
	/// Allocate a zeroed buffer for `shard_count` shards of `data_size` bytes
	/// each, with `prefix_size` bytes reserved before each shard.
	pub fn new(shard_count: usize, data_size: usize, prefix_size: usize) -> Self {
		let stride = prefix_size + data_size;
		Self {
			data: vec![0u8; shard_count * stride],
			block_start: 0,
			stride,
			prefix_size,
			data_size,
			shard_count,
		}
	}

	/// Select one FEC block within the owned frame allocation. No bytes move.
	pub fn select_block(&mut self, start: usize, count: usize) {
		assert!((start + count) * self.stride <= self.data.len());
		self.block_start = start;
		self.shard_count = count;
	}

	/// Returns a mutable reference to the data portion of the shard at the
	/// given index (excludes prefix).
	pub fn shard_mut(&mut self, index: usize) -> &mut [u8] {
		debug_assert!(index < self.shard_count);
		let start = (self.block_start + index) * self.stride + self.prefix_size;
		&mut self.data[start..start + self.data_size]
	}

	/// Returns a mutable reference to the prefix portion of the shard.
	pub fn prefix_mut(&mut self, index: usize) -> &mut [u8] {
		debug_assert!(index < self.shard_count);
		let start = (self.block_start + index) * self.stride;
		&mut self.data[start..start + self.prefix_size]
	}

	/// Provide mutable shard slices for FEC encoding.
	///
	/// Returns a `Vec` of `ShardSlice` wrappers that implement
	/// `AsRef<[u8]> + AsMut<[u8]>`, suitable for fec-rs.
	///
	/// Only the data portion of each shard is included; the prefix is excluded.
	pub fn as_fec_slices(&mut self) -> Vec<ShardSlice<'_>> {
		// This is safe because each ShardSlice references a non-overlapping region.
		let ptr = self.data.as_mut_ptr();
		let stride = self.stride;
		let prefix_size = self.prefix_size;
		let data_size = self.data_size;
		(0..self.shard_count)
			.map(|i| {
				let slice = unsafe {
					std::slice::from_raw_parts_mut(ptr.add((self.block_start + i) * stride + prefix_size), data_size)
				};
				ShardSlice(slice)
			})
			.collect()
	}

	/// Convert into a ShardBatch for sending over the channel.
	///
	/// The batch shard size equals the full stride (prefix + data) so that
	/// both regions are transmitted in a single UDP send.
	pub fn into_batch(self) -> ShardBatch {
		ShardBatch {
			data: self.data,
			shard_size: self.stride,
			frame_number: 0,
			encoded_size: 0,
			data_shards: 0,
			parity_shards: 0,
			fec_blocks: 0,
			pacing_origin: None,
			send_completion: None,
			send_started_at: None,
			outcome: TransportOutcome::default(),
			disposition: CompletionDisposition::Cancelled,
			release: None,
			storage: None,
			observer: None,
		}
	}
}

/// A mutable reference to one shard within a `ShardBuf`.
///
/// Implements `AsRef<[u8]> + AsMut<[u8]>` so it can be used with
/// fec-rs's `encode()` method.
pub(crate) struct ShardSlice<'a>(&'a mut [u8]);

impl AsRef<[u8]> for ShardSlice<'_> {
	fn as_ref(&self) -> &[u8] {
		self.0
	}
}

impl AsMut<[u8]> for ShardSlice<'_> {
	fn as_mut(&mut self) -> &mut [u8] {
		self.0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn release_is_once_even_when_notifications_cannot_be_delivered() {
		for disposition in [
			CompletionDisposition::Submitted,
			CompletionDisposition::Failed,
			CompletionDisposition::Discarded,
			CompletionDisposition::Cancelled,
		] {
			let counter = Arc::new(AtomicUsize::new(1));
			let queue = Arc::new(QueueDepth::default());
			let mut batch = ShardBuf::new(2, 64, 0).into_batch();
			batch.hold_until_release(NetworkCredit(counter.clone()));
			batch.track_queue(queue.clone());
			let (tx, rx) = std::sync::mpsc::sync_channel(0);
			batch.set_send_completion(tx);
			drop(rx); // completion must release resources without a waiting receiver.
			batch.finish(disposition);
			assert_eq!(counter.load(Ordering::Relaxed), 0);
			assert_eq!(queue.bytes.load(Ordering::Relaxed), 0);
			assert_eq!(queue.high_bytes.load(Ordering::Relaxed), 128);
			drop(batch);
			assert_eq!(counter.load(Ordering::Relaxed), 0);
		}
	}

	#[test]
	fn block_views_share_one_zeroed_frame_allocation() {
		let mut buffer = ShardBuf::new(7, 16, 4);
		let ptr = buffer.data.as_ptr();
		buffer.select_block(0, 3);
		buffer.shard_mut(1).fill(0xaa);
		buffer.select_block(3, 4);
		assert!(buffer.shard_mut(0).iter().all(|b| *b == 0));
		buffer.shard_mut(0).fill(0xbb);
		let batch = buffer.into_batch();
		assert_eq!(batch.as_bytes().as_ptr(), ptr);
		assert_eq!(batch.shard_count(), 7);
		assert!(batch.as_bytes().as_chunks::<20>().0.iter().all(|s| s[..4] == [0; 4]));
	}

	#[test]
	fn send_completion_is_signalled_once() {
		let mut batch = ShardBatch::empty();
		let (tx, rx) = std::sync::mpsc::sync_channel(1);
		batch.set_send_completion(tx);
		batch.notify_sent();
		assert!(rx.try_recv().is_ok());
		batch.notify_sent();
	}
}
