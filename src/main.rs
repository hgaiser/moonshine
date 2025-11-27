use std::io::Write;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use crate::clients::ClientManager;
use crate::config::Config;
use crate::crypto::create_certificate;
use crate::rtsp::RtspServer;
use crate::session::SessionManager;
use crate::state::State;
use crate::webserver::Webserver;
use openssl::pkey::PKey;

mod app_scanner;
mod clients;
mod config;
mod crypto;
mod rtsp;
mod session;
mod state;
mod publisher;
mod webserver;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	let args = Args::parse();

	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer())
		.with(EnvFilter::from_default_env())
		.init();

	let mut config;
	if args.config.exists() {
		config = Config::read_from_file(args.config).map_err(|_| std::process::exit(1))?;
	} else {
		tracing::info!("No config file found at {}, creating a default config file.", args.config.display());
		config = Config::default();

		let serialized_config = toml::to_string_pretty(&config)
			.map_err(|e| tracing::error!("Failed to serialize config: {e}"))?;

		let config_dir = args.config.parent()
			.ok_or_else(|| tracing::error!("Failed to get parent directory of config file."))?;
		std::fs::create_dir_all(config_dir)
			.map_err(|e| tracing::error!("Failed to create config directory: {e}"))?;
		std::fs::write(args.config, serialized_config)
			.map_err(|e| tracing::error!("Failed to save config file: {e}"))?;
	}

	// Resolve these paths so that the rest of the code doesn't need to.
	let cert_path = config.webserver.certificate.to_string_lossy().to_string();
	let cert_path = shellexpand::full(&cert_path)
		.map_err(|e| tracing::error!("Failed to expand certificate path: {e}"))?;
	config.webserver.certificate = cert_path.to_string().into();

	let private_key_path = config.webserver.private_key.to_string_lossy().to_string();
	let private_key_path = shellexpand::full(&private_key_path)
		.map_err(|e| tracing::error!("Failed to expand private key path: {e}"))?;
	config.webserver.private_key = private_key_path.to_string().into();

	tracing::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);

	// Spawn a task to wait for CTRL+C and trigger a shutdown.
	let shutdown = ShutdownManager::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			if let Err(e) = tokio::signal::ctrl_c().await {
				tracing::error!("Failed to wait for CTRL+C: {e}");
				std::process::exit(1);
			}

			tracing::info!("Shutting down server...");
			shutdown.trigger_shutdown(1).ok();
		}
	});

	// Create the main application.
	let moonshine = Moonshine::new(config, shutdown.clone()).await?;

	tracing::info!("Moonshine is ready and waiting for connections.");

	// Wait until something causes a shutdown trigger.
	shutdown.wait_shutdown_triggered().await;

	// Drop the main moonshine object, triggering other systems to shutdown too.
	drop(moonshine);

	// Wait until everything was shutdown.
	let exit_code = shutdown.wait_shutdown_complete().await;
	tracing::trace!("Successfully waited for shutdown to complete.");
	std::process::exit(exit_code);
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
			tracing::info!("No certificate found, creating a new one.");

			let (cert, pkey) = create_certificate()
				.map_err(|e| tracing::error!("Failed to create certificate: {e}"))?;

			// Write certificate to file
			let cert_dir = config.webserver.certificate.parent()
				.ok_or_else(|| tracing::error!("Failed to find parent directory for certificate file."))?;
			std::fs::create_dir_all(cert_dir)
				.map_err(|e| tracing::error!("Failed to create certificate directory: {e}"))?;
			let mut certfile = std::fs::File::create(&config.webserver.certificate).unwrap();
			certfile.write(&cert.to_pem().map_err(|e| tracing::error!("Failed to serialize PEM: {e}"))?)
				.map_err(|e| tracing::error!("Failed to write PEM to file: {e}"))?;

			// Write private key to file
			let private_key_dir = config.webserver.private_key.parent()
				.ok_or_else(|| tracing::error!("Failed to find parent directory for private key file."))?;
			std::fs::create_dir_all(private_key_dir)
				.map_err(|e| tracing::error!("Failed to create private key directory: {e}"))?;
			let mut keyfile = std::fs::File::create(&config.webserver.private_key).unwrap();
			keyfile.write(&pkey.private_key_to_pem_pkcs8().map_err(|e| tracing::error!("Failed to serialize private key: {e}"))?)
				.map_err(|e| tracing::error!("Failed to write private key to file: {e}"))?;

			tracing::debug!("Saved private key to {}", config.webserver.certificate.display());
			tracing::debug!("Saved certificate to {}", config.webserver.private_key.display());

			(cert, pkey)
		} else {
			let cert = std::fs::read(&config.webserver.certificate)
				.map_err(|e| tracing::error!("Failed to read server certificate: {e}"))?;
			let cert = openssl::x509::X509::from_pem(&cert)
				.map_err(|e| tracing::error!("Failed to parse server certificate: {e}"))?;

			let pkey = PKey::private_key_from_pem(&std::fs::read(&config.webserver.private_key)
				.map_err(|e| tracing::error!("Failed to read private key: {e}"))?)
				.map_err(|e| tracing::error!("Failed to parse private key: {e}"))?;

			(cert, pkey)
		};

		// Create a manager for interacting with sessions.
		let session_manager = SessionManager::new(config.clone(), shutdown.trigger_shutdown_token(2))?;

		// Create a manager for saving and loading client state.
		let client_manager = ClientManager::new(state.clone(), cert.clone(), pkey, shutdown.trigger_shutdown_token(3));

		// Run the RTSP server.
		let rtsp_server = RtspServer::new(config.clone(), session_manager.clone(), shutdown.clone());

		// Publish the Moonshine service using zeroconf.
		publisher::spawn(config.webserver.port, config.name.clone());

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
