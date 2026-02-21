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
		self.data.chunks_exact(self.shard_size)
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
pub struct ShardBuf {
	data: Vec<u8>,
	shard_size: usize,
	shard_count: usize,
}

impl ShardBuf {
	/// Allocate a zeroed buffer for `shard_count` shards of `shard_size` bytes.
	pub fn new(shard_count: usize, shard_size: usize) -> Self {
		Self {
			data: vec![0u8; shard_count * shard_size],
			shard_size,
			shard_count,
		}
	}

	/// Returns a mutable reference to shard at the given index.
	pub fn shard_mut(&mut self, index: usize) -> &mut [u8] {
		let start = index * self.shard_size;
		&mut self.data[start..start + self.shard_size]
	}

	/// Provide mutable shard slices for FEC encoding.
	///
	/// Returns a `Vec` of `ShardSlice` wrappers that implement
	/// `AsRef<[u8]> + AsMut<[u8]>`, suitable for reed-solomon-erasure.
	pub fn as_fec_slices(&mut self) -> Vec<ShardSlice<'_>> {
		// This is safe because each ShardSlice references a non-overlapping region.
		let ptr = self.data.as_mut_ptr();
		let shard_size = self.shard_size;
		(0..self.shard_count)
			.map(|i| {
				let slice = unsafe { std::slice::from_raw_parts_mut(ptr.add(i * shard_size), shard_size) };
				ShardSlice(slice)
			})
			.collect()
	}

	/// Convert into a ShardBatch for sending over the channel.
	pub fn into_batch(self) -> ShardBatch {
		ShardBatch {
			data: self.data,
			shard_size: self.shard_size,
		}
	}
}

/// A mutable reference to one shard within a `ShardBuf`.
///
/// Implements `AsRef<[u8]> + AsMut<[u8]>` so it can be used with
/// reed-solomon-erasure's `encode()` method.
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
