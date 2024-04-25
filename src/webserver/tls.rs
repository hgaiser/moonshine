use std::{path::Path, pin::Pin};

use anyhow::{Context, Result};
use openssl::ssl::{Ssl, SslAcceptor, SslFiletype, SslMethod};
use tokio::net::TcpStream;
use tokio_openssl::SslStream;

pub struct TlsAcceptor {
	acceptor: SslAcceptor,
}

impl TlsAcceptor {
	pub fn from_config<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<Self> {
		let acceptor = load_tls_files(certificate, private_key)?;
		Ok(Self { acceptor })
	}

	pub async fn accept(&self, connection: TcpStream) -> Result<SslStream<TcpStream>> {
		let ssl = Ssl::new(self.acceptor.context()).context("Failed to initialize TLS session")?;

		let mut stream = tokio_openssl::SslStream::new(ssl, connection).context("Failed to create TLS stream")?;
		Pin::new(&mut stream).accept().await.context("TLS handshake failed")?;
		Ok(stream)
	}
}

fn load_tls_files<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<SslAcceptor> {
	let mut builder =
		SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).context("Failed to initialize SSL acceptor")?;
	builder
		.set_private_key_file(&private_key, SslFiletype::PEM)
		.with_context(|| format!("Failed to set private key file '{:?}'", private_key.as_ref()))?;
	builder
		.set_certificate_chain_file(&certificate)
		.with_context(|| format!("Failed to set certificate file '{:?}'", certificate.as_ref()))?;

	Ok(builder.build())
}
