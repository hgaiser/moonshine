use std::io::Write;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::Parser;
use moonshine::clients::ClientManager;
use moonshine::config::Config;
use moonshine::{app_scanner, publisher};
use moonshine::crypto::create_certificate;
use moonshine::rtsp::RtspServer;
use moonshine::session::SessionManager;
use moonshine::state::State;
use moonshine::webserver::Webserver;
use openssl::pkey::PKey;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,

	/// Show more log messages.
	#[clap(long, short)]
	#[clap(action = clap::ArgAction::Count)]
	verbose: u8,

	/// Show less log messages.
	#[clap(long, short)]
	#[clap(action = clap::ArgAction::Count)]
	quiet: u8,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	let args = Args::parse();

	let log_level = match i16::from(args.verbose) - i16::from(args.quiet) {
		..= -2 => log::LevelFilter::Error,
		-1 => log::LevelFilter::Warn,
		0 => log::LevelFilter::Info,
		1 => log::LevelFilter::Debug,
		2.. => log::LevelFilter::Trace,
	};

	env_logger::Builder::new()
		.filter_module(module_path!(), log_level)
		.format_timestamp_millis()
		.parse_default_env()
		.init();

	let mut config = Config::read_from_file(args.config).map_err(|_| std::process::exit(1))?;

	log::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = app_scanner::scan_applications(&config.application_scanners);
	log::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);

	// Spawn a task to wait for CTRL+C and trigger a shutdown.
	let shutdown = ShutdownManager::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			if let Err(e) = tokio::signal::ctrl_c().await {
				log::error!("Failed to wait for CTRL+C: {e}");
				std::process::exit(1);
			} else {
				log::info!("Received interrupt signal. Shutting down server...");
				shutdown.trigger_shutdown(1).ok();
			}
		}
	});

	// Create the main application.
	let moonshine = Moonshine::new(config, shutdown.clone()).await?;

	// Wait until something causes a shutdown trigger.
	shutdown.wait_shutdown_triggered().await;

	// Drop the main moonshine object, triggering other systems to shutdown too.
	drop(moonshine);

	// Wait until everything was shutdown.
	let exit_code = shutdown.wait_shutdown_complete().await;
	log::trace!("Successfully waited for shutdown to complete.");
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
