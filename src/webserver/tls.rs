use std::{path::Path, pin::Pin};

use openssl::ssl::{SslMethod, SslFiletype, SslAcceptor, Ssl};
use tokio::net::TcpStream;
use tokio_openssl::SslStream;

pub struct TlsAcceptor {
	acceptor: SslAcceptor,
}

impl TlsAcceptor {
	pub fn from_config<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<Self, ()> {
		let acceptor = load_tls_files(certificate, private_key)?;
		Ok(Self { acceptor })
	}

	pub async fn accept(&self, connection: TcpStream) -> Result<SslStream<TcpStream>, ()> {
		let ssl = Ssl::new(self.acceptor.context())
			.map_err(|e| log::error!("Failed to initialize TLS session: {}", e))?;

		let mut stream = tokio_openssl::SslStream::new(ssl, connection)
			.map_err(|e| log::error!("Failed to create TLS stream: {}", e))?;
		Pin::new(&mut stream).accept()
			.await
			.map_err(|e| log::error!("TLS handshake failed: {}", e))?;
		Ok(stream)
	}
}

fn load_tls_files<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<SslAcceptor, ()> {
	let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server())
		.map_err(|e| log::error!("Failed to initialize SSL acceptor: {}", e))?;
	builder
		.set_private_key_file(&private_key, SslFiletype::PEM)
		.map_err(|e| log::error!("Failed to set private key file '{:?}': {}", private_key.as_ref(), e))?;
	builder
		.set_certificate_chain_file(&certificate)
		.map_err(|e| log::error!("Failed to set certificate file '{:?}': {}", certificate.as_ref(), e))?;

	Ok(builder.build())
}
