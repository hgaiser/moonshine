use std::{net::{ToSocketAddrs, SocketAddr}, sync::Arc};
use rtsp_types::{headers::{self, Transport}, Method};
use tokio::{net::{TcpListener, TcpStream}, io::{AsyncReadExt, AsyncWriteExt}, sync::Mutex};
use session::Session;

mod session;

pub async fn run(address: String, port: u16) -> Result<(), ()> {
	let address = (address.clone(), port).to_socket_addrs()
		.map_err(|e| log::error!("Failed to resolve address {}:{}: {}", address, port, e))?
		.next()
		.ok_or_else(|| log::error!("Failed to resolve address {}:{}", address, port))?;
	let listener = TcpListener::bind(address)
		.await
		.map_err(|e| log::error!("Failed to bind to address {}: {}", address, e))?;

	log::info!("RTSP server listening on {}", address);

	let session = Arc::new(Mutex::new(Session::new().await?));
	loop {
		let (connection, address) = listener.accept()
			.await
			.map_err(|e| log::error!("Failed to accept connection: {}", e))?;
		log::debug!("Accepted connection from {}", address);

		tokio::spawn(handle_connection(connection, address, session.clone()));
	}
}

async fn handle_connection(
	mut connection: TcpStream,
	address: SocketAddr,
	session: Arc<Mutex<Session>>,
) -> Result<(), ()> {
	let mut buffer = [0u8; 2048];
	let bytes_read = connection.read(&mut buffer).await
		.map_err(|e| log::error!("Failed to read from connection '{}': {}", address, e))?;
	if bytes_read == 0 {
		log::warn!("Received empty RTSP request.");
		return Ok(());
	}

	let buffer = &buffer[..bytes_read];
	let buffer = String::from_utf8(buffer.to_vec())
		.map_err(|e| log::error!("Failed to convert message to string: {e}"))?;

	// Hacky workaround to fix rtsp_types parsing SETUP/PLAY requests from Moonlight.
	let buffer = buffer.replace("streamid", "rtsp://localhost?streamid");
	let buffer = buffer.replace("PLAY /", "PLAY rtsp://localhost/");

	log::trace!("Request: {}", buffer);
	let (message, _consumed): (rtsp_types::Message<Vec<u8>>, _) = rtsp_types::Message::parse(&buffer)
		.map_err(|e| log::error!("Failed to parse request as RTSP message: {}", e))?;

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
				Method::Announce => handle_announce_request(request, cseq),
				Method::Describe => handle_describe_request(request, cseq, session.clone()).await,
				Method::Options => handle_options_request(request, cseq),
				Method::Setup => handle_setup_request(request, cseq),
				Method::Play => handle_play_request(request, cseq, session.clone()),
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

fn handle_announce_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let sdp_session = sdp_types::Session::parse(request.body())
		.map_err(|e| log::error!("Failed to parse ANNOUNCE request as SDP session: {e}"))?;

	log::trace!("Received SDP session from ANNOUNCE request: {sdp_session:#?}");

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
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let transports = request
		.typed_header::<rtsp_types::headers::Transports>()
		.map_err(|e| { log::error!("Failed to parse transport information from SETUP request: {}", e) })?
		.ok_or_else(|| log::error!("No transport information in SETUP request."))?;

	if let Some(transport) = (*transports).first() {
		match transport {
			// // This transport is to support `ffplay`, useful for debugging.
			// Transport::Rtp(transport) => {
			// 	let (rtp_port, rtcp_port) = transport.params.client_port
			// 		.ok_or_else(|| log::error!("No client_port in SETUP request."))?;
			// 	let rtcp_port = rtcp_port.ok_or_else(|| log::error!("No RTC port in SETUP request."))?;

			// 	log::info!("Setting up session with client port: {}-{}", rtp_port, rtcp_port);

			// 	let (local_rtp_port, local_rtcp_port) = session.lock().unwrap().setup(rtp_port, rtcp_port)?;

			// 	return Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			// 		.header(headers::CSEQ, cseq.to_string())
			// 		.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
			// 		.header(headers::TRANSPORT, format!(
			// 			"RTP/AVP/UDP;unicast;client_port={rtp_port}-{rtcp_port};server_port={local_rtp_port}-{local_rtcp_port}"
			// 		))
			// 		.build(Vec::new())
			// 	);
			// },
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
				let (stream_id, port) = match query.1.split("/").next() {
					Some("control") => ("control", 47999u16),
					Some("audio") => ("audio", 48000u16),
					Some("video") => ("video", 47998u16),
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
	session: Arc<Mutex<Session>>,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	tokio::task::spawn(async move {
		session.lock().await.run().await
	});

	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new()))
}

