use std::io::Write;

use async_shutdown::ShutdownManager;
use clients::ClientManager;
use config::Config;
use openssl::{
	asn1::Asn1Time,
	bn::{BigNum, MsbOption},
	error::ErrorStack,
	hash::MessageDigest,
	pkey::{PKey, Private},
	rsa::Rsa,
	x509::{extension::{BasicConstraints, KeyUsage, SubjectKeyIdentifier}, X509}
};
use rtsp::RtspServer;
use session::SessionManager;
use state::State;
use webserver::Webserver;

pub mod app_scanner;
pub mod clients;
pub mod config;
pub mod crypto;
pub mod rtsp;
pub mod session;
pub mod state;
pub mod publisher;
pub mod webserver;

fn create_certificate() -> Result<(X509, PKey<Private>), ErrorStack> {
	let rsa = Rsa::generate(2048)?;
	let key_pair = PKey::from_rsa(rsa)?;

	let mut cert_builder = X509::builder()?;
	cert_builder.set_version(2)?;
	let serial_number = {
		let mut serial = BigNum::new()?;
		serial.rand(159, MsbOption::MAYBE_ZERO, false)?;
		serial.to_asn1_integer()?
	};
	cert_builder.set_serial_number(&serial_number)?;
	cert_builder.set_pubkey(&key_pair)?;
	let not_before = Asn1Time::days_from_now(0)?;
	cert_builder.set_not_before(&not_before)?;
	let not_after = Asn1Time::days_from_now(3650)?;
	cert_builder.set_not_after(&not_after)?;

	cert_builder.append_extension(BasicConstraints::new().critical().ca().build()?)?;
	cert_builder.append_extension(
		KeyUsage::new()
			.critical()
			.key_cert_sign()
			.crl_sign()
			.build()?,
	)?;

	let subject_key_identifier =
		SubjectKeyIdentifier::new().build(&cert_builder.x509v3_context(None, None))?;
	cert_builder.append_extension(subject_key_identifier)?;

	cert_builder.sign(&key_pair, MessageDigest::sha256())?;
	let cert = cert_builder.build();

	Ok((cert, key_pair))
}

pub struct Moonshine {
	_rtsp_server: RtspServer,
	_session_manager: SessionManager,
	_client_manager: ClientManager,
	_webserver: Webserver,
}

impl Moonshine {
	pub async fn new(
		config: Config,
		shutdown: ShutdownManager<i32>,
	) -> Result<Self, ()> {
		let state = State::new().await?;

		let (cert, pkey) = if !config.webserver.certificate.exists() && !config.webserver.private_key.exists() {
			log::info!("No certificate found, creating a new one.");

			let (cert, pkey) = create_certificate()
				.map_err(|e| log::error!("Failed to create certificate: {e}"))?;

			// Write certificate to file
			let mut certfile = std::fs::File::create(&config.webserver.certificate).unwrap();
			certfile.write(&cert.to_pem().map_err(|e| log::error!("Failed to serialize PEM: {e}"))?)
				.map_err(|e| log::error!("Failed to write PEM to file: {e}"))?;

			// Write private key to file
			let mut keyfile = std::fs::File::create(&config.webserver.private_key).unwrap();
			keyfile.write(&pkey.private_key_to_pem_pkcs8().map_err(|e| log::error!("Failed to serialize private key: {e}"))?)
				.map_err(|e| log::error!("Failed to write private key to file: {e}"))?;

			log::debug!("Saved private key to {}", config.webserver.private_key.display());
			log::debug!("Saved certificate to {}", config.webserver.certificate.display());

			(cert, pkey)
		} else {
			let cert = std::fs::read(&config.webserver.certificate)
				.map_err(|e| log::error!("Failed to read server certificate: {e}"))?;
			let cert = openssl::x509::X509::from_pem(&cert)
				.map_err(|e| log::error!("Failed to parse server certificate: {e}"))?;
			let pkey = PKey::private_key_from_pem(&std::fs::read(&config.webserver.private_key)
				.map_err(|e| log::error!("Failed to read private key: {e}"))?)
				.map_err(|e| log::error!("Failed to parse private key: {e}"))?;

			(cert, pkey)
		};

		// Create a manager for interacting with sessions.
		let session_manager = SessionManager::new(config.clone(), shutdown.trigger_shutdown_token(2))?;

		// Create a manager for saving and loading client state.
		let client_manager = ClientManager::new(state.clone(), cert.clone(), pkey, shutdown.trigger_shutdown_token(3));

		// Run the RTSP server.
		let rtsp_server = RtspServer::new(config.clone(), session_manager.clone(), shutdown.clone());

		// Publish the Moonshine service using zeroconf.
		publisher::spawn(config.webserver.port, config.name.clone(), shutdown.clone());

		// Create a handler for the webserver.
		let webserver = Webserver::new(
			config,
			state.get_uuid().await?,
			cert,
			client_manager.clone(),
			session_manager.clone(),
			shutdown,
		)?;

		Ok(Self {
			_rtsp_server: rtsp_server,
			_session_manager: session_manager,
			_client_manager: client_manager,
			_webserver: webserver,
		})
	}
}
