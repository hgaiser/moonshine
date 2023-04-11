use std::{path::Path, pin::Pin};

use openssl::ssl::{SslMethod, SslFiletype, SslContext, SslAcceptor, Ssl};
use tokio::net::TcpStream;
use tokio_openssl::SslStream;

pub struct TlsAcceptor {
	context: SslContext,
}

impl TlsAcceptor {
	pub fn from_config<P: AsRef<Path>>(certificate_chain: P, private_key: P) -> Result<Self, ()> {
		let context = load_tls_files(certificate_chain, private_key)?;
		Ok(Self { context })
	}

	pub async fn accept(&self, connection: TcpStream) -> Result<SslStream<TcpStream>, ()> {
		let ssl = Ssl::new(&self.context)
			.map_err(|e| log::error!("Failed to initialize TLS session: {}", e))?;
		let mut stream = tokio_openssl::SslStream::new(ssl, connection)
			.map_err(|e| log::error!("Failed to create TLS stream: {}", e))?;
		Pin::new(&mut stream).accept()
			.await
			.map_err(|e| log::error!("TLS handshake failed: {}", e))?;
		Ok(stream)
	}
}

fn load_tls_files<P: AsRef<Path>>(certificate_chain: P, private_key: P) -> Result<SslContext, ()> {
	let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server())
		.map_err(|e| log::error!("Failed to initialize SSL acceptor: {}", e))?;
	builder
		.set_private_key_file(&private_key, SslFiletype::PEM)
		.map_err(|e| log::error!("Failed to set private key file '{:?}': {}", private_key.as_ref(), e))?;
	builder
		.set_certificate_chain_file(&certificate_chain)
		.map_err(|e| log::error!("Failed to set certificate chain file '{:?}': {}", certificate_chain.as_ref(), e))?;

	Ok(builder.build().into_context())
}
