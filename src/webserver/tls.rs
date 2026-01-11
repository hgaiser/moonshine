use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::net::TcpStream;
use tokio_rustls::{server::TlsStream, TlsAcceptor as TlsAcceptorTokio};

pub struct TlsAcceptor {
	acceptor: TlsAcceptorTokio,
}

impl TlsAcceptor {
	pub fn from_config<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<Self, ()> {
		let config = load_tls_files(certificate, private_key)?;
		let acceptor = TlsAcceptorTokio::from(Arc::new(config));
		Ok(Self { acceptor })
	}

	pub async fn accept(&self, connection: TcpStream) -> Result<TlsStream<TcpStream>, ()> {
		let stream = self
			.acceptor
			.accept(connection)
			.await
			.map_err(|e| tracing::error!("TLS handshake failed: {}", e))?;
		Ok(stream)
	}
}

fn load_tls_files<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<ServerConfig, ()> {
    let certs = load_certs(certificate.as_ref())?;
    let key = load_private_key(private_key.as_ref())?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| tracing::error!("Failed to create TLS configuration: {}", e))?;
    
    Ok(config)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, ()> {
    let mut reader = BufReader::new(File::open(path).map_err(|e| tracing::error!("Failed to open certificate file: {}", e))?);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tracing::error!("Failed to load certificate: {}", e))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, ()> {
    let mut reader = BufReader::new(File::open(path).map_err(|e| tracing::error!("Failed to open private key file: {}", e))?);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| tracing::error!("Failed to load private key: {}", e))?
        .ok_or_else(|| tracing::error!("No private key found in file"))
}
