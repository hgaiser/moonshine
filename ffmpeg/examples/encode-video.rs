use std::io::Write;

use ffmpeg::{CodecContext, CodecContextBuilder, Codec, FrameBuilder, Frame, Packet};

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
	let args: Vec<String> = std::env::args().collect();
	if args.len() <= 2 {
		println!("Usage: {} <output file> <codec name>", args[0]);
		return Ok(());
	}

	let filename = &args[1];
	let codec_name = &args[2];

	// Find the encoder.
	let codec = Codec::new(codec_name)
		.map_err(|e| println!("Failed to find codec: {e}"))?;
	println!("Using codec: {codec:?}");

	let mut codec_context_builder = CodecContextBuilder::new(&codec)
		.map_err(|e| println!("Failed to create codec: {e}"))?;
	codec_context_builder
		.set_width(2560)
		.set_height(1600)
		.set_framerate(30)
		.set_max_b_frames(0)
		.set_pix_fmt(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)
		.set_bit_rate(1000000)
		.set_gop_size(30)
		.set_flags(ffmpeg_sys::AV_CODEC_FLAG_CLOSED_GOP | ffmpeg_sys::AV_CODEC_FLAG_LOW_DELAY)
		.set_flags2(ffmpeg_sys::AV_CODEC_FLAG2_FAST)
	;

	let mut codec_context = codec_context_builder
		.open()
		.map_err(|e| println!("Failed to open codec: {e}"))?;

	let mut packet = Packet::new()
		.map_err(|e| println!("Failed to create packet: {e}"))?;

	let mut file = std::fs::File::create(filename)
		.map_err(|e| println!("Failed to create output file: {e}"))?;

	let mut frame_builder = FrameBuilder::new()
		.map_err(|e| println!("Failed to create frame: {e}"))?;
	frame_builder
		.set_format(codec_context.as_raw().pix_fmt)
		.set_width(codec_context.as_raw().width as u32)
		.set_height(codec_context.as_raw().height as u32);
	let mut frame = frame_builder.allocate(0)
		.map_err(|e| println!("Failed to allocate frame: {e}"))?;

	// Encode 1 second of video.
	for i in 0..90 {
		// Make sure the frame data is writable.
		// On the first round, the frame is fresh from av_frame_get_buffer()
		// and therefore we know it is writable.
		// But on the next rounds, encode() will have called
		// avcodec_send_frame(), and the codec may have kept a reference to
		// the frame in its internal structures, that makes the frame
		// unwritable.
		// av_frame_make_writable() checks that and allocates a new buffer
		// for the frame only if necessary.
		frame.make_writable()
			.map_err(|e| println!("Failed to make frame writable: {e}"))?;

		// Prepare a dummy image.
		// In real code, this is where you would have your own logic for
		// filling the frame. FFmpeg does not care what you put in the
		// frame.
		unsafe {
			// Y
			let y_data = std::slice::from_raw_parts_mut(
				frame.as_raw_mut().data[0],
				frame.as_raw().linesize[0] as usize * codec_context.as_raw().height as usize,
			);
			for y in 0..codec_context.as_raw().height {
				for x in 0..codec_context.as_raw().width {
					y_data[(y * frame.as_raw().linesize[0] + x) as usize] = (x + y + i * 3) as u8;
				}
			}

			// Cb and Cr
			let cb_data = std::slice::from_raw_parts_mut(
				frame.as_raw_mut().data[1],
				frame.as_raw().linesize[1] as usize * codec_context.as_raw().height as usize,
			);
			let cr_data = std::slice::from_raw_parts_mut(
				frame.as_raw_mut().data[2],
				frame.as_raw().linesize[2] as usize * codec_context.as_raw().height as usize,
			);
			for y in 0..codec_context.as_raw().height / 2 {
				for x in 0..codec_context.as_raw().width / 2 {
					cb_data[(y * frame.as_raw().linesize[1] + x) as usize] = (128 + y + i * 2) as u8;
					cr_data[(y * frame.as_raw().linesize[2] + x) as usize] = (64 + x + i * 5) as u8;
				}
			}
		}

		frame.as_raw_mut().pts = i as i64;

		// Encode the image.
		encode(&mut codec_context, Some(&frame), &mut packet, &mut file)?;
	}

	// Flush the encoder.
	encode(&mut codec_context, None, &mut packet, &mut file)?;

	Ok(())
}
