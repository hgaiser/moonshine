use std::{net::{ToSocketAddrs, SocketAddr}, sync::{Arc, Mutex}};
use rtsp_types::{headers::{self, Transport}, Method};
use tokio::{net::TcpListener, io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt}};

mod session;
use session::Session;

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
	let session = Arc::new(Mutex::new(Session::new()?));
	loop {
		let mut buffer = [0u8; 1024];
		connection.read(&mut buffer).await
			.map_err(|e| log::error!("Failed to read from connection '{}': {}", address, e))?;

		let (message, consumed): (rtsp_types::Message<Vec<u8>>, _) = rtsp_types::Message::parse(&buffer)
			.map_err(|e| log::error!("Failed to parse request as RTSP message: {}", e))?;

		log::trace!("Consumed {} bytes into RTSP request: {:#?}", consumed, message);

		let response = match message {
			rtsp_types::Message::Request(ref request) => {
				log::debug!("Received RTSP {:?} request", request.method());

				let cseq: i32 = request.header(&headers::CSEQ)
					.ok_or_else(|| log::error!("RTSP request has no CSeq header"))?
					.as_str()
					.parse()
					.map_err(|e| log::error!("Failed to parse CSeq header: {}", e))?;

				match request.method() {
					Method::Options => handle_options_request(request, cseq),
					Method::Describe => handle_describe_request(request, cseq, session.clone()),
					Method::Setup => handle_setup_request(request, cseq, session.clone()),
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
	}
}

fn handle_options_request(request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.header(headers::PUBLIC, "OPTIONS DESCRIBE SETUP PLAY")
		.build(Vec::new()))
}

fn handle_describe_request(
	request: &rtsp_types::Request<Vec<u8>>,
	cseq: i32,
	session: Arc<Mutex<Session>>,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let mut buffer = Vec::new();
	session.lock().unwrap().description()?.write(&mut buffer)
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
	session: Arc<Mutex<Session>>,
) -> Result<rtsp_types::Response<Vec<u8>>, ()> {
	let transports = request
		.typed_header::<rtsp_types::headers::Transports>()
		.map_err(|e| { log::error!("Failed to parse transport information from SETUP request: {}", e) })?
		.ok_or_else(|| log::error!("No transport information in SETUP request."))?;

	if let Some(transport) = (*transports).first() {
		match transport {
			Transport::Rtp(transport) => {
				let (rtp_port, rtcp_port) = transport.params.client_port
					.ok_or_else(|| log::error!("No client_port in SETUP request."))?;
				let rtcp_port = rtcp_port.ok_or_else(|| log::error!("No RTC port in SETUP request."))?;

				log::info!("Client port: {}-{}", rtp_port, rtcp_port);

				let (local_rtp_port, local_rtcp_port) = session.lock().unwrap().setup(rtp_port, rtcp_port)?;

				return Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
					.header(headers::CSEQ, cseq.to_string())
					.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
					.header(headers::TRANSPORT, format!(
						"RTP/AVP/UDP;unicast;client_port={rtp_port}-{rtcp_port};server_port={local_rtp_port}-{local_rtcp_port}"
					))
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
	tokio::spawn({
		let session = session.clone();
		async move {
			session.lock().unwrap().play().unwrap();
		}
	});

	Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new()))
}

