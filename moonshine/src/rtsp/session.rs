use std::{ptr::null_mut, mem::MaybeUninit};
use nvfbc::{BufferFormat, cuda::CaptureMethod, CudaCapturer};
use ffmpeg::{to_c_str, Codec, CodecType, VideoQuality, check_ret};

use crate::cuda;

pub(super) struct Session {
	codec: Codec,
	frame: *mut ffmpeg_sys::AVFrame,
	video_stream: *mut ffmpeg_sys::AVStream,
	format_context: *mut ffmpeg_sys::AVFormatContext,
	capturer: nvfbc::CudaCapturer,
}

unsafe impl Send for Session {}

impl Session {
	pub(super) fn new() -> Result<Self, ()> {
		unsafe {
			let cuda_context = cuda::init_cuda(0)
				.map_err(|e| println!("Failed to initialize CUDA: {e}"))?;

			// Create a capturer that captures to CUDA context.
			let capturer = CudaCapturer::new()
				.map_err(|e| println!("Failed to create CUDA capture device: {e}"))?;

			let status = capturer.status()
				.map_err(|e| println!("Failed to get capturer status: {e}"))?;

			println!("{status:#?}");
			if !status.can_create_now {
				panic!("Can't create a CUDA capture session.");
			}

			let width = status.screen_size.w;
			let height = status.screen_size.h;
			let fps = 60;

			let codec = Codec::new(
				width,
				height,
				fps,
				CodecType::H264,
				VideoQuality::Fastest,
				cuda_context,
			)
				.map_err(|e| log::error!("Failed to create codec: {e}"))?;

			// Init the format context
			let mut format_context = ffmpeg_sys::avformat_alloc_context();
			let format = ffmpeg_sys::av_guess_format(
				to_c_str("rtp")
					.map_err(|e| log::error!("Failed to create C string: {e}"))?
					.as_ptr(),
				null_mut(), null_mut()
			);
			ffmpeg_sys::avformat_alloc_output_context2(&mut format_context, format, (*format).name, null_mut());

			// Configure the AVStream for the output format context
			let video_stream = ffmpeg_sys::avformat_new_stream(format_context, codec.as_ref().codec);

			ffmpeg_sys::avcodec_parameters_from_context((*video_stream).codecpar, codec.as_ptr());
			(*video_stream).time_base.num = 1;
			(*video_stream).time_base.den = fps as i32;

			// Init the Frame containing our raw data
			let frame = ffmpeg_sys::av_frame_alloc();
			(*frame).format = codec.as_ref().pix_fmt;
			(*frame).width  = codec.as_ref().width;
			(*frame).height = codec.as_ref().height;
			(*frame).hw_frames_ctx = codec.as_ref().hw_frames_ctx;

			// TODO: Remove this, this shouldn't be necessary!
			// This allocates a HW frame, but we should manually create our own frame (through nvfbc).
			check_ret(ffmpeg_sys::av_hwframe_get_buffer((*frame).hw_frames_ctx, frame, 0))
				.map_err(|e| println!("Failed to allocate hardware frame: {e}"))?;
			(*frame).linesize[0] = (*frame).width * 4;
			// ffmpeg_sys::av_image_alloc((*frame).data.as_mut_ptr(), (*frame).linesize.as_mut_ptr(), (*frame).width, (*frame).height, (*codec_context).pix_fmt, 32);

			Ok(Self {
				codec,
				frame,
				video_stream,
				format_context,
				capturer,
			})
		}
	}

	pub(super) fn description(&mut self) -> Result<sdp_types::Session, ()> {
		let mut buf = [0u8; 1024];
		unsafe {
			ffmpeg_sys::av_sdp_create(
				&mut self.format_context,
				1,
				buf.as_mut_ptr() as *mut i8,
				buf.len() as i32,
			);
		}

		sdp_types::Session::parse(&buf)
			.map_err(|e| log::error!("Failed to create session descriptor: {e}"))
	}

	pub(super) fn setup(&mut self, rtp_port: u16, _rtcp_port: u16) -> Result<(u16, u16), ()> {
		unsafe {
			ffmpeg_sys::avio_open(
				&mut (*self.format_context).pb,
				to_c_str(format!("rtp://127.0.0.1:{rtp_port}").as_str())
				.map_err(|e| log::error!("{e}"))?
				.as_ptr(),
				ffmpeg_sys::AVIO_FLAG_WRITE as i32
			);
		}

		let mut local_rtp_port: i64 = 0;
		check_ret(unsafe { ffmpeg_sys::av_opt_get_int(
				(*self.format_context).pb as *mut ffmpeg_sys::AVIOContext as *mut ::std::os::raw::c_void,
				to_c_str("local_rtpport")
				.map_err(|e| log::error!("Failed to create C string: {e}"))?
				.as_ptr(),
				ffmpeg_sys::AV_OPT_SEARCH_CHILDREN as i32,
				&mut local_rtp_port as *mut i64
		) })
			.map_err(|e| log::error!("Failed to find local RTP port in format context: {e}"))?;

		let mut local_rtcp_port: i64 = 0;
		check_ret(unsafe {ffmpeg_sys::av_opt_get_int(
				(*self.format_context).pb as *mut ffmpeg_sys::AVIOContext as *mut ::std::os::raw::c_void,
				to_c_str("local_rtcpport")
				.map_err(|e| log::error!("Failed to create C string: {e}"))?
				.as_ptr(),
				ffmpeg_sys::AV_OPT_SEARCH_CHILDREN as i32,
				&mut local_rtcp_port as *mut i64
		) })
			.map_err(|e| log::error!("Failed to find local RTCP port in format context: {e}"))?;

		Ok((local_rtp_port as u16, local_rtcp_port as u16))
	}

	pub(super) fn play(&mut self) -> Result<(), ()> {
		unsafe {
			// Write the header to the client.
			ffmpeg_sys::avformat_write_header(self.format_context, null_mut());

			self.capturer.start(BufferFormat::Bgra, 60)
				.map_err(|e| println!("Failed to start frame capturer: {e}")).unwrap();

			let mut packet: ffmpeg_sys::AVPacket = MaybeUninit::zeroed().assume_init();
			let mut j = 0;
			for i in 0.. {
				let frame_info = self.capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
					.map_err(|e| println!("Failed to capture frame: {e}")).unwrap();
				(*self.frame).data[0] = frame_info.device_buffer as *mut u8;

				ffmpeg_sys::fflush(ffmpeg_sys::stdout);
				ffmpeg_sys::av_init_packet(&mut packet);
				packet.data = null_mut();    // packet data will be allocated by the encoder
				packet.size = 0;

				/* Which frame is it ? */
				(*self.frame).pts = i;

				/* Send the frame to the codec */
				ffmpeg_sys::avcodec_send_frame(self.codec.as_ptr(), self.frame);

				/* Use the data in the codec to the AVPacket */
				let ret = ffmpeg_sys::avcodec_receive_packet(self.codec.as_ptr(), &mut packet);
				if ret == ffmpeg_sys::AVERROR_EOF {
					println!("Stream EOF");
				} else if ret == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
					println!("Stream EAGAIN");
				} else {
					println!("Write frame {} (size={})", j, packet.size);
					j += 1;

					/* Write the data on the packet to the output format  */
					ffmpeg_sys::av_packet_rescale_ts(&mut packet, self.codec.as_ref().time_base, (*self.video_stream).time_base);
					ffmpeg_sys::av_interleaved_write_frame(self.format_context, &mut packet);

					/* Reset the packet */
					ffmpeg_sys::av_packet_unref(&mut packet);
				}
			}
		}

		Ok(())
	}
}
