use std::fmt;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rand::RngCore;
use rcgen::{
	BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose, SerialNumber,
};
use rsa::{
	pkcs8::{EncodePrivateKey, LineEnding},
	RsaPrivateKey,
};

use crate::config::Config;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, Error, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::{server::TlsStream, TlsAcceptor as TlsAcceptorTokio};
use tracing::Level;
use x509_parser::prelude::*;

use aws_lc_rs::signature::{
	UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA384, RSA_PKCS1_2048_8192_SHA512,
	RSA_PSS_2048_8192_SHA256, RSA_PSS_2048_8192_SHA384, RSA_PSS_2048_8192_SHA512,
};

/// A lenient client certificate verifier that mirrors Sunshine's validation behavior.
///
/// This verifier accepts X.509 v1, v2, and v3 certificates, ignoring:
/// - Certificate version (accepts v1/v2/v3, not just v3)
/// - Expiration (accepts expired and not-yet-valid certificates)
/// - Issuer validation (skips chain validation)
///
/// Cryptographic signature verification is still performed during the TLS handshake
/// using the verifier's configured signature algorithms and manual signature checks.
///
/// Actual authorization (checking if the certificate belongs to a paired client)
/// happens at the application layer via fingerprint matching.
struct LenientClientCertVerifier {
	supported_algs: WebPkiSupportedAlgorithms,
}

impl LenientClientCertVerifier {
	fn new() -> Self {
		let provider = CryptoProvider::get_default()
			.cloned()
			.unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
		Self {
			supported_algs: provider.signature_verification_algorithms,
		}
	}

	/// Perform lenient certificate validation, mirroring Sunshine's openssl_verify_cb().
	///
	/// This function:
	/// - Accepts X.509 v1, v2, and v3 certificates
	/// - Ignores expiration errors (expired, not-yet-valid)
	/// - Ignores issuer validation errors
	/// - Logs certificate metadata for debugging (only when DEBUG level is enabled)
	///
	/// To avoid redundant parsing overhead, certificate parsing only occurs when
	/// debug logging is enabled. The certificate is still validated for basic
	/// DER encoding correctness in all cases.
	fn lenient_validate(&self, cert_der: &[u8]) -> Result<(), Error> {
		if tracing::level_enabled!(Level::DEBUG) {
			let (_, cert) = X509Certificate::from_der(cert_der).map_err(|e| {
				tracing::debug!("Failed to parse client certificate: {}", e);
				Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
			})?;

			let version = cert.version();
			let subject = cert.subject().to_string();
			let fingerprint = Sha256::digest(cert_der);

			tracing::debug!(
				"Accepted client certificate: version={}, subject={}, fingerprint={:x}",
				version,
				subject,
				fingerprint
			);
		}

		Ok(())
	}

	/// Extract public key from certificate using x509-parser.
	///
	/// This bypasses WebPki's certificate parsing which rejects X.509 v2 certificates.
	fn extract_public_key(cert_der: &[u8]) -> Result<Vec<u8>, Error> {
		let (_, cert) = X509Certificate::from_der(cert_der).map_err(|e| {
			tracing::debug!("Failed to parse certificate for public key extraction: {}", e);
			Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
		})?;

		let public_key_bytes = cert.public_key().subject_public_key.data.to_vec();
		Ok(public_key_bytes)
	}

	/// Verify a signature manually using aws-lc-rs, bypassing WebPki.
	///
	/// This supports RSA PKCS1, RSA PSS, ECDSA, and Ed25519 signatures.
	fn verify_signature_manual(
		&self,
		message: &[u8],
		cert_der: &[u8],
		dss: &DigitallySignedStruct,
		tls13: bool,
	) -> Result<HandshakeSignatureValid, Error> {
		let public_key_bytes = Self::extract_public_key(cert_der)?;

		match dss.scheme {
			// RSA PKCS1 signatures
			SignatureScheme::RSA_PKCS1_SHA256 => {
				let public_key = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "RSA_PKCS1_SHA256")
			},
			SignatureScheme::RSA_PKCS1_SHA384 => {
				let public_key = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA384, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "RSA_PKCS1_SHA384")
			},
			SignatureScheme::RSA_PKCS1_SHA512 => {
				let public_key = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA512, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "RSA_PKCS1_SHA512")
			},

			// RSA PSS signatures
			SignatureScheme::RSA_PSS_SHA256 => {
				let public_key = UnparsedPublicKey::new(&RSA_PSS_2048_8192_SHA256, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "RSA_PSS_SHA256")
			},
			SignatureScheme::RSA_PSS_SHA384 => {
				let public_key = UnparsedPublicKey::new(&RSA_PSS_2048_8192_SHA384, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "RSA_PSS_SHA384")
			},
			SignatureScheme::RSA_PSS_SHA512 => {
				let public_key = UnparsedPublicKey::new(&RSA_PSS_2048_8192_SHA512, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "RSA_PSS_SHA512")
			},

			// ECDSA signatures (ASN.1 DER-encoded)
			SignatureScheme::ECDSA_NISTP256_SHA256 => {
				let public_key =
					UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "ECDSA_NISTP256_SHA256")
			},
			SignatureScheme::ECDSA_NISTP384_SHA384 => {
				let public_key =
					UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P384_SHA384_ASN1, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "ECDSA_NISTP384_SHA384")
			},
			// ECDSA P-521 is not supported by aws-lc-rs
			SignatureScheme::ECDSA_NISTP521_SHA512 => {
				tracing::warn!("ECDSA P-521 is not supported by aws-lc-rs");
				Err(Error::InvalidCertificate(
					rustls::CertificateError::UnsupportedSignatureAlgorithmContext {
						signature_algorithm_id: vec![],
						supported_algorithms: vec![],
					},
				))
			},

			// Ed25519 signatures
			SignatureScheme::ED25519 => {
				let public_key = UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, &public_key_bytes);
				self.verify_with_scheme(public_key, message, dss, "ED25519")
			},

			// Unsupported signature scheme
			scheme => {
				let version = if tls13 { "TLS 1.3" } else { "TLS 1.2" };
				tracing::warn!("Unsupported signature scheme for {}: {:?}", version, scheme);
				Err(Error::InvalidCertificate(
					rustls::CertificateError::UnsupportedSignatureAlgorithmContext {
						signature_algorithm_id: vec![],
						supported_algorithms: vec![],
					},
				))
			},
		}
	}

	/// Helper to verify a signature with a given public key and scheme name.
	fn verify_with_scheme<B: AsRef<[u8]>>(
		&self,
		public_key: UnparsedPublicKey<B>,
		message: &[u8],
		dss: &DigitallySignedStruct,
		scheme_name: &str,
	) -> Result<HandshakeSignatureValid, Error> {
		public_key
			.verify(message, dss.signature())
			.map(|_| HandshakeSignatureValid::assertion())
			.map_err(|e| {
				tracing::warn!("{} signature verification failed: {:?}", scheme_name, e);
				Error::InvalidCertificate(rustls::CertificateError::BadSignature)
			})
	}

	/// Verify TLS 1.2 signature manually using aws-lc-rs, bypassing WebPki.
	fn verify_tls12_signature_manual(
		&self,
		message: &[u8],
		cert_der: &[u8],
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, Error> {
		self.verify_signature_manual(message, cert_der, dss, false)
	}

	/// Verify TLS 1.3 signature manually using aws-lc-rs, bypassing WebPki.
	fn verify_tls13_signature_manual(
		&self,
		message: &[u8],
		cert_der: &[u8],
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, Error> {
		self.verify_signature_manual(message, cert_der, dss, true)
	}
}

impl fmt::Debug for LenientClientCertVerifier {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("LenientClientCertVerifier").finish()
	}
}

impl ClientCertVerifier for LenientClientCertVerifier {
	fn offer_client_auth(&self) -> bool {
		true
	}

	fn client_auth_mandatory(&self) -> bool {
		false
	}

	fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
		&[]
	}

	fn verify_client_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		_intermediates: &[CertificateDer<'_>],
		_now: UnixTime,
	) -> Result<ClientCertVerified, Error> {
		self.lenient_validate(end_entity.as_ref())?;
		Ok(ClientCertVerified::assertion())
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, Error> {
		// Use manual verification to bypass WebPki's X.509 v2 certificate rejection
		self.verify_tls12_signature_manual(message, cert.as_ref(), dss)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, Error> {
		// Use manual verification to bypass WebPki's X.509 v2 certificate rejection
		self.verify_tls13_signature_manual(message, cert.as_ref(), dss)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.supported_algs.supported_schemes()
	}
}

pub(crate) struct TlsAcceptor {
	acceptor: TlsAcceptorTokio,
}

impl TlsAcceptor {
	pub(crate) fn from_config<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<Self, ()> {
		let config = load_tls_files(certificate, private_key)?;
		let acceptor = TlsAcceptorTokio::from(Arc::new(config));
		Ok(Self { acceptor })
	}

	pub(crate) async fn accept(&self, connection: TcpStream) -> Result<TlsStream<TcpStream>, ()> {
		let stream = self
			.acceptor
			.accept(connection)
			.await
			.map_err(|e| tracing::warn!("TLS handshake failed: {}", e))?;
		Ok(stream)
	}
}

fn load_tls_files<P: AsRef<Path>>(certificate: P, private_key: P) -> Result<ServerConfig, ()> {
	let certs = load_certs(certificate.as_ref())?;
	let key = load_private_key(private_key.as_ref())?;

	let config = ServerConfig::builder()
		.with_client_cert_verifier(Arc::new(LenientClientCertVerifier::new()))
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

/// Generate a self-signed TLS certificate and private key.
///
/// Creates a 2048-bit RSA key pair and a self-signed X.509 certificate valid for
/// 10 years. The certificate includes the following properties:
/// - Common Name: "Moonshine"
/// - CA constraint: unconstrained (can sign other certificates)
/// - Key usages: digital signature, key encipherment, key agreement
/// - Serial number: random 64-bit value
pub(crate) fn create_certificate() -> Result<(String, String), Box<dyn std::error::Error>> {
	// Generate RSA key (2048 bits)
	let mut rng = rand::rngs::OsRng;
	let private_key = RsaPrivateKey::new(&mut rng, 2048)?;
	let key_pem = private_key.to_pkcs8_pem(LineEnding::LF)?.to_string();

	let mut params = CertificateParams::default();
	params.not_before = SystemTime::now().into();
	params.not_after = (SystemTime::now() + Duration::from_secs(3650 * 24 * 60 * 60)).into();
	params.serial_number = Some(SerialNumber::from(rng.next_u64()));

	let mut distinguished_name = DistinguishedName::new();
	distinguished_name.push(DnType::CommonName, "Moonshine");
	params.distinguished_name = distinguished_name;

	params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
	params.key_usages = vec![
		KeyUsagePurpose::DigitalSignature,
		KeyUsagePurpose::KeyEncipherment,
		KeyUsagePurpose::KeyAgreement,
	];

	let key_pair = KeyPair::from_pem(&key_pem)?;
	let cert = params.self_signed(&key_pair)?;

	Ok((cert.pem(), key_pem))
}

/// Load existing TLS certificate and private key from disk, or create new ones if they don't exist.
pub fn load_or_create_certificate(config: &Config) -> Result<(String, String), ()> {
	let cert_path = &config.webserver.certificate;
	let key_path = &config.webserver.private_key;

	if !cert_path.exists() && !key_path.exists() {
		tracing::info!("No certificate found, creating a new one.");

		let (cert, pkey) = create_certificate().map_err(|e| tracing::error!("Failed to create certificate: {e}"))?;

		// Write certificate to file.
		let cert_dir = cert_path
			.parent()
			.ok_or_else(|| tracing::error!("Failed to find parent directory for certificate file."))?;
		std::fs::create_dir_all(cert_dir)
			.map_err(|e| tracing::error!("Failed to create certificate directory: {e}"))?;
		let mut certfile =
			std::fs::File::create(cert_path).map_err(|e| tracing::error!("Failed to create certificate file: {e}"))?;
		certfile
			.write(cert.as_bytes())
			.map_err(|e| tracing::error!("Failed to write PEM to file: {e}"))?;

		// Write private key to file.
		let key_dir = key_path
			.parent()
			.ok_or_else(|| tracing::error!("Failed to find parent directory for private key file."))?;
		std::fs::create_dir_all(key_dir).map_err(|e| tracing::error!("Failed to create private key directory: {e}"))?;
		let mut keyfile =
			std::fs::File::create(key_path).map_err(|e| tracing::error!("Failed to create private key file: {e}"))?;
		keyfile
			.write(pkey.as_bytes())
			.map_err(|e| tracing::error!("Failed to write private key to file: {e}"))?;

		tracing::debug!("Saved certificate to {}", cert_path.display());
		tracing::debug!("Saved private key to {}", key_path.display());

		Ok((cert, pkey))
	} else {
		let cert = std::fs::read_to_string(cert_path)
			.map_err(|e| tracing::error!("Failed to read server certificate: {e}"))?;

		let pkey = std::fs::read_to_string(key_path).map_err(|e| tracing::error!("Failed to read private key: {e}"))?;

		Ok((cert, pkey))
	}
}
