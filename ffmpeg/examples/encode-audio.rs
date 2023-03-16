use std::{io::Write, f32::consts::PI, ptr::null};

use ffmpeg::{
	Codec,
	CodecContext,
	CodecContextBuilder,
	Frame,
	FrameBuilder,
	Packet,
	check_ret,
};

// /// Check that a given sample format is supported by the encoder.
// static int check_sample_fmt(const AVCodec *codec, enum AVSampleFormat sample_fmt)
// {
// 	const enum AVSampleFormat *p = codec->sample_fmts;

// 	while (*p != AV_SAMPLE_FMT_NONE) {
// 		if (*p == sample_fmt)
// 			return 1;
// 		p++;
// 	}
// 	return 0;
// }

/// Just pick the highest supported samplerate.
fn select_sample_rate(codec: &Codec) -> u32 {
	if !codec.as_raw().supported_samplerates.is_null() {
		return 44100;
	}

	let mut p = codec.as_raw().supported_samplerates;
	let mut best_samplerate: i32 = 0;
	while !p.is_null() {
		let value = unsafe { *p };
		if best_samplerate == 0 || (44100 - value).abs() < (44100 - best_samplerate).abs() {
			best_samplerate = value;
		}
		p = unsafe { p.offset(1) };
	}

	best_samplerate as u32
}

/// Select layout with the highest channel count.
fn select_channel_layout(
	codec: &Codec,
	dst: *mut ffmpeg_sys::AVChannelLayout,
) -> Result<(), ()> {
	if codec.as_raw().ch_layouts.is_null() {
		return check_ret(unsafe { ffmpeg_sys::av_channel_layout_copy(dst, &ffmpeg_sys::AV_CHANNEL_LAYOUT_STEREO) })
			.map_err(|e| println!("Failed to copy channel layout: {e}"));
	}

	let mut p = codec.as_raw().ch_layouts;
	let mut nb_channels = unsafe { *p }.nb_channels;
	let mut best_nb_channels = 0;
	let mut best_ch_layout = null();
	while nb_channels > 0 {
		if nb_channels > best_nb_channels {
			best_ch_layout   = p;
			best_nb_channels = nb_channels;
		}
		p = unsafe { p.offset(1) };
		nb_channels = unsafe { *p }.nb_channels;
	}

	check_ret(unsafe { ffmpeg_sys::av_channel_layout_copy(dst, best_ch_layout) })
		.map_err(|e| println!("Failed to copy channel layout: {e}"))
}

fn encode(
	codec_context: &mut CodecContext,
	frame: Option<&Frame>,
	packet: &mut Packet,
	file: &mut std::fs::File,
) -> Result<(), ()> {
	// if let Some(frame) = &frame {
	// 	println!("Send frame {}", frame.as_raw().pts);
	// }

	// Send the frame to the encoder.
	codec_context.send_frame(frame)
		.map_err(|e| println!("Error sending frame for encoding: {e}"))?;

	// Read all the available output packets (in general there may be any number of them.
	loop {
		match codec_context.receive_packet(packet) {
			Ok(()) => {
				println!("Write packet (size={})", packet.as_raw().size);
				file.write(packet.data())
					.map_err(|e| println!("Failed to write to file: {e}"))?;
			},
			Err(e) => {
				if e.code == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
					// println!("Need more frames for encoding...");
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
	if args.len() <= 1 {
		println!("Usage: {} <output file>", args[0]);
		return Ok(());
	}

	let filename = &args[1];

	// Find the MP2 encoder.
	let codec = Codec::new("mp2")
		.map_err(|e| println!("Failed to find codec: {e}"))?;

	let mut codec_context_builder = CodecContextBuilder::new(&codec)
		.map_err(|e| println!("Failed to create codec: {e}"))?;
	codec_context_builder
		.set_bit_rate(64000)
		.set_sample_fmt(ffmpeg_sys::AVSampleFormat_AV_SAMPLE_FMT_S16 as u32)
		.set_sample_rate(select_sample_rate(&codec));

	// Check that the encoder supports s16 pcm input.
	// if (!check_sample_fmt(codec, c->sample_fmt)) {
	// 	fprintf(stderr, "Encoder does not support sample format %s",
	// 		av_get_sample_fmt_name(c->sample_fmt));
	// 	exit(1);
	// }

	// Select other audio parameters supported by the encoder.
	select_channel_layout(&codec, &mut codec_context_builder.as_raw_mut().ch_layout)?;

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
		.set_format(codec_context.as_raw().sample_fmt)
		.set_nb_samples(codec_context.as_raw().frame_size as u32);

	unsafe {
		check_ret(ffmpeg_sys::av_channel_layout_copy(&mut frame_builder.as_raw_mut().ch_layout, &codec_context.as_raw().ch_layout))
			.map_err(|e| println!("Failed to copy channel layout: {e}"))?;
	}

	let mut frame = frame_builder.allocate(0)
		.map_err(|e| println!("Failed to allocate frame: {e}"))?;

	// Encode a single tone sound.
	let mut t: f32 = 0.0;
	let tincr = 2.0 * PI * 440.0 / codec_context.as_raw().sample_rate as f32;
	for _ in 0..200 {
		// Make sure the frame is writable -- makes a copy if the encoder kept a reference internally.
		frame.make_writable()
			.map_err(|e| println!("Failed to make frame writable: {e}"))?;

		unsafe {
			let data = std::slice::from_raw_parts_mut(
				frame.as_raw_mut().data[0] as *mut u16,
				frame.as_raw().linesize[0] as usize,
			);
			for j in 0..codec_context.as_raw().frame_size {
				data[(2 * j) as usize] = (t.sin() * 10000.0) as u16;

				for k in 1..codec_context.as_raw().ch_layout.nb_channels {
					data[(2 * j + k) as usize] = data[(2 * j) as usize];
				}
				t += tincr;
			}
		}
		encode(&mut codec_context, Some(&frame), &mut packet, &mut file)?;
	}

	// Flush the encoder.
	encode(&mut codec_context, None, &mut packet, &mut file)?;

	Ok(())
}
