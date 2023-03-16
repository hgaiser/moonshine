use std::net::SocketAddr;

use ffmpeg::{CodecContext, Frame, Packet, Codec, CodecContextBuilder, FrameBuilder};
use tokio::net::UdpSocket;

pub(super) struct VideoStream {
	socket: UdpSocket,
	codec_context: CodecContext,
	frame: Frame,
	packet: Packet,
}

impl VideoStream {
	pub(super) async fn new(address: &str, port: u16) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port)).await
			.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

		let codec = Codec::new("libx264")
			.map_err(|e| log::error!("Failed to create codec: {e}"))?;

		let mut codec_context_builder = CodecContextBuilder::new(&codec)
			.map_err(|e| log::error!("Failed to create codec context builder: {e}"))?;
		codec_context_builder
			.set_width(2560)
			.set_height(1600)
			.set_framerate(60)
			.set_max_b_frames(1)
			.set_pix_fmt(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)
			.set_bit_rate(1000000)
			.set_gop_size(30);
		let codec_context = codec_context_builder.open()
			.map_err(|e| log::error!("Failed to open codec context: {e}"))?;

		let mut frame_builder = FrameBuilder::new()
			.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
		frame_builder.set_format(codec_context.as_raw().pix_fmt);
		frame_builder.set_width(codec_context.as_raw().width as u32);
		frame_builder.set_height(codec_context.as_raw().height as u32);
		let frame = frame_builder.allocate(0)
			.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

		let packet = Packet::new()
			.map_err(|e| log::error!("Failed to create packet: {e}"))?;

		Ok(Self {
			socket,
			codec_context,
			frame,
			packet,
		})
	}

	pub(super) async fn run(mut self) -> Result<(), ()> {
		log::info!(
			"Listening for video messages on {}",
			self.socket.local_addr()
				.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let mut buf = [0; 1024];
		let mut client_address = None;
		for i in 0.. {
			match self.socket.try_recv_from(&mut buf) {
				Ok((len, addr)) => {
					if &buf[..len] == b"PING" {
						log::info!("Received video stream PING message from {addr}.");
						client_address = Some(addr);
					} else {
						log::warn!("Received unknown message on video stream of length {len}.");
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

			unsafe {
				// Y
				let y_data = std::slice::from_raw_parts_mut(self.frame.as_raw().data[0], self.frame.as_raw().linesize[0] as usize * self.codec_context.as_raw().height as usize);
				for y in 0..self.codec_context.as_raw().height {
					for x in 0..self.codec_context.as_raw().width {
						y_data[(y * self.frame.as_raw().linesize[0] + x) as usize] = (x + y + i * 3) as u8;
					}
				}

				// Cb and Cr
				let cb_data = std::slice::from_raw_parts_mut(self.frame.as_raw().data[1], self.frame.as_raw().linesize[1] as usize * self.codec_context.as_raw().height as usize);
				let cr_data = std::slice::from_raw_parts_mut(self.frame.as_raw().data[2], self.frame.as_raw().linesize[2] as usize * self.codec_context.as_raw().height as usize);
				for y in 0..self.codec_context.as_raw().height / 2 {
					for x in 0..self.codec_context.as_raw().width / 2 {
						cb_data[(y * self.frame.as_raw().linesize[1] + x) as usize] = (128 + y + i * 2) as u8;
						cr_data[(y * self.frame.as_raw().linesize[2] + x) as usize] = (64 + x + i * 5) as u8;
					}
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

unsafe impl Send for VideoStream { }
