use std::{ptr::null, net::SocketAddr, f32::consts::PI};

use ffmpeg::{
	Codec,
	CodecContext,
	CodecContextBuilder,
	Frame,
	FrameBuilder,
	Packet,
	check_ret,
};
use tokio::net::UdpSocket;

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


pub(super) struct AudioStream {
	socket: UdpSocket,
	codec_context: CodecContext,
	frame: Frame,
	packet: Packet,
}

impl AudioStream {
	pub(super) async fn new(address: &str, port: u16) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port)).await
			.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

		let codec = Codec::new("mp2")
			.map_err(|e| println!("Failed to find codec: {e}"))?;

		let mut codec_context_builder = CodecContextBuilder::new(&codec)
			.map_err(|e| println!("Failed to create codec: {e}"))?;
		codec_context_builder
			.set_bit_rate(64000)
			.set_sample_fmt(ffmpeg_sys::AVSampleFormat_AV_SAMPLE_FMT_S16 as u32)
			.set_sample_rate(select_sample_rate(&codec));

		// Select other audio parameters supported by the encoder.
		select_channel_layout(&codec, &mut codec_context_builder.as_raw_mut().ch_layout)?;

		let codec_context = codec_context_builder
			.open()
			.map_err(|e| println!("Failed to open codec: {e}"))?;

		let packet = Packet::new()
			.map_err(|e| println!("Failed to create packet: {e}"))?;

		let mut frame_builder = FrameBuilder::new()
			.map_err(|e| println!("Failed to create frame: {e}"))?;
		frame_builder
			.set_format(codec_context.as_raw().sample_fmt)
			.set_nb_samples(codec_context.as_raw().frame_size as u32);

		unsafe {
			check_ret(ffmpeg_sys::av_channel_layout_copy(&mut frame_builder.as_raw_mut().ch_layout, &codec_context.as_raw().ch_layout))
				.map_err(|e| println!("Failed to copy channel layout: {e}"))?;
			}

		let frame = frame_builder.allocate(0)
			.map_err(|e| println!("Failed to allocate frame: {e}"))?;

		Ok(Self {
			socket,
			codec_context,
			frame,
			packet,
		})
	}

	pub(super) async fn run(mut self) -> Result<(), ()> {
		log::info!(
			"Listening for audio messages on {}",
			self.socket.local_addr()
				.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let mut buf = [0; 1024];
		let mut client_address = None;
		for i in 0.. {
			match self.socket.try_recv_from(&mut buf) {
				Ok((len, addr)) => {
					if &buf[..len] == b"PING" {
						log::info!("Received audio stream PING message from {addr}.");
						client_address = Some(addr);
					} else {
						log::warn!("Received unknown message on audio stream of length {len}.");
					}
				},
				Err(ref e) => {
					if e.kind() != std::io::ErrorKind::WouldBlock {
						log::error!("Failed to receive UDP message: {e}");
						return Err(());
					}
				}
			}

			self.frame.make_writable()
				.map_err(|e| println!("Failed to make frame writable: {e}"))?;

			let mut t: f32 = 0.0;
			let tincr = 2.0 * PI * 440.0 / self.codec_context.as_raw().sample_rate as f32;
			unsafe {
				let data = std::slice::from_raw_parts_mut(
					self.frame.as_raw_mut().data[0] as *mut u16,
					self.frame.as_raw().linesize[0] as usize,
				);
				for j in 0..self.codec_context.as_raw().frame_size {
					data[(2 * j) as usize] = (t.sin() * 10000.0) as u16;

					for k in 1..self.codec_context.as_raw().ch_layout.nb_channels {
						data[(2 * j + k) as usize] = data[(2 * j) as usize];
					}
					t += tincr;
				}
			}

			self.frame.as_raw_mut().pts = i as i64;

			// Encode the image.
			if let Some(client_address) = client_address {
				self.encode(&client_address).await?;
			}

		}

		Ok(())
	}

	async fn encode(
		&mut self,
		client_address: &SocketAddr,
	) -> Result<(), ()> {
		log::trace!("Send frame {}", self.frame.as_raw().pts);

		// Send the frame to the encoder.
		self.codec_context.send_frame(Some(&self.frame))
			.map_err(|e| log::error!("Error sending frame for encoding: {e}"))?;

		loop {
			match self.codec_context.receive_packet(&mut self.packet) {
				Ok(()) => {
					log::info!("Write packet {} (size={})", self.packet.as_raw().pts, self.packet.as_raw().size);
					let data = self.packet.data();
					self.socket.send_to(
						data,
						client_address,
					).await
						.map_err(|e| log::error!("Failed to send packet: {e}"))?;
				},
				Err(e) => {
					if e.code == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
						// log::info!("Need more frames for encoding...");
						return Ok(());
					} else if e.code == ffmpeg_sys::AVERROR_EOF {
						log::info!("End of file");
						return Ok(());
					} else {
						log::error!("Error while encoding: {e}");
						return Err(());
					}
				}
			}
		}
	}
}

unsafe impl Send for AudioStream { }
