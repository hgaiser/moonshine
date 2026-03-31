use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::DistinguishedName;
use rustls::{DigitallySignedStruct, Error, ServerConfig, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::{server::TlsStream, TlsAcceptor as TlsAcceptorTokio};

/// A client certificate verifier that accepts any client certificate.
///
/// This allows the TLS layer to request and capture client certificates
/// without rejecting connections. Actual authorization (checking if the
/// certificate belongs to a paired client) happens at the application layer.
///
/// Client certificates are optional — clients without certificates (e.g.,
/// during pairing) can still connect.
struct AllowAnyClientCert {
	supported_algs: WebPkiSupportedAlgorithms,
	strict_verification: bool,
}

impl AllowAnyClientCert {
	fn new(strict_verification: bool) -> Self {
		let provider = CryptoProvider::get_default()
			.cloned()
			.unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
		Self {
			supported_algs: provider.signature_verification_algorithms,
			strict_verification,
		}
	}
}

impl fmt::Debug for AllowAnyClientCert {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("AllowAnyClientCert").finish()
	}
}

impl ClientCertVerifier for AllowAnyClientCert {
	fn offer_client_auth(&self) -> bool {
		true
	}

	fn client_auth_mandatory(&self) -> bool {
		false
	}

	fn root_hint_subjects(&self) -> &[DistinguishedName] {
		&[]
	}

	fn verify_client_cert(
		&self,
		_end_entity: &CertificateDer<'_>,
		_intermediates: &[CertificateDer<'_>],
		_now: UnixTime,
	) -> Result<ClientCertVerified, Error> {
		// Accept any certificate — authorization is checked at the application layer.
		Ok(ClientCertVerified::assertion())
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, Error> {
		if self.strict_verification {
			verify_tls12_signature(message, cert, dss, &self.supported_algs)
		} else {
			// Skip signature verification for compatibility with X.509 v2 certificates
			Ok(HandshakeSignatureValid::assertion())
		}
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, Error> {
		if self.strict_verification {
			verify_tls13_signature(message, cert, dss, &self.supported_algs)
		} else {
			// Skip signature verification for compatibility with X.509 v2 certificates
			Ok(HandshakeSignatureValid::assertion())
		}
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.supported_algs.supported_schemes()
	}
}

pub struct TlsAcceptor {
	acceptor: TlsAcceptorTokio,
}

impl TlsAcceptor {
	pub fn from_config<P: AsRef<Path>>(certificate: P, private_key: P, strict_verification: bool) -> Result<Self, ()> {
		let config = load_tls_files(certificate, private_key, strict_verification)?;
		let acceptor = TlsAcceptorTokio::from(Arc::new(config));
		Ok(Self { acceptor })
	}

	pub async fn accept(&self, connection: TcpStream) -> Result<TlsStream<TcpStream>, ()> {
		let stream = self
			.acceptor
			.accept(connection)
			.await
			.map_err(|e| tracing::warn!("TLS handshake failed: {}", e))?;
		Ok(stream)
	}
}

fn load_tls_files<P: AsRef<Path>>(
	certificate: P,
	private_key: P,
	strict_verification: bool,
) -> Result<ServerConfig, ()> {
	let certs = load_certs(certificate.as_ref())?;
	let key = load_private_key(private_key.as_ref())?;

	let config = ServerConfig::builder()
		.with_client_cert_verifier(Arc::new(AllowAnyClientCert::new(strict_verification)))
		.with_single_cert(certs, key)
		.map_err(|e| tracing::error!("Failed to create TLS configuration: {}", e))?;

	Ok(config)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, ()> {
	let mut reader =
		BufReader::new(File::open(path).map_err(|e| tracing::error!("Failed to open certificate file: {}", e))?);
	rustls_pemfile::certs(&mut reader)
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| tracing::error!("Failed to load certificate: {}", e))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, ()> {
	let mut reader =
		BufReader::new(File::open(path).map_err(|e| tracing::error!("Failed to open private key file: {}", e))?);
	rustls_pemfile::private_key(&mut reader)
		.map_err(|e| tracing::error!("Failed to load private key: {}", e))?
		.ok_or_else(|| tracing::error!("No private key found in file"))
}
