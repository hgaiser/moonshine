use super::{codec::Codec, check_ret};

pub(super) struct VideoFrame {
	frame: *mut ffmpeg_sys::AVFrame,
}

impl VideoFrame {
	pub(super) fn new(codec: &Codec) -> Result<Self, ()> {
		unsafe {
			let frame = ffmpeg_sys::av_frame_alloc();
			if frame.is_null() {
				log::error!("Failed to allocate VideoFrame.");
				return Err(());
			}

			let frame = &mut *frame;
			frame.format = codec.as_ref().pix_fmt;
			frame.width = codec.as_ref().width;
			frame.height = codec.as_ref().height;
			frame.key_frame = 1;
			frame.hw_frames_ctx = codec.as_ref().hw_frames_ctx;

			// TODO: Remove this, this shouldn't be necessary!
			// This allocates a HW frame, but we should manually create our own frame (through nvfbc).
			check_ret(ffmpeg_sys::av_hwframe_get_buffer(frame.hw_frames_ctx, frame, 0))
				.map_err(|e| log::error!("Failed to allocate hardware frame: {}", e))?;
			frame.linesize[0] = frame.width * 4;

			Ok(Self { frame })
		}
	}

	pub(super) fn set_buffer(&mut self, device_buffer: usize, time: std::time::Duration) {
		unsafe {
			self.as_mut().data[0] = device_buffer as *mut u8;
			self.as_mut().pts = (time.as_secs_f64() * ffmpeg_sys::AV_TIME_BASE as f64) as i64;
		}
	}

	pub(super) fn as_ptr(&self) -> *mut ffmpeg_sys::AVFrame {
		self.frame
	}

	pub(super) unsafe fn as_ref(&self) -> &ffmpeg_sys::AVFrame {
		&*self.frame
	}

	pub(super) unsafe fn as_mut(&mut self) -> &mut ffmpeg_sys::AVFrame {
		&mut *self.frame
	}
}

// impl Drop for VideoFrame {
// 	fn drop(&mut self) {
// 		unsafe {
// 			ffmpeg_sys::av_frame_free(&mut self.frame);
// 		}
// 	}
// }
