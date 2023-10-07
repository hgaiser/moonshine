use std::{net::{ToSocketAddrs, SocketAddr}, sync::Arc};
use rtsp_types::{headers::{self, Transport}, Method};
use tokio::{net::{TcpListener, TcpStream}, io::{AsyncReadExt, AsyncWriteExt}, sync::Mutex};
use stream::Session;

use crate::config::Config;

use super::SessionContext;

mod stream;

pub async fn run(
	config: Config,
	context: SessionContext,
) -> Result<(), ()> {
	let address = (config.address.as_str(), config.stream.port).to_socket_addrs()
		.map_err(|e| log::error!("Failed to resolve address {}:{}: {}", config.address, config.stream.port, e))?
		.next()
		.ok_or_else(|| log::error!("Failed to resolve address {}:{}", config.address, config.stream.port))?;
	let listener = TcpListener::bind(address)
		.await
		.map_err(|e| log::error!("Failed to bind to address {}: {}", address, e))?;

	log::info!("RTSP server listening on {}", address);

	let session = Arc::new(Mutex::new(Session::new(config.clone()).await?));
	loop {
		let (connection, address) = listener.accept()
			.await
			.map_err(|e| log::error!("Failed to accept connection: {}", e))?;
		log::debug!("Accepted connection from {}", address);

		tokio::spawn(handle_connection(
			connection,
			address,
			config.clone(),
			context.clone(),
			session.clone(),
		));
	}
}

async fn handle_connection(
	mut connection: TcpStream,
	address: SocketAddr,
	config: Config,
	context: SessionContext,
	session: Arc<Mutex<Session>>,
) -> Result<(), ()> {
	let mut message_buffer = String::new();

	let message = loop {
		let mut buffer = [0u8; 2048];
		let bytes_read = connection.read(&mut buffer).await
			.map_err(|e| log::error!("Failed to read from connection '{}': {}", address, e))?;
		if bytes_read == 0 {
			log::warn!("Received empty RTSP request.");
			return Ok(());
		}
		message_buffer.push_str(std::str::from_utf8(&buffer[..bytes_read])
			.map_err(|e| log::error!("Failed to convert message to string: {e}"))?);

		// Hacky workaround to fix rtsp_types parsing SETUP/PLAY requests from Moonlight.
		let message_buffer = message_buffer.replace("streamid", "rtsp://localhost?streamid");
		let message_buffer = message_buffer.replace("PLAY /", "PLAY rtsp://localhost/");

		log::trace!("Request: {}", message_buffer);
		let result = rtsp_types::Message::parse(&message_buffer);

		break match result {
			Ok((message, _consumed)) => message,
			Err(rtsp_types::ParseError::Incomplete(_)) => {
				log::debug!("Incomplete RTSP message received, waiting for more data.");
				continue;
			},
			Err(e) => {
				log::error!("Failed to parse request as RTSP message: {}", e);
				return Err(());
			}
		};
	};

	// log::trace!("Consumed {} bytes into RTSP request: {:#?}", consumed, message);

	let response = match message {
		rtsp_types::Message::Request(ref request) => {
			log::debug!("Received RTSP {:?} request", request.method());

			let cseq: i32 = request.header(&headers::CSEQ)
				.ok_or_else(|| log::error!("RTSP request has no CSeq header"))?
				.as_str()
				.parse()
				.map_err(|e| log::error!("Failed to parse CSeq header: {}", e))?;

			match request.method() {
				Method::Announce => handle_announce_request(request, cseq, session.clone()).await,
				Method::Describe => handle_describe_request(request, cseq, session.clone()).await,
				Method::Options => handle_options_request(request, cseq),
				Method::Setup => handle_setup_request(request, cseq, &config),
				Method::Play => handle_play_request(request, cseq, context.clone(), session.clone()),
				method => {
					log::error!("Received request with unsupported method {:?}", method);
					Err(())
				}
			}
		},
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

	// For some reason, Moonlight expects a connection per request, so we close the connection here.
	connection.shutdown()
		.await
		.map_err(|e| log::error!("Failed to shutdown the connection: {e}"))?;

	Ok(())
}

async fn handle_announce_request(
	request: &rtsp_types::Request<Vec<u8>>,
	cseq: i32,
	session: Arc<Mutex<Session>>,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let sdp_session = sdp_types::Session::parse(request.body())
		.map_err(|e| log::error!("Failed to parse ANNOUNCE request as SDP session: {e}"))?;

	log::trace!("Received SDP session from ANNOUNCE request: {sdp_session:#?}");

	let mut session = session.lock().await;
	session.video_stream_context.width = sdp_session.get_first_attribute_value("x-nv-video[0].clientViewportWd")
		.map_err(|e| log::error!("Failed to get width attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No width attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("Width attribute is not an integer: {e}"))?;
	session.video_stream_context.height = sdp_session.get_first_attribute_value("x-nv-video[0].clientViewportHt")
		.map_err(|e| log::error!("Failed to get height attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No height attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("Height attribute is not an integer: {e}"))?;
	session.video_stream_context.fps = sdp_session.get_first_attribute_value("x-nv-video[0].maxFPS")
		.map_err(|e| log::error!("Failed to get FPS attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No FPS attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("FPS attribute is not an integer: {e}"))?;
	session.video_stream_context.packet_size = sdp_session.get_first_attribute_value("x-nv-video[0].packetSize")
		.map_err(|e| log::error!("Failed to get packet size attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No packet size attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("Packet size attribute is not an integer: {e}"))?;
	session.video_stream_context.bitrate = sdp_session.get_first_attribute_value("x-nv-vqos[0].bw.maximumBitrateKbps")
		.map_err(|e| log::error!("Failed to get bitrate attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No bitrate attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("Bitrate attribute is not an integer: {e}"))?;
	session.video_stream_context.bitrate *= 1024; // Convert from kbps to bps.
	session.video_stream_context.minimum_fec_packets = sdp_session.get_first_attribute_value("x-nv-vqos[0].fec.minRequiredFecPackets")
		.map_err(|e| log::error!("Failed to get minimum required FEC packets attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No minimum required FEC packets attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("Minimum required FEC packets attribute is not an integer: {e}"))?;
	session.audio_stream_context.packet_duration = sdp_session.get_first_attribute_value("x-nv-aqos.packetDuration")
		.map_err(|e| log::error!("Failed to get packet duration attribute from announce request: {e}"))?
		.ok_or_else(|| log::error!("No packet duration attribute in announce request"))?
		.trim()
		.parse()
		.map_err(|e| log::error!("Packet duration attribute is not an integer: {e}"))?;

	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new()))
}

fn handle_options_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.header(headers::PUBLIC, "OPTIONS DESCRIBE SETUP PLAY")
		.build(Vec::new()))
}

async fn handle_describe_request(
	request: &rtsp_types::Request<Vec<u8>>,
	cseq: i32,
	session: Arc<Mutex<Session>>,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let mut buffer = Vec::new();
	session.lock().await.description()?.write(&mut buffer)
		.map_err(|e| log::error!("Failed to write SDP data to buffer: {}", e))?;

	let debug = String::from_utf8(buffer.clone())
		.map_err(|e| log::error!("Failed to write SDP debug string: {}", e))?;
	log::trace!("SDP session data: \n{}", debug.trim());

	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(buffer)
	)
}

fn handle_setup_request(
	request: &rtsp_types::Request<Vec<u8>>,
	cseq: i32,
	config: &Config,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let transports = request
		.typed_header::<rtsp_types::headers::Transports>()
		.map_err(|e| { log::error!("Failed to parse transport information from SETUP request: {}", e) })?
		.ok_or_else(|| log::error!("No transport information in SETUP request."))?;

	if let Some(transport) = (*transports).first() {
		match transport {
			Transport::Other(_transport) => {
				let request_uri = request.request_uri()
					.ok_or_else(|| log::error!("No request URI in SETUP request."))?;
				let query = request_uri.query_pairs().next()
					.ok_or_else(||log::error!("No query in request URI in SETUP request."))?;
				if query.0 != "streamid" {
					log::error!("Expected only one query parameter with 'streamid', but didn't find it.");
					return Err(());
				}

				// Example query: streamid=control/13/0
				let (stream_id, port) = match query.1.split('/').next() {
					Some("video") => ("video", config.stream.video.port),
					Some("audio") => ("audio", config.stream.audio.port),
					Some("control") => ("control", config.stream.control.port),
					Some(stream) => {
						log::error!("Unknown stream '{stream}'");
						return Err(());
					}
					None => {
						log::error!("Unexpected query format for query '{}'", query.1);
						return Err(());
					},
				};

				log::info!("Responding with server_port={port} for stream '{stream_id}'.");

				return Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
					.header(headers::CSEQ, cseq.to_string())
					.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
					.header(headers::TRANSPORT, format!("server_port={port}"))
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

fn handle_play_request(
	request: &rtsp_types::Request<Vec<u8>>,
	cseq: i32,
	context: SessionContext,
	session: Arc<Mutex<Session>>,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	// TODO: Think of a better way to prevent double running.
	let locked = session.try_lock().is_err();
	if !locked {
		tokio::task::spawn(async move {
			session.lock().await.run(context).await
		});
	}

	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new()))
}

