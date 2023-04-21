use std::{net::SocketAddr, fs::File, os::fd::FromRawFd};

use ffmpeg::{Packet, Codec, CodecContextBuilder, FrameBuilder};
use ffmpeg_sys::AV_PKT_FLAG_KEY;
use memmap::MmapOptions;
use tokio::{net::UdpSocket, sync::mpsc};
use xcb::{x, shm};

use crate::config::Config;

#[repr(u8)]
pub(super) enum RtpFlag {
	ContainsPicData = 0x1,
	EndOfFrame = 0x2,
	StartOfFrame = 0x4,
}

pub(super) struct VideoFrameHeader {
	pub(super) header_type: u8,
	pub(super) padding1: u16,
	pub(super) frame_type: u8,
	pub(super) padding2: u32,
}

impl VideoFrameHeader {
	pub(super) fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.header_type.to_le_bytes());
		buffer.extend(self.padding1.to_le_bytes());
		buffer.extend(self.frame_type.to_le_bytes());
		buffer.extend(self.padding2.to_le_bytes());
	}
}

pub(super) struct RtpHeader {
	pub(super) header: u8,
	pub(super) packet_type: u8,
	pub(super) sequence_number: u16,
	pub(super) timestamp: u32,
	pub(super) ssrc: u32,
	pub(super) padding: u32,
}

impl RtpHeader {
	pub(super) fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.header.to_be_bytes());
		buffer.extend(self.packet_type.to_be_bytes());
		buffer.extend(self.sequence_number.to_be_bytes());
		buffer.extend(self.timestamp.to_be_bytes());
		buffer.extend(self.ssrc.to_be_bytes());
		buffer.extend(self.padding.to_be_bytes());
	}
}

pub(super) struct NvVideoPacket {
	pub(super) stream_packet_index: u32,
	pub(super) frame_index: u32,
	pub(super) flags: u8,
	pub(super) reserved: u8,
	pub(super) multi_fec_flags: u8,
	pub(super) multi_fec_blocks: u8,
	pub(super) fec_info: u32,
}

impl NvVideoPacket {
	pub(super) fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.stream_packet_index.to_le_bytes());
		buffer.extend(self.frame_index.to_le_bytes());
		buffer.extend(self.flags.to_le_bytes());
		buffer.extend(self.reserved.to_le_bytes());
		buffer.extend(self.multi_fec_flags.to_le_bytes());
		buffer.extend(self.multi_fec_blocks.to_le_bytes());
		buffer.extend(self.fec_info.to_le_bytes());
	}
}

pub(super) enum VideoCommand {
	StartStreaming,
	RequestIdrFrame,
}

#[derive(Clone, Default)]
pub struct VideoStreamContext {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub packet_size: usize,
	pub bitrate: u64,
	pub minimum_fec_packets: u32,
}

pub(super) async fn run_video_stream(
	config: Config,
	context: VideoStreamContext,
	mut video_command_rx: mpsc::Receiver<VideoCommand>,
) -> Result<(), ()> {
	let socket = UdpSocket::bind((config.address, config.stream.video.port)).await
		.map_err(|e| log::error!("Failed to bind to UDP socket: {e}"))?;

	let codec = Codec::new(&config.stream.video.codec)
		.map_err(|e| log::error!("Failed to create codec: {e}"))?;

	let mut codec_context_builder = CodecContextBuilder::new(&codec)
		.map_err(|e| log::error!("Failed to create codec context builder: {e}"))?;
	codec_context_builder
		.set_width(context.width)
		.set_height(context.height)
		.set_framerate(context.fps)
		.set_max_b_frames(0)
		.set_pix_fmt(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P)
		.set_bit_rate(context.bitrate)
		.set_gop_size(i32::max_value() as u32)
		.set_preset("ultrafast")
		.set_tune("zerolatency")
	;
	codec_context_builder.as_raw_mut().refs = 1;

	let mut codec_context = codec_context_builder.open()
		.map_err(|e| log::error!("Failed to open codec context: {e}"))?;

	let mut frame_builder = FrameBuilder::new()
		.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
	frame_builder
		.set_format(ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_BGRA)
		.set_width(codec_context.as_raw().width as u32)
		.set_height(codec_context.as_raw().height as u32);
	let raw_frame = frame_builder.allocate(0)
		.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

	let mut frame_builder = FrameBuilder::new()
		.map_err(|e| log::error!("Failed to create frame builder: {e}"))?;
	frame_builder
		.set_format(codec_context.as_raw().pix_fmt)
		.set_width(codec_context.as_raw().width as u32)
		.set_height(codec_context.as_raw().height as u32);
	let mut frame = frame_builder.allocate(0)
		.map_err(|e| log::error!("Failed to allocate frame: {e}"))?;

	let mut packet = Packet::new()
		.map_err(|e| log::error!("Failed to create packet: {e}"))?;

	log::info!(
		"Listening for video messages on {}",
		socket.local_addr()
		.map_err(|e| log::error!("Failed to get local address associated with control socket: {e}"))?
	);

	let (conn, screen_num) = xcb::Connection::connect(None).unwrap();
	let setup = conn.get_setup();
	let screen = setup.roots().nth(screen_num as usize).unwrap();

	let width = screen.width_in_pixels();
	let height = screen.height_in_pixels();

	let shmseg = conn.generate_id();
	let cookie = conn.send_request(&shm::CreateSegment {
		shmseg,
		size: width as u32 * height as u32 * 4,
		read_only: false,
	});
	let segment = conn.wait_for_reply(cookie).unwrap();
	let shared_file = unsafe { File::from_raw_fd(segment.shm_fd()) };
	let mmap = unsafe { MmapOptions::new().map(&shared_file).unwrap() };

	let stream_start_time = std::time::Instant::now();
	let mut start_streaming = true;
	let mut buf = [0; 1024];
	let mut client_address = None;
	let mut frame_number = 0u32;
	let mut sequence_number = 0u16;
	let sws_context = ffmpeg::SwsContext::new(
		(width as u32, height as u32), raw_frame.as_raw().format,
		(context.width, context.height), codec_context.as_raw().pix_fmt,
		ffmpeg_sys::SWS_FAST_BILINEAR,
	);
	for _ in 0.. {
		match socket.try_recv_from(&mut buf) {
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
						frame.as_raw_mut().pict_type = ffmpeg_sys::AVPictureType_AV_PICTURE_TYPE_I;
						frame.as_raw_mut().key_frame = 1;
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
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			continue;
		}

		raw_frame.make_writable()
			.map_err(|e| println!("Failed to make frame writable: {e}"))?;
		frame.make_writable()
			.map_err(|e| println!("Failed to make frame writable: {e}"))?;

		let cookie = conn.send_request(&shm::GetImage {
			format: x::ImageFormat::ZPixmap as u8,
			drawable: x::Drawable::Window(screen.root()),
			x: 0,
			y: 0,
			width,
			height,
			plane_mask: u32::MAX,
			shmseg,
			offset: 0,
		});

		conn.wait_for_reply(cookie).unwrap();
		// unsafe {
		// 	// B
		// 	let data = std::slice::from_raw_parts_mut(
		// 		raw_frame.as_raw().data[0],
		// 		raw_frame.as_raw().linesize[0] as usize * codec_context.as_raw().height as usize * 4,
		// 	);
		// 	for y in 0..codec_context.as_raw().height {
		// 		for x in 0..codec_context.as_raw().width {
		// 			let index = (y as usize * frame.as_raw().linesize[0] as usize + x as usize) * 4;
		// 			data[index + 0] = (x + y + sequence_number as i32 * 3) as u8;
		// 			data[index + 1] = (x + y + sequence_number as i32 * 3) as u8;
		// 			data[index + 2] = 255 - (x + y + sequence_number as i32 * 3) as u8;
		// 			data[index + 3] = 255;
		// 		}
		// 	}
		// }

		sws_context.scale(
			[mmap.as_ptr()].as_ptr(),
			&[width as i32 * 4],
			height as i32,
			frame.as_raw_mut().data.as_mut_ptr(),
			frame.as_raw().linesize.as_slice(),
		);
		frame.as_raw_mut().pts = frame_number as i64;

		// We increase this value here, because the first value expected is a 1.
		frame_number += 1;

		// Encode the image.
		if let Some(client_address) = client_address {
			log::debug!("Encoding frame {}", frame.as_raw().pts);

			// Send the frame to the encoder.
			codec_context.send_frame(Some(&frame))
				.map_err(|e| log::error!("Error sending frame for encoding: {e}"))?;

			loop {
				match codec_context.receive_packet(&mut packet) {
					Ok(()) => {
						log::debug!("Sending frame {}", packet.as_raw().pts);
						send_packet(
							&packet,
							&socket,
							&context,
							config.stream.video.fec_percentage,
							frame_number,
							&mut sequence_number,
							&client_address,
							stream_start_time,
						).await?
					},
					Err(e) => {
						if e.code == ffmpeg_sys::av_error(ffmpeg_sys::EAGAIN as i32) {
							// log::info!("Need more frames for encoding...");
							break;
						} else if e.code == ffmpeg_sys::AVERROR_EOF {
							log::info!("End of file");
							break;
						} else {
							log::error!("Error while encoding: {e}");
							break;
						}
					}
				}
			}
		}

		// TODO: Check if this is necessary?
		// Reset possible request for keyframe.
		frame.as_raw_mut().pict_type = ffmpeg_sys::AVPictureType_AV_PICTURE_TYPE_NONE;
		frame.as_raw_mut().key_frame = 0;

		tokio::time::sleep(std::time::Duration::from_millis(33)).await;
	}

	Ok(())
}

async fn send_packet(
	packet: &Packet,
	socket: &UdpSocket,
	context: &VideoStreamContext,
	fec_percentage: u8,
	frame_number: u32,
	sequence_number: &mut u16,
	client_address: &SocketAddr,
	stream_start_time: std::time::Instant,
) -> Result<(), ()> {
	// TODO: Figure out what this header means?
	let video_frame_header = VideoFrameHeader {
		header_type: 0x01, // Always 0x01 for short headers. What is this exactly?
		padding1: 0,
		frame_type: if (packet.as_raw().flags & AV_PKT_FLAG_KEY as i32) != 0 { 2 } else { 1 },
		padding2: 0,
	};
	let timestamp = ((std::time::Instant::now() - stream_start_time).as_micros() / (1000 / 90)) as u32;

	let mut buffer = Vec::new();
	video_frame_header.serialize(&mut buffer);
	let packet_data = [&buffer, packet.data()].concat();

	let payload_size = context.packet_size - std::mem::size_of::<NvVideoPacket>();
	let nr_data_shards = (packet_data.len() + payload_size - 1) / payload_size;
	let mut nr_parity_shards = (nr_data_shards * fec_percentage as usize / 100)
		.max(context.minimum_fec_packets as usize);
	let fec_percentage = nr_parity_shards * 100 / nr_data_shards;
	log::debug!("Number of packets: {nr_data_shards}, number of parity packets: {nr_parity_shards}");

	let encoder = reed_solomon::ReedSolomon::new(nr_data_shards, nr_parity_shards);
	if let Err(e) = &encoder {
		log::info!("Failed to create error correction: {e}");
		nr_parity_shards = 0;
	}

	let mut shards = Vec::with_capacity(nr_data_shards + nr_parity_shards);
	for i in 0..nr_data_shards {
		let start = i * payload_size;
		let end = ((i + 1) * payload_size).min(packet_data.len());

		// TODO: Do this without cloning.
		let mut shard = vec![0u8; payload_size];
		shard[..(end - start)].copy_from_slice(&packet_data[start..end]);
		shards.push(shard);
	}
	for _ in 0..nr_parity_shards {
		shards.push(vec![0u8; payload_size]);
	}
	if let Ok(encoder) = encoder {
		encoder.encode(&mut shards)
			.map_err(|e| log::error!("Failed to encode packet as FEC shards: {e}"))?;
	}

	for (index, shard) in shards.iter().enumerate() {
		let rtp_header = RtpHeader {
			header: 0x90, // What is this?
			packet_type: 0,
			sequence_number: *sequence_number,
			timestamp,
			ssrc: 0,
			padding: 0,
		};

		let mut video_packet_header = NvVideoPacket {
			stream_packet_index: (*sequence_number as u32) << 8,
			frame_index: frame_number,
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

		log::trace!("Sending packet {}/{} with size {} bytes.", index + 1, shards.len(), buffer.len());
		socket.send_to(
			buffer.as_slice(),
			client_address,
		).await
			.map_err(|e| log::error!("Failed to send packet: {e}"))?;

		*sequence_number += 1;
	}

	Ok(())
}
