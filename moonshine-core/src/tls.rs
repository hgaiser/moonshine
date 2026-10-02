use std::fmt;
use std::fs::File;
use std::io::{BufReader, ErrorKind};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
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
	let identity = initialize_identity(&config.webserver.certificate, &config.webserver.private_key)
		.map_err(|e| tracing::error!("Failed to load or provision TLS identity: {e}"))?;
	if std::fs::metadata(&config.webserver.private_key).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
		tracing::warn!(
			"Existing TLS private key permits group/other access; ask its administrator to restrict permissions to 0600"
		);
	}
	Ok(identity)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct IdentityJournal {
	certificate: String,
	private_key: String,
}

fn identity_sidecar(key: &Path, suffix: &str) -> std::path::PathBuf {
	let mut name = key.as_os_str().to_os_string();
	name.push(suffix);
	name.into()
}

fn initialize_identity(cert: &Path, key: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
	if cert == key {
		return Err("Certificate and key paths must differ".into());
	}
	let journal_path = identity_sidecar(key, ".creation.toml");
	if cert.exists() && key.exists() && !journal_path.exists() {
		load_tls_files(cert, key).map_err(|_| "Invalid existing identity")?;
		return Ok((std::fs::read_to_string(cert)?, std::fs::read_to_string(key)?));
	}
	crate::durable::create_directories(crate::durable::directory(key))?;
	// Kernel lock survives thread races and releases automatically on process death.
	// Do not unlink this file: replacing its inode would split the lock domain.
	let lock_path = identity_sidecar(key, ".creation.lock");
	let lock = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.mode(0o600)
		.custom_flags(libc::O_NOFOLLOW)
		.open(lock_path)?;
	lock.lock()?;
	let exists = |path: &Path| -> std::io::Result<bool> {
		match std::fs::symlink_metadata(path) {
			Ok(_) => Ok(true),
			Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
			Err(e) => Err(e),
		}
	};
	if exists(cert)? && exists(key)? {
		// Validate the pair without changing externally managed permissions/content.
		load_tls_files(cert, key).map_err(|_| "Invalid existing identity")?;
		let certificate = std::fs::read_to_string(cert)?;
		let private_key = std::fs::read_to_string(key)?;
		if exists(&journal_path)? && !std::fs::symlink_metadata(&journal_path)?.file_type().is_symlink() {
			let journal: IdentityJournal = toml::from_str(&std::fs::read_to_string(&journal_path)?)
				.map_err(|_| "Invalid identity recovery journal")?;
			if journal.certificate == certificate && journal.private_key == private_key {
				crate::durable::sync_parent(key)?;
				crate::durable::sync_parent(cert)?;
				std::fs::remove_file(&journal_path)?;
				crate::durable::sync_parent(&journal_path)?;
			}
		}
		return Ok((certificate, private_key));
	}
	let journal: IdentityJournal = if exists(&journal_path)? {
		// Never follow a symlink containing an administrator's unrelated secret.
		if std::fs::symlink_metadata(&journal_path)?.file_type().is_symlink() {
			return Err("Identity recovery journal is a symlink".into());
		}
		toml::from_str(&std::fs::read_to_string(&journal_path)?).map_err(|_| "Invalid identity recovery journal")?
	} else {
		if exists(cert)? || exists(key)? {
			return Err("Incomplete existing TLS identity; restore its matching file (no recovery journal)".into());
		}
		let (certificate, private_key) = create_certificate()?;
		let journal = IdentityJournal {
			certificate,
			private_key,
		};
		crate::durable::create(&journal_path, toml::to_string(&journal)?.as_bytes())?;
		journal
	};
	// Both files come from a durable journal. Recovery may complete missing files,
	// but cannot overwrite any existing file, including symlinks or partial files.
	for (path, content) in [(key, &journal.private_key), (cert, &journal.certificate)] {
		if exists(path)? {
			if std::fs::read_to_string(path)? != *content {
				return Err("Existing identity conflicts with recovery journal; restore manually".into());
			}
		} else {
			crate::durable::create(path, content.as_bytes())?;
		}
	}
	load_tls_files(cert, key).map_err(|_| "Invalid provisioned identity")?;
	std::fs::remove_file(&journal_path)?;
	crate::durable::sync_parent(&journal_path)?;
	Ok((journal.certificate, journal.private_key))
}

#[cfg(test)]
mod identity_tests {
	use super::*;

	#[test]
	fn interrupted_creation_child_probe() {
		let Some(directory) = std::env::var_os("PYROSHINE_IDENTITY_CRASH_DIR") else {
			return;
		};
		let directory = std::path::PathBuf::from(directory);
		let count = std::env::var("PYROSHINE_IDENTITY_CRASH_AFTER")
			.unwrap()
			.parse()
			.unwrap();
		crate::durable::fail_after("create", count, 0);
		let _ = initialize_identity(&directory.join("server.pem"), &directory.join("server.key"));
		panic!("child did not reach interruption point");
	}

	#[test]
	fn process_exit_during_creation_recovers_and_releases_writer_lock() {
		for count in [0, 1, 2] {
			let directory = tempfile::tempdir().unwrap();
			let result = std::process::Command::new(std::env::current_exe().unwrap())
				.args(["--exact", "tls::identity_tests::interrupted_creation_child_probe"])
				.env("PYROSHINE_IDENTITY_CRASH_DIR", directory.path())
				.env("PYROSHINE_IDENTITY_CRASH_AFTER", count.to_string())
				.output()
				.unwrap();
			assert_eq!(result.status.code(), Some(91));
			let key = directory.path().join("server.key");
			let before = std::fs::read(&key).ok();
			assert!(initialize_identity(&directory.path().join("server.pem"), &key).is_ok());
			if let Some(before) = before {
				assert!(std::fs::read(&key).unwrap() == before);
			}
			for entry in std::fs::read_dir(directory.path()).unwrap() {
				assert_eq!(entry.unwrap().metadata().unwrap().permissions().mode() & 0o777, 0o600);
			}
		}
	}

	#[test]
	fn umask_child_probe() {
		let Some(directory) = std::env::var_os("PYROSHINE_KEY_TEST_DIR") else {
			return;
		};
		let mask = u32::from_str_radix(&std::env::var("PYROSHINE_KEY_TEST_UMASK").unwrap(), 8).unwrap();
		// This test runs alone in a child process; umask cannot race other tests.
		unsafe {
			libc::umask(mask);
		}
		let directory = std::path::PathBuf::from(directory);
		let cert = directory.join("custom-cert/identity.pem");
		let key = directory.join("custom-key/identity.key");
		assert!(initialize_identity(&cert, &key).is_ok());
		assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
		let before = std::fs::read(&key).unwrap();
		assert!(initialize_identity(&cert, &key).is_ok());
		assert!(std::fs::read(&key).unwrap() == before);
	}

	#[test]
	fn private_keys_ignore_permissive_umask() {
		for mask in ["022", "000", "077"] {
			let directory = tempfile::tempdir().unwrap();
			let result = std::process::Command::new(std::env::current_exe().unwrap())
				.args(["--exact", "tls::identity_tests::umask_child_probe"])
				.env("PYROSHINE_KEY_TEST_DIR", directory.path())
				.env("PYROSHINE_KEY_TEST_UMASK", mask)
				.output()
				.unwrap();
			assert!(result.status.success(), "isolated umask test failed for {mask}");
		}
	}

	#[test]
	fn concurrent_creators_preserve_one_valid_identity() {
		let directory = tempfile::tempdir().unwrap();
		let cert = directory.path().join("server.pem");
		let key = directory.path().join("server.key");
		std::thread::scope(|scope| {
			let mut threads = Vec::new();
			for _ in 0..8 {
				let (cert, key) = (&cert, &key);
				threads.push(scope.spawn(move || initialize_identity(cert, key).is_ok()));
			}
			for thread in threads {
				assert!(thread.join().unwrap());
			}
		});
		let original = std::fs::read(&key).unwrap();
		assert!(initialize_identity(&cert, &key).is_ok());
		assert!(std::fs::read(&key).unwrap() == original);
		assert!(load_tls_files(&cert, &key).is_ok());
		assert!(!identity_sidecar(&key, ".creation.toml").exists());
	}

	#[test]
	fn interruption_and_filesystem_failures_recover_without_rotating_keys() {
		let (certificate, private_key) = create_certificate().unwrap();
		for (operation, count, errno) in [
			("write", 0, libc::ENOSPC),
			("file_sync", 0, libc::EIO),
			("create", 0, libc::EACCES),
			("create", 1, libc::EACCES),
			("dir_sync", 1, libc::EIO),
			("dir_sync", 2, libc::EIO),
		] {
			let directory = tempfile::tempdir().unwrap();
			let cert = directory.path().join("server.pem");
			let key = directory.path().join("server.key");
			let journal = IdentityJournal {
				certificate: certificate.clone(),
				private_key: private_key.clone(),
			};
			// An interruption after the durable intent, before either publication.
			crate::durable::create(
				&identity_sidecar(&key, ".creation.toml"),
				toml::to_string(&journal).unwrap().as_bytes(),
			)
			.unwrap();
			crate::durable::fail_after(operation, count, errno);
			assert!(initialize_identity(&cert, &key).is_err());
			assert!(initialize_identity(&cert, &key).is_ok());
			assert!(std::fs::read_to_string(&key).unwrap() == private_key);
			assert!(std::fs::read_to_string(&cert).unwrap() == certificate);
			assert!(!identity_sidecar(&key, ".creation.toml").exists());
		}
	}

	#[test]
	fn incomplete_external_identity_and_symlinks_are_not_overwritten() {
		let directory = tempfile::tempdir().unwrap();
		let cert = directory.path().join("server.pem");
		let key = directory.path().join("server.key");
		let (certificate, private_key) = create_certificate().unwrap();
		std::fs::write(&key, &private_key).unwrap();
		std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
		assert!(initialize_identity(&cert, &key).is_err());
		assert!(std::fs::read_to_string(&key).unwrap() == private_key);
		std::fs::write(&cert, certificate).unwrap();
		assert!(initialize_identity(&cert, &key).is_ok());
		assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o640);
		std::fs::remove_file(&key).unwrap();
		std::fs::remove_file(&cert).unwrap();
		std::os::unix::fs::symlink(directory.path().join("missing"), &key).unwrap();
		assert!(initialize_identity(&cert, &key).is_err());
		assert!(std::fs::symlink_metadata(&key).unwrap().file_type().is_symlink());
	}
}
