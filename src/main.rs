use std::io::Write;
use std::path::PathBuf;

use crate::clients::ClientManager;
use crate::config::Config;
use crate::crypto::create_certificate;
use crate::rtsp::RtspServer;
use crate::session::SessionManager;
use crate::state::State;
use crate::webserver::Webserver;
use anyhow::{Context, Result};
use async_shutdown::ShutdownManager;
use clap::Parser;
use openssl::pkey::PKey;
use tracing_subscriber::fmt;
use tracing_subscriber::{filter::LevelFilter, prelude::*, EnvFilter, Layer};

mod app_scanner;
mod clients;
mod config;
mod crypto;
mod ffmpeg;
mod publisher;
mod rtsp;
mod session;
mod state;
mod webserver;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,

	/// Show more tracing messages.
	#[clap(long, short)]
	#[clap(action = clap::ArgAction::Count)]
	verbose: u8,

	/// Show less tracing messages.
	#[clap(long, short)]
	#[clap(action = clap::ArgAction::Count)]
	quiet: u8,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
	let args = Args::parse();

	let log_level = match i16::from(args.verbose) - i16::from(args.quiet) {
		..=-2 => LevelFilter::ERROR,
		-1 => LevelFilter::WARN,
		0 => LevelFilter::INFO,
		1 => LevelFilter::DEBUG,
		2.. => LevelFilter::TRACE,
	};

	tracing_subscriber::registry()
		.with(fmt::layer().with_filter(log_level))
		.with(
			EnvFilter::try_from_default_env()
				.or_else(|_| EnvFilter::try_new("info"))
				.unwrap(),
		)
		.init();

	let mut config;
	if args.config.exists() {
		config = Config::read_from_file(args.config)?
	} else {
		tracing::info!(
			"No config file found at {}, creating a default config file.",
			args.config.display()
		);
		config = Config::default();

		let serialized_config = toml::to_string_pretty(&config).context("Failed to serialize config")?;

		let config_dir = args
			.config
			.parent()
			.context("Failed to get parent directory of config file.")?;
		std::fs::create_dir_all(config_dir).context("Failed to create config directory")?;
		std::fs::write(args.config, serialized_config).context("Failed to save config file")?;
	}

	// Resolve these paths so that the rest of the code doesn't need to.
	let cert_path = config.webserver.certificate.to_string_lossy();
	let cert_path = shellexpand::full(&cert_path).context("Failed to expand certificate path")?;
	config.webserver.certificate = cert_path.to_string().into();

	let private_key_path = config.webserver.private_key.to_string_lossy();
	let private_key_path = shellexpand::full(&private_key_path).context("Failed to expand private key path")?;
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

			tracing::info!("Received interrupt signal. Shutting down server...");
			shutdown.trigger_shutdown(1).ok();
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
	pub async fn new(config: Config, shutdown: ShutdownManager<i32>) -> Result<Self> {
		let state = State::new().await?;

		let (cert, pkey) = if !config.webserver.certificate.exists() && !config.webserver.private_key.exists() {
			tracing::info!("No certificate found, creating a new one.");

			let (cert, pkey) = create_certificate().context("Failed to create certificate")?;

			// Write certificate to file
			let cert_dir = config
				.webserver
				.certificate
				.parent()
				.context("Failed to find parent directory for certificate file.")?;
			std::fs::create_dir_all(cert_dir).context("Failed to create certificate directory")?;
			let mut certfile = std::fs::File::create(&config.webserver.certificate).unwrap();
			certfile
				.write(&cert.to_pem().context("Failed to serialize PEM")?)
				.context("Failed to write PEM to file")?;

			// Write private key to file
			let private_key_dir = config
				.webserver
				.private_key
				.parent()
				.context("Failed to find parent directory for private key file.")?;
			std::fs::create_dir_all(private_key_dir).context("Failed to create private key directory")?;
			let mut keyfile = std::fs::File::create(&config.webserver.private_key).unwrap();
			keyfile
				.write(
					&pkey
						.private_key_to_pem_pkcs8()
						.context("Failed to serialize private key")?,
				)
				.context("Failed to write private key to file")?;

			tracing::debug!("Saved private key to {}", config.webserver.certificate.display());
			tracing::debug!("Saved certificate to {}", config.webserver.private_key.display());

			(cert, pkey)
		} else {
			let cert = std::fs::read(&config.webserver.certificate).context("Failed to read server certificate")?;
			let cert = openssl::x509::X509::from_pem(&cert).context("Failed to parse server certificate")?;

			let pkey = PKey::private_key_from_pem(
				&std::fs::read(&config.webserver.private_key).context("Failed to read private key")?,
			)
			.context("Failed to parse private key")?;

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
