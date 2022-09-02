use std::net::{ToSocketAddrs, SocketAddr};

use rtsp_types::{Method, headers, Response, Empty};
use tokio::{net::TcpListener, io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt}};

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

async fn handle_request(request: &rtsp_types::Request<Vec<u8>>) -> Result<Response<Empty>, ()> {
	log::debug!("Received RTSP {:?} request", request.method());

	let cseq = request.header(&headers::CSEQ)
		.ok_or_else(|| log::error!("RTSP request has no CSeq header"))?;

	match request.method() {
		Method::Options => {
			Ok(rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
				.header(headers::CSEQ, cseq.clone())
				.header(headers::PUBLIC, "DESCRIBE")
				.empty()
			)
		},
		method => {
			log::error!("Received request with unsupported method {:?}", method);
			Err(())
		}
	}
}
