use std::net::{ToSocketAddrs, SocketAddr};

use nvfbc::{CudaCapturer, cuda::CaptureMethod, BufferFormat};
use rtsp_types::{Method, headers::{self, Transport}, Response, Empty};
use tokio::{net::TcpListener, io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt}};

use crate::{encoder::{NvencEncoder, VideoQuality, CodecType}, cuda};

pub async fn run(address: String, port: u16) -> Result<(), ()> {
	let address = (address.clone(), port).to_socket_addrs()
		.map_err(|e| log::error!("Failed to resolve address {}:{}: {}", address, port, e))?
		.next()
		.ok_or_else(|| log::error!("Failed to resolve address {}:{}", address, port))?;
	let listener = TcpListener::bind(address)
		.await
		.map_err(|e| log::error!("Failed to bind to address {}: {}", address, e))?;

	log::info!("RTSP server listening on {}", address);

	loop {
		let (connection, address) = listener.accept()
			.await
			.map_err(|e| log::error!("Failed to accept connection: {}", e))?;
		log::debug!("Accepted connection from {}", address);

		tokio::spawn(handle_connection(connection, address));
	}
}

async fn handle_connection<C>(mut connection: C, address: SocketAddr) -> Result<(), ()>
where
	C: AsyncRead + AsyncReadExt + AsyncWrite + AsyncWriteExt + Unpin + 'static,
{
	loop {
		let mut buffer = [0u8; 1024];
		connection.read(&mut buffer).await
			.map_err(|e| log::error!("Failed to read from connection '{}': {}", address, e))?;

		let (message, consumed): (rtsp_types::Message<Vec<u8>>, _) = rtsp_types::Message::parse(&buffer)
			.map_err(|e| log::error!("Failed to parse request as RTSP message: {}", e))?;

		log::trace!("Consumed {} bytes into RTSP request: {:#?}", consumed, message);

		let response = match message {
			rtsp_types::Message::Request(ref request) => handle_request(request).await,
			_ => {
				log::error!("Unknown RTSP message type received");
				Err(())
			}
		}?;

		log::debug!("Sending RTSP response");
		log::trace!("{:#?}", response);

		let mut buffer = Vec::new();
		response.write(&mut buffer)
			.map_err(|e| log::error!("Failed to serialize RTSP response: {}", e))?;

		connection.write_all(&buffer).await
			.map_err(|e| log::error!("Failed to send RTSP response: {}", e))?;
	}
}

async fn handle_request(request: &rtsp_types::Request<Vec<u8>>) -> Result<Response<Vec<u8>>, ()> {
	log::debug!("Received RTSP {:?} request", request.method());

	let cseq: i32 = request.header(&headers::CSEQ)
		.ok_or_else(|| log::error!("RTSP request has no CSeq header"))?
		.as_str()
		.parse()
		.map_err(|e| log::error!("Failed to parse cseq header: {}", e))?
	;

	match request.method() {
		Method::Options => Ok(handle_options_request(request, cseq)),
		Method::Describe => handle_describe_request(request, cseq),
		Method::Setup => handle_setup_request(request, cseq),
		Method::Play => Ok(handle_play_request(request, cseq)),
		method => {
			log::error!("Received request with unsupported method {:?}", method);
			Err(())
		}
	}
}

fn handle_options_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
	rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.header(headers::PUBLIC, "OPTIONS DESCRIBE")
		.build(Vec::new())
}

fn handle_describe_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	// TODO: Enable HEVC support.
	// let sdp = "sprop-parameter-sets=AAAAAU\n".to_string();
	let sdp = sdp_types::Session {
		origin: sdp_types::Origin {
			username: Some("-".to_string()),
			sess_id: "0".to_string(), // TODO: Generate this.
			sess_version: 0,
			nettype: "IN".to_string(),
			addrtype: "IP4".to_string(), // TODO: Support ipv6.
			unicast_address: "127.0.0.1".to_string(),
		},
		session_name: " ".to_string(),
		session_description: Some("Moonshine stream session.".to_string()),
		uri: None,
		emails: Vec::new(),
		phones: Vec::new(),
		connection: Some(sdp_types::Connection {
			nettype: "IN".to_string(),
			addrtype: "IP4".to_string(),
			connection_address: "127.0.0.1".to_string(),
		}),
		bandwidths: Vec::new(),
		times: Vec::new(),
		time_zones: Vec::new(),
		key: None,
		attributes: Vec::new(),
		medias: vec![
			sdp_types::Media {
				media: "video".to_string(),
				port: 1337,
				num_ports: None,
				proto: "RTP/AVP".to_string(),
				fmt: "".to_string(), // ?
				media_title: None,
				connections: Vec::new(),
				bandwidths: Vec::new(),
				key: None,
				attributes: Vec::new(),
			},
		],
	};

	let mut buffer = Vec::new();
	sdp.write(&mut buffer)
		.map_err(|e| log::error!("Failed to write SDP data to buffer: {}", e))?;

	let debug = String::from_utf8(buffer.clone())
		.map_err(|e| log::error!("Failed to write SDP debug string: {}", e))?;
	log::trace!("SDP session data: \n{}", debug.trim());

	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(buffer)
	)
}

fn handle_setup_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let transports = request
		.typed_header::<rtsp_types::headers::Transports>()
		.map_err(|e| {
			log::error!("Failed to parse transport information from SETUP request: {}", e);
		})?
		.ok_or_else(|| log::error!("No transport information in SETUP request."))?;

	for transport in &*transports {
		match transport {
			Transport::Rtp(transport) => {
				let (rtp_port, rtcp_port) = transport.params.client_port
					.ok_or_else(|| log::error!("No client_port in SETUP request."))?;
				let rtc_port = rtcp_port.ok_or_else(|| log::error!("No RTC port in SETUP request."))?;

				// rtp_port = ffmpeg_sys::ff_rtp_get_local_rtp_port(rtp_c->rtp_handles[stream_index]);
				// rtcp_port = ffmpeg_sys::ff_rtp_get_local_rtcp_port(rtp_c->rtp_handles[stream_index]);

				log::info!("Client port: {}-{}", rtp_port, rtc_port);

				tokio::spawn(async move {
					let cuda_context = cuda::init_cuda(0)
						.map_err(|e| log::error!("Failed to initialize CUDA: {}", e)).unwrap();

					// Create a capturer that captures to CUDA context.
					let mut capturer = CudaCapturer::new()
						.map_err(|e| log::error!("Failed to create CUDA capture device: {}", e)).unwrap();

					let status = capturer.status()
						.map_err(|e| log::error!("Failed to get capturer status: {}", e)).unwrap();
					println!("{:#?}", status);
					if !status.can_create_now {
						panic!("Can't create a CUDA capture session.");
					}

					let width = status.screen_size.w;
					let height = status.screen_size.h;
					let fps = 60;

					capturer.start(BufferFormat::Bgra, fps)
						.map_err(|e| log::error!("Failed to start frame capturer: {}", e)).unwrap();

					let mut encoder = NvencEncoder::new(
						rtp_port,
						width,
						height,
						CodecType::H264,
						VideoQuality::Slowest,
						cuda_context,
					).unwrap();

					let start_time = std::time::Instant::now();
					while start_time.elapsed().as_secs() < 20 {
						let start = std::time::Instant::now();
						let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
							.map_err(|e| log::error!("Failed to capture frame: {}", e)).unwrap();
						encoder.encode(frame_info.device_buffer, start_time.elapsed())
							.map_err(|e| log::error!("Failed to encode frame: {}", e)).unwrap();
						println!("Capture: {}msec", start.elapsed().as_millis());
					}

					encoder.stop().unwrap();
				});

				return Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
					.header(headers::CSEQ, cseq.to_string())
					.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
					.header(headers::TRANSPORT, format!("RTP/AVP/UDP;unicast;client_port={}-{};server_port=2001", rtp_port, rtc_port))
					.build(Vec::new())
				);
			}
			t => {
				log::error!("Received request for unsupported transport: {:?}", t);
				return Err(());
			}
		}
	}

	log::error!("No transports found in SETUP request.");
	Err(())
}

fn handle_play_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
	rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new())
}
