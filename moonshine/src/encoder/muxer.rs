use std::ptr::{null, null_mut};

use super::{to_c_str, check_ret, codec::Codec};

pub(super) struct Muxer {
	format_context: *mut ffmpeg_sys::AVFormatContext,
	video_stream: *const ffmpeg_sys::AVStream,
}

impl Muxer {
	pub(super) fn new(codec: &Codec) -> Result<Self, String> {
		let filename = "test.mp4";

		unsafe {
			let format_context = ffmpeg_sys::avformat_alloc_context();
			if format_context.is_null() {
				return Err("Failed to allocate a format context.".to_string());
			}
			let format_context = &mut *format_context;

			let video_stream = Self::create_video_stream(format_context, codec)?;

			format_context.oformat = Self::create_format()?;

			// TODO: Delete this, we don't want to write to a file.
			check_ret(ffmpeg_sys::avio_open(
				&mut format_context.pb,
				to_c_str(filename)?.as_ptr(),
				ffmpeg_sys::AVIO_FLAG_WRITE as i32
			))
				.map_err(|e| format!("Failed to open output file: {}", e))?;
			check_ret(ffmpeg_sys::avformat_write_header(format_context, null_mut()))
				.map_err(|e| format!("Failed to write output header: {}", e))?;

			Ok(Self { format_context, video_stream })
		}
	}

	fn create_format() -> Result<*const ffmpeg_sys::AVOutputFormat, String> {
		let output_format = unsafe { ffmpeg_sys::av_guess_format(to_c_str("mp4")?.as_ptr(), null(), null()) };
		if output_format.is_null() {
			return Err("Failed to determine output format.".to_string());
		}

		Ok(output_format)
	}

	fn create_video_stream(format_context: *mut ffmpeg_sys::AVFormatContext, codec: &Codec) -> Result<*const ffmpeg_sys::AVStream, String> {
		unsafe {
			let stream = ffmpeg_sys::avformat_new_stream(format_context, null());
			if stream.is_null() {
				return Err("Could not create a new stream.".to_string());
			}
			let stream = &mut *stream;
			stream.id = (*format_context).nb_streams as i32 - 1;
			stream.time_base = codec.as_ref().time_base;
			stream.avg_frame_rate = codec.as_ref().framerate;

			// Set parameters based on the codec.
			check_ret(ffmpeg_sys::avcodec_parameters_from_context(stream.codecpar, codec.as_ptr()))
				.map_err(|e| format!("Failed to set codec parameters: {}", e))?;
			Ok(stream)
		}
	}

	pub(super) fn stop(&self) -> Result<(), String> {
		unsafe {
			check_ret(ffmpeg_sys::av_write_trailer(self.as_ptr()))
				.map_err(|e| format!("Failed to write format trailer: {}", e))?;
			check_ret(ffmpeg_sys::avio_close(self.as_ref().pb))
				.map_err(|e| format!("Failed to close file: {}", e))?;
		};

		Ok(())
	}

	pub(super) fn video_stream(&self) -> *const ffmpeg_sys::AVStream {
		self.video_stream
	}

	pub(super) fn as_ptr(&self) -> *mut ffmpeg_sys::AVFormatContext {
		self.format_context
	}

	pub(super) unsafe fn as_ref(&self) -> &ffmpeg_sys::AVFormatContext {
		&*self.format_context
	}

	pub(super) unsafe fn as_mut(&self) -> &mut ffmpeg_sys::AVFormatContext {
		&mut *self.format_context
	}
}

impl Drop for Muxer {
	fn drop(&mut self) {
		unsafe {
			ffmpeg_sys::avformat_free_context(self.format_context);
		}
	}
}
