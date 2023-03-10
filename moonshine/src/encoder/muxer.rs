use std::{ptr::{null, null_mut}, ffi::CStr};

use ffmpeg_sys::URLContext;
use std::io::Write;

use super::{to_c_str, check_ret, codec::Codec};

pub(super) struct Muxer {
	format_context: *mut ffmpeg_sys::AVFormatContext,
	video_stream: *const ffmpeg_sys::AVStream,

	local_rtp_port: i64,
	local_rtcp_port: i64,
}

impl Muxer {
	pub(super) fn new(port: u16, codec: &Codec) -> Result<Self, ()> {
		let url = format!("rtp://localhost:{port}");

		unsafe {
			let format_context = ffmpeg_sys::avformat_alloc_context();
			if format_context.is_null() {
				log::error!("Failed to allocate a format context.");
				return Err(());
			}
			let format_context = &mut *format_context;

			format_context.oformat = Self::create_format()?;

			check_ret(ffmpeg_sys::avio_open(
				&mut format_context.pb,
				to_c_str(url.as_str())?.as_ptr(),
				ffmpeg_sys::AVIO_FLAG_WRITE as i32
			))
				.map_err(|e| log::error!("Failed to open output file: {}", e))?;

			let video_stream = Self::create_video_stream(format_context, codec)?;

			let mut local_rtp_port: i64 = 0;
			check_ret(ffmpeg_sys::av_opt_get_int(
					format_context.pb as *mut ffmpeg_sys::AVIOContext as *mut ::std::os::raw::c_void,
					to_c_str("local_rtpport")?.as_ptr(),
					ffmpeg_sys::AV_OPT_SEARCH_CHILDREN as i32,
					&mut local_rtp_port as *mut i64
				))
				.map_err(|e| log::error!("Failed to find local RTP port in format context."))?;

			let mut local_rtcp_port: i64 = 0;
			check_ret(ffmpeg_sys::av_opt_get_int(
					format_context.pb as *mut ffmpeg_sys::AVIOContext as *mut ::std::os::raw::c_void,
					to_c_str("local_rtcpport")?.as_ptr(),
					ffmpeg_sys::AV_OPT_SEARCH_CHILDREN as i32,
					&mut local_rtcp_port as *mut i64
				))
				.map_err(|e| log::error!("Failed to find local RTCP port in format context."))?;

			Ok(Self { format_context, video_stream, local_rtp_port, local_rtcp_port })
		}
	}

	fn create_format() -> Result<*const ffmpeg_sys::AVOutputFormat, ()> {
		let output_format = unsafe { ffmpeg_sys::av_guess_format(to_c_str("rtp")?.as_ptr(), null(), null()) };
		if output_format.is_null() {
			log::error!("Failed to determine output format.");
			return Err(());
		}

		Ok(output_format)
	}

	fn create_video_stream(format_context: *mut ffmpeg_sys::AVFormatContext, codec: &Codec) -> Result<*const ffmpeg_sys::AVStream, ()> {
		unsafe {
			let stream = ffmpeg_sys::avformat_new_stream(format_context, null());
			if stream.is_null() {
				log::error!("Could not create a new stream.");
				return Err(());
			}
			let stream = &mut *stream;
			stream.id = (*format_context).nb_streams as i32 - 1;
			stream.time_base = codec.as_ref().time_base;
			stream.avg_frame_rate = codec.as_ref().framerate;

			// Set parameters based on the codec.
			check_ret(ffmpeg_sys::avcodec_parameters_from_context(stream.codecpar, codec.as_ptr()))
				.map_err(|e| log::error!("Failed to set codec parameters: {}", e))?;
			Ok(stream)
		}
	}

	pub(super) fn start(&mut self) -> Result<(), ()> {
		unsafe {
			let mut buf = [0u8; 200000];
			ffmpeg_sys::av_sdp_create(&mut self.format_context, 1, buf.as_mut_ptr() as *mut i8, buf.len() as i32);
			let mut w = std::fs::File::create("video.sdp").unwrap();
			w.write_all(&buf).unwrap();
			std::thread::sleep_ms(10_000);

			let ret = ffmpeg_sys::avformat_write_header(self.format_context, null_mut());
			if ret as u32 != ffmpeg_sys::AVSTREAM_INIT_IN_WRITE_HEADER {
				log::error!("Failed to write header.");
				return Err(());
			} else {
				log::info!("Successfully encoded header!");
			}
			check_ret(ffmpeg_sys::fflush(ffmpeg_sys::stdout))
				.map_err(|e| log::error!("Failed to flush data: {e}"))?;
		}

		Ok(())
	}

	pub(super) fn stop(&self) -> Result<(), ()> {
		unsafe {
			check_ret(ffmpeg_sys::av_write_trailer(self.as_ptr()))
				.map_err(|e| log::error!("Failed to write format trailer: {}", e))?;
			check_ret(ffmpeg_sys::avio_close(self.as_ref().pb))
				.map_err(|e| log::error!("Failed to close file: {}", e))?;
		};

		Ok(())
	}

	pub(super) fn video_stream(&self) -> *const ffmpeg_sys::AVStream {
		self.video_stream
	}

	pub(super) fn local_rtp_port(&self) -> i64 {
		self.local_rtp_port
	}

	pub(super) fn local_rtcp_port(&self) -> i64 {
		self.local_rtcp_port
	}

	pub(super) fn session_description(&mut self) -> Result<sdp_types::Session, ()> {
		let mut buf = [0u8; 1024];
		unsafe {
			ffmpeg_sys::av_sdp_create(&mut self.format_context, 1, buf.as_mut_ptr() as *mut i8, buf.len() as i32);
		}

		sdp_types::Session::parse(&buf)
			.map_err(|e| log::error!("Failed to create session descriptor: {e}"))
	}

	pub(super) fn as_ptr(&self) -> *mut ffmpeg_sys::AVFormatContext {
		self.format_context
	}

	pub(super) unsafe fn as_ref(&self) -> &ffmpeg_sys::AVFormatContext {
		&*self.format_context
	}
}

unsafe impl Send for Muxer {}
unsafe impl Sync for Muxer {}

impl Drop for Muxer {
	fn drop(&mut self) {
		unsafe {
			ffmpeg_sys::avformat_free_context(self.format_context);
		}
	}
}
