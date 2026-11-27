use std::fmt;
use std::fs::File;
use std::io::{BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rcgen::{
	BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose, SerialNumber,
};
use rsa::rand_core::RngCore;
use rsa::{
	RsaPrivateKey,
	pkcs8::{EncodePrivateKey, LineEnding},
};

use crate::config::Config;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, Error, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor as TlsAcceptorTokio, server::TlsStream};
use tracing::Level;
use x509_parser::prelude::*;

use aws_lc_rs::signature::{
	RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA384, RSA_PKCS1_2048_8192_SHA512, RSA_PSS_2048_8192_SHA256,
	RSA_PSS_2048_8192_SHA384, RSA_PSS_2048_8192_SHA512, UnparsedPublicKey,
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
				"Accepted client certificate: version={}, subject={}, fingerprint={}",
				version,
				subject,
				hex::encode(fingerprint)
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
		match self.acceptor.accept(connection).await {
			Ok(stream) => Ok(stream),
			Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
				tracing::debug!("TLS handshake aborted by peer: {}", e);
				Err(())
			},
			Err(e) => {
				tracing::warn!("TLS handshake failed: {}", e);
				Err(())
			},
		}
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

/// The TLS configuration of the control panel: the certificate in [ui], or else a self-signed
/// certificate of its own (see [`load_or_create_page_certificate`]). The control panel asks for no
/// client certificate.
///
/// `names` are the host names the control panel answers to.
pub(crate) fn control_panel_tls_config(config: &Config, names: &[String]) -> Result<ServerConfig, ()> {
	let (certificate, private_key) = match (&config.ui.certificate, &config.ui.private_key) {
		(Some(certificate), Some(private_key)) => (certificate.clone(), private_key.clone()),
		(None, None) => load_or_create_page_certificate(config, names)?,
		_ => {
			tracing::error!("Set both `certificate` and `private_key` in the [ui] table, or neither.");
			return Err(());
		},
	};
	let certs = load_certs(&certificate)?;
	let key = load_private_key(&private_key)?;
	let mut tls = ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(certs, key)
		.map_err(|e| tracing::error!("Failed to create the control panel's TLS configuration: {e}"))?;
	tls.alpn_protocols = vec![b"http/1.1".to_vec()];
	Ok(tls)
}

/// How long the control panel's own certificate is valid. Browsers refuse longer validity for
/// web pages, also for a certificate the user accepted.
const PAGE_CERTIFICATE_DAYS: u64 = 397;

/// Make a new page certificate when the current one expires within this many days.
const PAGE_CERTIFICATE_RENEW_DAYS: u64 = 30;

/// The control panel's self-signed certificate, next to the GameStream certificate. It is not the
/// GameStream certificate: that one is a long-lived RSA CA certificate without names, which Safari
/// and Firefox refuse for a web page, and pairing depends on it, so it must not change.
///
/// The file stays as long as it is valid and holds all of `names` and this computer's addresses,
/// so the browser keeps the user's exception. Returns the certificate and key paths.
fn load_or_create_page_certificate(config: &Config, names: &[String]) -> Result<(PathBuf, PathBuf), ()> {
	let directory = config
		.webserver
		.certificate
		.parent()
		.map(Path::to_path_buf)
		.unwrap_or_else(|| PathBuf::from("."));
	let certificate_path = directory.join("ui-cert.pem");
	let key_path = directory.join("ui-key.pem");

	// The names and IPv4 addresses must be in the certificate. IPv6 addresses go into a new
	// certificate too, but a missing one does not make a new certificate: privacy addresses change
	// every day, and each new certificate means a new browser warning.
	let mut required: Vec<String> = vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()];
	required.extend(names.iter().cloned());
	let mut ipv6 = Vec::new();
	if let Ok(interfaces) = if_addrs::get_if_addrs() {
		for ip in interfaces.iter().map(|interface| interface.ip()) {
			match ip {
				std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => required.push(v4.to_string()),
				std::net::IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unicast_link_local() => {
					ipv6.push(v6.to_string())
				},
				_ => {},
			}
		}
	}
	required.sort();
	required.dedup();

	if page_certificate_is_current(&certificate_path, &key_path, &required) {
		return Ok((certificate_path, key_path));
	}
	let mut wanted = required;
	wanted.extend(ipv6);
	wanted.dedup();

	tracing::info!("Creating a certificate for the control panel.");
	let key_pair =
		KeyPair::generate().map_err(|e| tracing::error!("Failed to create a key for the control panel: {e}"))?;
	let mut params = CertificateParams::new(wanted)
		.map_err(|e| tracing::error!("Failed to create the control panel certificate: {e}"))?;
	params.distinguished_name.push(
		rcgen::DnType::CommonName,
		format!("Moonshine control panel ({})", config.name),
	);
	params.is_ca = rcgen::IsCa::ExplicitNoCa;
	params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
	params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
	let now = SystemTime::now();
	params.not_before = (now - Duration::from_secs(60 * 60)).into();
	params.not_after = (now + Duration::from_secs(PAGE_CERTIFICATE_DAYS * 24 * 60 * 60)).into();
	let certificate = params
		.self_signed(&key_pair)
		.map_err(|e| tracing::error!("Failed to sign the control panel certificate: {e}"))?;

	std::fs::create_dir_all(&directory)
		.map_err(|e| tracing::error!("Failed to create {}: {e}", directory.display()))?;
	std::fs::write(&certificate_path, certificate.pem())
		.map_err(|e| tracing::error!("Failed to write {}: {e}", certificate_path.display()))?;
	write_private_file(&key_path, key_pair.serialize_pem().as_bytes())?;
	Ok((certificate_path, key_path))
}

/// Whether the page certificate exists, has a key, stays valid for a while and names everything in
/// `wanted`.
fn page_certificate_is_current(certificate_path: &Path, key_path: &Path, wanted: &[String]) -> bool {
	if !key_path.exists() {
		return false;
	}
	let Ok(certs) = load_certs_quiet(certificate_path) else {
		return false;
	};
	let Some(der) = certs.first() else {
		return false;
	};
	let Ok((_, certificate)) = X509Certificate::from_der(der) else {
		return false;
	};
	let renew_at = SystemTime::now() + Duration::from_secs(PAGE_CERTIFICATE_RENEW_DAYS * 24 * 60 * 60);
	let not_after = certificate.validity().not_after.timestamp();
	let renew_at = renew_at
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(i64::MAX);
	if not_after < renew_at {
		return false;
	}
	let mut names: Vec<String> = Vec::new();
	if let Ok(Some(san)) = certificate.subject_alternative_name() {
		for name in &san.value.general_names {
			match name {
				GeneralName::DNSName(dns) => names.push(dns.to_string()),
				GeneralName::IPAddress(bytes) => match bytes.len() {
					4 => names.push(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(*bytes).unwrap()).to_string()),
					16 => names.push(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(*bytes).unwrap()).to_string()),
					_ => {},
				},
				_ => {},
			}
		}
	}
	wanted
		.iter()
		.all(|name| names.iter().any(|have| have.eq_ignore_ascii_case(name)))
}

fn load_certs_quiet(path: &Path) -> Result<Vec<CertificateDer<'static>>, ()> {
	let mut reader = BufReader::new(File::open(path).map_err(|_| ())?);
	rustls_pemfile::certs(&mut reader)
		.collect::<Result<Vec<_>, _>>()
		.map_err(|_| ())
}

/// Write a file that only the user can read.
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), ()> {
	use std::os::unix::fs::OpenOptionsExt;
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create(true)
		.truncate(true)
		.mode(0o600)
		.open(path)
		.map_err(|e| tracing::error!("Failed to create {}: {e}", path.display()))?;
	file.write_all(contents)
		.map_err(|e| tracing::error!("Failed to write {}: {e}", path.display()))
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
	// rsa depends on rand_core 0.6, so we use its bundled OsRng
	let mut rng = rsa::rand_core::OsRng;
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
