use std::ptr::null_mut;

pub struct SwsContext {
	context: *mut ffmpeg_sys::SwsContext,
}

impl SwsContext {
	pub fn new(
		source_dimensions: (u32, u32),
		source_format: ffmpeg_sys::AVPixelFormat,
		dest_dimensions: (u32, u32),
		dest_format: ffmpeg_sys::AVPixelFormat,
		flags: i32,
	) -> Self {
		let context = unsafe { ffmpeg_sys::sws_getContext(
			source_dimensions.0 as i32, source_dimensions.1 as i32, source_format,
			dest_dimensions.0 as i32, dest_dimensions.1 as i32, dest_format,
			flags,
			null_mut(),
			null_mut(),
			null_mut(),
		) };

		Self { context }
	}

	pub fn scale(
		&self,
		source: *const *const u8,
		source_stride: &[i32],
		height: i32,
		dest: *mut *mut u8,
		dest_stride: &[i32],
	) {
		unsafe { ffmpeg_sys::sws_scale(
			self.context,
			source,
			source_stride.as_ptr(),
			0,
			height,
			dest,
			dest_stride.as_ptr(),
		) };
	}
}

impl Drop for SwsContext {
	fn drop(&mut self) {
		unsafe { ffmpeg_sys::sws_freeContext(self.context) };
	}
}

unsafe impl Send for SwsContext { }
unsafe impl Sync for SwsContext { }
