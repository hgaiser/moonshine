/// A batch of equal-sized network shards stored in a single contiguous buffer.
///
/// Replaces `Vec<Vec<u8>>` to avoid one heap allocation per shard
/// (~50–100 per frame). All shards are packed into a single `Vec<u8>`
/// and accessed via `chunks_exact(shard_size)`.
pub struct ShardBatch {
	/// Contiguous buffer holding `shard_count * shard_size` bytes.
	data: Vec<u8>,
	/// Size of each shard in bytes.
	shard_size: usize,
}

impl ShardBatch {
	/// Create an empty batch (no allocation).
	pub fn empty() -> Self {
		Self {
			data: Vec::new(),
			shard_size: 0,
		}
	}

	/// Iterate over all shards as byte slices for sending.
	pub fn shards(&self) -> impl Iterator<Item = &[u8]> {
		let size = self.shard_size;
		if size == 0 {
			[].chunks_exact(1) // yields an empty iterator
		} else {
			self.data.chunks_exact(size)
		}
	}

	/// Append all shards from `other` into this batch.
	///
	/// Both batches must have the same shard_size (or `self` must be empty).
	pub fn extend_from(&mut self, other: &ShardBatch) {
		debug_assert!(self.shard_size == 0 || self.shard_size == other.shard_size);
		if self.shard_size == 0 {
			self.shard_size = other.shard_size;
		}
		self.data.extend_from_slice(&other.data);
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
pub struct ShardBuf {
	data: Vec<u8>,
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
			stride,
			prefix_size,
			data_size,
			shard_count,
		}
	}

	/// Returns a mutable reference to the data portion of the shard at the
	/// given index (excludes prefix).
	pub fn shard_mut(&mut self, index: usize) -> &mut [u8] {
		let start = index * self.stride + self.prefix_size;
		&mut self.data[start..start + self.data_size]
	}

	/// Returns a mutable reference to the prefix portion of the shard.
	pub fn prefix_mut(&mut self, index: usize) -> &mut [u8] {
		let start = index * self.stride;
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
				let slice = unsafe { std::slice::from_raw_parts_mut(ptr.add(i * stride + prefix_size), data_size) };
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
		}
	}
}

/// A mutable reference to one shard within a `ShardBuf`.
///
/// Implements `AsRef<[u8]> + AsMut<[u8]>` so it can be used with
/// fec-rs's `encode()` method.
pub struct ShardSlice<'a>(&'a mut [u8]);

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
