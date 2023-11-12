#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

mod generated;

/// Maximum allowed number of shards in the encoder (data + parity).
pub const MAX_SHARDS: usize = 255;

pub struct ReedSolomon {
	inner: *mut generated::reed_solomon,
}

impl ReedSolomon {
	pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self, String> {
		if data_shards == 0 || parity_shards == 0 {
			return Err(format!("expected data_shards > 0 and parity_shards > 0, but got {data_shards} and {parity_shards}"));
		}

		if data_shards + parity_shards > MAX_SHARDS {
			return Err(format!("number of shards ({}) exceeds maximum of {}", data_shards + parity_shards, MAX_SHARDS));
		}

		unsafe { generated::reed_solomon_init() };
		let inner = unsafe { generated::reed_solomon_new(data_shards as i32, parity_shards as i32) };
		if inner.is_null() {
			return Err("failed to create Reed Solomon structure".to_string());
		}

		Ok(Self { inner })
	}

	pub fn as_raw_mut(&mut self) -> &mut generated::reed_solomon {
		unsafe { &mut *self.inner }
	}

	pub fn as_raw(&self) -> &generated::reed_solomon {
		unsafe { &*self.inner }
	}

	pub fn set_parity_matrix(&mut self, parity_matrix: [u8; 8]) {
		unsafe {
			std::ptr::copy_nonoverlapping(parity_matrix.as_ptr(), self.as_raw_mut().m.offset(16), std::mem::size_of_val(&parity_matrix));
			std::ptr::copy_nonoverlapping(parity_matrix.as_ptr(), self.as_raw_mut().parity, std::mem::size_of_val(&parity_matrix));
		}
	}

	pub fn encode<T, U>(&self, shards: &mut T) -> Result<(), String>
	where
		T: AsRef<[U]> + AsMut<[U]>,
		U: AsRef<[u8]> + AsMut<[u8]>,
	{
		let shard_size = shards.as_ref()[0].as_ref().len();
		self.encode_fixed_length(shards, shard_size)
	}

	pub fn encode_fixed_length<T, U>(&self, shards: &mut T, size: usize) -> Result<(), String>
	where
		T: AsRef<[U]> + AsMut<[U]>,
		U: AsRef<[u8]> + AsMut<[u8]>,
	{
		let shards = shards.as_mut();
		let inner = unsafe { &*self.inner };
		if inner.data_shards as usize + inner.parity_shards as usize != shards.len() {
			return Err(format!(
				"expected exactly {} shards, got {}",
				inner.data_shards + inner.parity_shards,
				shards.len(),
			));
		}
		for (shard_index, shard) in shards.as_ref().iter().enumerate() {
			if shard.as_ref().len() < size {
				return Err(format!("shard {shard_index} has size {}, but we expect at least {size} bytes", shard.as_ref().len()));
			}
		}

		let mut shards: Vec<*mut u8> = shards
			.iter_mut()
			.map(|s| s.as_mut().as_mut_ptr())
			.collect();

		unsafe {
			generated::reed_solomon_encode(
				self.inner,
				shards.as_mut_ptr(),
				shards.len() as i32,
				size as i32,
			);
		}

		Ok(())
	}

	pub fn reconstruct<T, U>(&self, shards: &mut T, marks: &[bool]) -> Result<(), String>
	where
		T: AsRef<[U]> + AsMut<[U]>,
		U: AsRef<[u8]> + AsMut<[u8]>,
	{
		let shards = shards.as_mut();
		if shards.len() != marks.len() {
			return Err(format!("shards and marks lengths must be identical, got {} and {}", shards.len(), marks.len()));
		}
		let inner = unsafe { &*self.inner };
		if inner.data_shards as usize + inner.parity_shards as usize != shards.len() {
			return Err(format!(
				"expected exactly {} shards, got {}",
				inner.data_shards + inner.parity_shards,
				shards.len(),
			));
		}
		let shard_size = shards[0].as_ref().len();
		for shard in &mut *shards {
			if shard.as_ref().len() != shard_size {
				return Err("not all shards have the same size".to_string());
			}
		}

		let mut shards: Vec<*mut u8> = shards
			.iter_mut()
			.map(|s| s.as_mut().as_mut_ptr())
			.collect();

		let mut marks: Vec<u8> = marks
			.iter()
			.map(|b| if *b { 1 } else { 0 })
			.collect();

		let err = unsafe {
			generated::reed_solomon_reconstruct(
				self.inner,
				shards.as_mut_ptr(),
				marks.as_mut_ptr(),
				shards.len() as i32,
				shard_size as i32,
			)
		};

		if err != 0 {
			return Err(format!("failed to reconstruct with error code {err}"));
		}

		Ok(())
	}
}

impl Drop for ReedSolomon {
	fn drop(&mut self) {
		unsafe { generated::reed_solomon_release(self.inner) };
	}
}

unsafe impl Send for ReedSolomon {}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn encode() {
		let expected_shards: &mut [&mut [u8]] = &mut [
			&mut [0, 1, 2, 3],
			&mut [4, 5, 6, 7],
			&mut [8, 9, 10, 11],
			&mut [245, 200, 143, 178], // parity
			&mut [83, 142, 244, 41], // parity
		];
		let mut shards: &mut [&mut [u8]] = &mut [
			&mut [0, 1, 2, 3],
			&mut [4, 5, 6, 7],
			&mut [8, 9, 10, 11],
			&mut [0, 0, 0, 0], // parity
			&mut [0, 0, 0, 0], // parity
		];

		let rs = ReedSolomon::new(3, 2).unwrap();
		rs.encode(&mut shards).unwrap();

		assert_eq!(shards, expected_shards);
	}

	#[test]
	fn reconstruct() {
		let expected_shards: &mut [&mut [u8]] = &mut [
			&mut [0, 1, 2, 3],
			&mut [4, 5, 6, 7],
			&mut [8, 9, 10, 11],
			&mut [245, 200, 143, 178], // parity
			&mut [83, 142, 244, 41], // parity
		];
		let mut shards: &mut [&mut [u8]] = &mut [
			&mut [0, 1, 2, 3],
			&mut [0, 0, 0, 0],
			&mut [8, 9, 10, 11],
			&mut [245, 200, 143, 178], // parity
			&mut [83, 142, 244, 41], // parity
		];
		let marks = &[
			false, true, false, false, false
		];

		let rs = ReedSolomon::new(3, 2).unwrap();
		rs.reconstruct(&mut shards, marks).unwrap();

		assert_eq!(shards, expected_shards);
	}
}
