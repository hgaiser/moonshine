use std::{net::SocketAddr, io::Write};

use ffmpeg::{CodecContext, Frame, Packet, Codec, CodecContextBuilder, FrameBuilder};
use ffmpeg_sys::AV_PKT_FLAG_KEY;
use tokio::{net::UdpSocket, sync::mpsc};

use crate::session::rtsp::stream::rtp::{RtpHeader, RtpFlag, NvVideoPacket, VideoFrameHeader};

pub(super) enum VideoCommand {
	StartStreaming,
	RequestIdrFrame,
}

#[derive(Clone, Default)]
pub struct VideoStreamConfig {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub packet_size: usize,
	pub bitrate: u64,
	pub minimum_fec_packets: u32,
	pub codec_name: String,
	pub fec_percentage: u32,
}

pub(super) struct VideoStream {
	socket: UdpSocket,
	codec_context: CodecContext,
	frame: Frame,
	packet: Packet,
	sequence_number: u16,
	frame_number: u32,
	config: VideoStreamConfig,
}

impl VideoStream {
	pub(super) async fn new(address: &str, port: u16, config: VideoStreamConfig) -> Result<Self, ()> {
		let socket = UdpSocket::bind((address, port)).await
			.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

		let codec = Codec::new("libx264")
			.map_err(|e| log::error!("Failed to create codec: {e}"))?;

		let mut codec_context_builder = CodecContextBuilder::new(&codec)
			.map_err(|e| log::error!("Failed to create codec context builder: {e}"))?;
		codec_context_builder
			.set_width(config.width)
			.set_height(config.height)
			.set_framerate(config.fps)
			.set_max_b_frames(0)
			.set_pix_fmt(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)
			.set_bit_rate(config.bitrate)
			.set_gop_size(i32::max_value() as u32);
		codec_context_builder.as_raw_mut().refs = 1;

		let codec_context = codec_context_builder.open()
			.map_err(|e| log::error!("Failed to open codec context: {e}"))?;

		let mut frame_builder = FrameBuilder::new()
			.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
		frame_builder
			.set_format(codec_context.as_raw().pix_fmt)
			.set_width(codec_context.as_raw().width as u32)
			.set_height(codec_context.as_raw().height as u32);
		let frame = frame_builder.allocate(0)
			.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

		let packet = Packet::new()
			.map_err(|e| log::error!("Failed to create packet: {e}"))?;

		Ok(Self {
			socket,
			codec_context,
			frame,
			packet,
			sequence_number: 0,
			frame_number: 0,
			config,
		})
	}

	pub(super) async fn run(
		mut self,
		mut video_command_rx: mpsc::Receiver<VideoCommand>,
	) -> Result<(), ()> {
		log::info!(
			"Listening for video messages on {}",
			self.socket.local_addr()
				.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
		);

		let stream_start_time = std::time::Instant::now();
		let mut start_streaming = true;
		let mut buf = [0; 1024];
		let mut client_address = None;
		for _ in 0.. {
			match self.socket.try_recv_from(&mut buf) {
				Ok((len, addr)) => {
					if &buf[..len] == b"PING" {
						log::debug!("Received video stream PING message from {addr}.");
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

			match video_command_rx.try_recv() {
				Ok(command) => {
					match command {
						VideoCommand::RequestIdrFrame => {
							log::info!("Received request for IDR frame, next frame will be an IDR frame.");
							self.frame.as_raw_mut().pict_type = ffmpeg_sys::AVPictureType_AV_PICTURE_TYPE_I;
							self.frame.as_raw_mut().key_frame = 1;
						},
						VideoCommand::StartStreaming => start_streaming = true,
					}
				},
				Err(mpsc::error::TryRecvError::Empty) => { },
				Err(e) => {
					log::error!("Failed to receive video stream command: {e}");
					return Err(());
				}
			}

			// Check if we should already start streaming.
			if !start_streaming || client_address.is_none() {
				continue;
			}

			self.frame.make_writable()
				.map_err(|e| println!("Failed to make frame writable: {e}"))?;

			unsafe {
				// Y
				let y_data = std::slice::from_raw_parts_mut(self.frame.as_raw().data[0], self.frame.as_raw().linesize[0] as usize * self.codec_context.as_raw().height as usize);
				for y in 0..self.codec_context.as_raw().height {
					for x in 0..self.codec_context.as_raw().width {
						y_data[(y * self.frame.as_raw().linesize[0] + x) as usize] = (x + y + self.sequence_number as i32 * 3) as u8;
					}
				}

				// Cb and Cr
				let cb_data = std::slice::from_raw_parts_mut(self.frame.as_raw().data[1], self.frame.as_raw().linesize[1] as usize * self.codec_context.as_raw().height as usize);
				let cr_data = std::slice::from_raw_parts_mut(self.frame.as_raw().data[2], self.frame.as_raw().linesize[2] as usize * self.codec_context.as_raw().height as usize);
				for y in 0..self.codec_context.as_raw().height / 2 {
					for x in 0..self.codec_context.as_raw().width / 2 {
						cb_data[(y * self.frame.as_raw().linesize[1] + x) as usize] = (128 + y + self.sequence_number as i32 * 2) as u8;
						cr_data[(y * self.frame.as_raw().linesize[2] + x) as usize] = (64 + x + self.sequence_number as i32 * 5) as u8;
					}
				}
			}

			self.frame.as_raw_mut().pts = self.frame_number as i64;

			// We increase this value here, because the first value expected is a 1.
			self.frame_number += 1;

			// Encode the image.
			if let Some(client_address) = client_address {
				self.encode(&client_address, stream_start_time).await?;
			}

			// TODO: Check if this is necessary?
			// Reset possible request for keyframe.
			self.frame.as_raw_mut().pict_type = ffmpeg_sys::AVPictureType_AV_PICTURE_TYPE_NONE;
			self.frame.as_raw_mut().key_frame = 0;

			tokio::time::sleep(std::time::Duration::from_millis(33)).await;
		}

		Ok(())
	}

	async fn send_packet(
		&mut self,
		client_address: &SocketAddr,
		stream_start_time: std::time::Instant,
	) -> Result<(), ()> {
		// TODO: Figure out what this header means?
		let video_frame_header = VideoFrameHeader {
			header_type: 0x01, // Always 0x01 for short headers. What is this exactly?
			padding1: 0,
			frame_type: if (self.packet.as_raw().flags & AV_PKT_FLAG_KEY as i32) != 0 { 2 } else { 1 },
			padding2: 0,
		};
		let timestamp = ((std::time::Instant::now() - stream_start_time).as_micros() / (1000 / 90)) as u32;
		log::info!("Timestamp: {}", timestamp);

		let mut buffer = Vec::new();
		video_frame_header.serialize(&mut buffer);
		let packet_data = [&buffer, self.packet.data()].concat();

		let payload_size = self.config.packet_size - std::mem::size_of::<NvVideoPacket>();
		let nr_data_shards = (packet_data.len() + payload_size - 1) / payload_size;
		let nr_parity_shards = (nr_data_shards * self.config.fec_percentage as usize / 100)
			.max(self.config.minimum_fec_packets as usize);
		let fec_percentage = nr_parity_shards * 100 / nr_data_shards;
		log::trace!("Number of packets: {nr_data_shards}, number of parity packets: {nr_parity_shards}");

		let encoder = reed_solomon::ReedSolomon::new(nr_data_shards, nr_parity_shards)
			.map_err(|e| log::error!("Failed to create FEC encoder: {e}"))?;

		let mut shards = Vec::with_capacity(nr_data_shards + nr_parity_shards);
		for i in 0..nr_data_shards {
			let start = i * payload_size;
			let end = ((i + 1) * payload_size).min(packet_data.len());
			log::info!("total: {}, blocksize: {}, i: {}, start: {}, end: {}", packet_data.len(), payload_size, i, start, end);

			// TODO: Do this without cloning.
			let mut shard = vec![0u8; payload_size];
			shard[..(end - start)].copy_from_slice(&packet_data[start..end]);
			shards.push(shard);
		}
		for _ in 0..nr_parity_shards {
			shards.push(vec![0u8; payload_size]);
		}
		encoder.encode(&mut shards)
			.map_err(|e| log::error!("Failed to encode packet as FEC shards: {e}"))?;

		for (index, shard) in shards.iter().enumerate() {
			let rtp_header = RtpHeader {
				header: 0x90, // What is this?
				packet_type: 0,
				sequence_number: self.sequence_number,
				timestamp,
				ssrc: 0,
				padding: 0,
			};

			let mut video_packet_header = NvVideoPacket {
				stream_packet_index: (self.sequence_number as u32) << 8,
				frame_index: self.frame_number,
				flags: RtpFlag::ContainsPicData as u8,
				reserved: 0,
				multi_fec_flags: 0x10,
				multi_fec_blocks: 0, // TODO: Support multiple blocks
				fec_info: (index << 12 | nr_data_shards << 22 | fec_percentage << 4) as u32,
			};
			if index == 0 {
				video_packet_header.flags |= RtpFlag::StartOfFrame as u8;
			}
			if index == nr_data_shards - 1 {
				video_packet_header.flags |= RtpFlag::EndOfFrame as u8;
			}

			let mut buffer = Vec::with_capacity(
				std::mem::size_of::<RtpHeader>()
				+ std::mem::size_of::<NvVideoPacket>()
				+ shard.len(),
			);
			rtp_header.serialize(&mut buffer);
			video_packet_header.serialize(&mut buffer);
			buffer.extend(shard);

			log::info!("Sending packet {}/{} with size {} bytes.", index + 1, shards.len(), buffer.len());
			self.socket.send_to(
				buffer.as_slice(),
				client_address,
			).await
				.map_err(|e| log::error!("Failed to send packet: {e}"))?;

			self.sequence_number += 1;
		}

		Ok(())
	}

	async fn encode(
		&mut self,
		client_address: &SocketAddr,
		stream_start_time: std::time::Instant,
	) -> Result<(), ()> {
		log::trace!("Send frame {}", self.frame.as_raw().pts);

		// Send the frame to the encoder.
		self.codec_context.send_frame(Some(&self.frame))
			.map_err(|e| log::error!("Error sending frame for encoding: {e}"))?;

		loop {
			match self.codec_context.receive_packet(&mut self.packet) {
				Ok(()) => self.send_packet(client_address, stream_start_time).await?,
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
