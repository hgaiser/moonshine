use std::{io::Write, sync::{Arc, Mutex}};

use ffmpeg::{CudaDeviceContextBuilder, HwFrameContextBuilder, CodecContextBuilder, Codec, Frame, Packet, FrameBuilder, CodecContext, HwFrameContext};
use moonshine::cuda::{CudaContext, check_ret};
use nvfbc::{CudaCapturer, cuda::CaptureMethod};

fn create_frame(width: u32, height: u32, pixel_format: i32, context: &mut HwFrameContext) -> Result<Frame, ()> {
	let mut frame_builder = FrameBuilder::new()
		.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
	frame_builder
		.set_format(pixel_format)
		.set_width(width)
		.set_height(height)
		.set_hw_frames_ctx(context);
	let mut frame = frame_builder.allocate_hwframe()
		.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

	// frame.make_writable()
	// 	.map_err(|e| log::error!("Failed to make frame writable: {e}"))?;

	unsafe {
		ffmpeg::check_ret(ffmpeg_sys::av_hwframe_get_buffer(frame.as_raw_mut().hw_frames_ctx, frame.as_raw_mut(), 0))
			.map_err(|e| println!("Failed to allocate hardware frame: {e}"))?;
		frame.as_raw_mut().linesize[0] = frame.as_raw().width * 4
	}

	Ok(frame)
}

fn encode(
	codec_context: &mut CodecContext,
	frame: Option<&Frame>,
	packet: &mut Packet,
	file: &mut std::fs::File,
) -> Result<(), ()> {
	if let Some(frame) = &frame {
		println!("Send frame {}", frame.as_raw().pts);
	}

	// Send the frame to the encoder.
	codec_context.send_frame(frame)
		.map_err(|e| println!("Error sending frame for encoding: {e}"))?;

	loop {
		match codec_context.receive_packet(packet) {
			Ok(()) => {
				println!("Write packet {} (size={})", packet.as_raw().pts, packet.as_raw().size);
				file.write(packet.data())
					.map_err(|e| println!("Failed to write to file: {e}"))?;
			},
			Err(e) => {
				if e.code == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
					println!("Need more frames for encoding...");
					return Ok(());
				} else if e.code == ffmpeg_sys::AVERROR_EOF {
					println!("End of file");
					return Ok(());
				} else {
					println!("Error while encoding: {e}");
					return Err(());
				}
			}
		}
	}
}

fn main() -> Result<(), ()> {
	let cuda_context = CudaContext::new(0)
		.map_err(|e| println!("Failed to create CUDA context: {e}"))?;

	let mut capturer = CudaCapturer::new()
		.map_err(|e| println!("Failed to create CUDA capturer: {e}"))?;

	let status = capturer.status()
		.map_err(|e| println!("Failed to get capturer status: {e}"))?;
	println!("{status:#?}");
	if !status.can_create_now {
		panic!("Can't create a CUDA capture session.");
	}

	capturer.release_context()
		.map_err(|e| println!("Failed to release capture CUDA context: {e}"))?;

	let width = status.screen_size.w;
	let height = status.screen_size.h;
	let framerate = 60;
	let codec_name = "h264_nvenc";
	let bitrate = 40960000;
	let filename = "test.h264";

	let cuda_device_context = CudaDeviceContextBuilder::new()
		.map_err(|e| log::error!("Failed to create CUDA device context: {e}"))?
		.set_cuda_context(cuda_context.as_raw())
		.build()
		.map_err(|e| log::error!("Failed to build CUDA device context: {e}"))?
	;

	let mut hw_frame_context = HwFrameContextBuilder::new(cuda_device_context)
		.map_err(|e| log::error!("Failed to create CUDA frame context: {e}"))?
		.set_width(width)
		.set_height(height)
		.set_sw_format(ffmpeg_sys::AV_PIX_FMT_0RGB32)
		.set_format(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA)
		.build()
		.map_err(|e| log::error!("Failed to build CUDA frame context: {e}"))?
	;

	let mut capture_buffer = create_frame(width, height, ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA, &mut hw_frame_context)?;
	let intermediate_buffer = Arc::new(Mutex::new(create_frame(width, height, ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA, &mut hw_frame_context)?));
	let mut encoder_buffer = create_frame(width, height, ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA, &mut hw_frame_context)?;
	let notifier = Arc::new(std::sync::Condvar::new());

	let codec = Codec::new(codec_name)
		.map_err(|e| log::error!("Failed to create codec: {e}"))?;

	let mut codec_context_builder = CodecContextBuilder::new(&codec)
		.map_err(|e| log::error!("Failed to create codec context builder: {e}"))?;
	codec_context_builder
		.set_width(width)
		.set_height(height)
		.set_fps(framerate * 2) // ???
		.set_max_b_frames(0)
		.set_pix_fmt(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_CUDA)
		.set_bit_rate(bitrate)
		.set_gop_size(i32::max_value() as u32)
		.set_preset("fast")
		.set_tune("ull")
		.set_hw_frames_ctx(&mut hw_frame_context)
	;
	codec_context_builder.as_raw_mut().refs = 1;

	let mut codec_context = codec_context_builder
		.open()
		.map_err(|e| log::error!("Failed to create codec context: {e}"))?;

	let mut packet = Packet::new()
		.map_err(|e| println!("Failed to create packet: {e}"))?;

	let mut file = std::fs::File::create(filename)
		.map_err(|e| println!("Failed to create output file: {e}"))?;

	std::thread::spawn({
		let intermediate_buffer = intermediate_buffer.clone();
		let notifier = notifier.clone();
		move || {
			cuda_context.set_current()
				.map_err(|e| println!("Failed to set CUDA context as current context: {e}")).unwrap();
			capturer.bind_context()
				.map_err(|e| println!("Failed to bind capture CUDA context: {e}")).unwrap();
			capturer.start(nvfbc::BufferFormat::Bgra, framerate)
				.map_err(|e| println!("Failed to start capture: {e}")).unwrap();

			for i in 0.. {
				let start = std::time::Instant::now();
				let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
					.map_err(|e| println!("Failed to get new frame: {e}")).unwrap();

				// println!("CUDA buffer: {:?}", capture_buffer.as_raw_mut().data);
				unsafe {
					check_ret(ffmpeg_sys::cuMemcpy(
						capture_buffer.as_raw_mut().data[0] as u64,
						frame_info.device_buffer as u64,
						frame_info.device_buffer_len as usize,
					))
						.map_err(|e| println!("Failed to copy CUDA memory: {e}")).unwrap();
				}
				// capture_buffer.as_raw_mut().data[0] = frame_info.device_buffer as *mut u8;
				capture_buffer.as_raw_mut().pts = i;

				// Swap the intermediate buffer with the output buffer and signal that we have a new frame.
				// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
				{
					let mut lock = intermediate_buffer.lock()
						.map_err(|e| println!("Failed to lock intermediate buffer: {e}")).unwrap();
					std::mem::swap(&mut *lock, &mut capture_buffer);
				}
				notifier.notify_one();

				println!("Elapsed: {}msec", start.elapsed().as_millis());
			}
		}
	});

	let encode_task = std::thread::spawn({
		let intermediate_buffer = intermediate_buffer.clone();
		let notifier = notifier.clone();
		move || {
			for _ in 0..180 {
				// Swap the intermediate buffer with the output buffer.
				// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
				{
					println!("Waiting for new frame.");
					// Wait for a new frame.
					let mut lock = notifier.wait(intermediate_buffer.lock().unwrap())
						.map_err(|e| println!("Failed to wait for new frame: {e}")).unwrap();
					println!("Received notification of new frame.");

					std::mem::swap(&mut *lock, &mut encoder_buffer);
				}

				encode(&mut codec_context, Some(&encoder_buffer), &mut packet, &mut file).unwrap();
			}

			file.flush()
				.map_err(|e| println!("Failed to flush file: {e}")).unwrap();
		}
	});

	encode_task.join()
		.map_err(|e| println!("Failed to wait for encoding task: {e:?}"))?;

	Ok(())
}