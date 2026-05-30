use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::clients::ClientManager;
use crate::config::Config;
use crate::discovery::ZeroconfDiscovery;
use crate::rtsp::RtspServer;
use crate::session::manager::SessionManager;
use crate::webserver::Webserver;

mod app_scanner;
mod clients;
mod config;
mod crypto;
mod discovery;
mod rtsp;
mod session;
mod state;
mod webserver;

/// Reasons for initiating a global shutdown.
///
/// Used as the type parameter for `ShutdownManager<ShutdownReason>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
	/// Application quit signal (Ctrl+C or SIGTERM).
	///
	/// Usually means a shutdown trigger initiated by the user.
	AppQuit = 1,

	/// HTTP webserver is shutting down.
	HttpShutdown = 2,

	/// HTTPS webserver is shutting down.
	HttpsShutdown = 3,

	/// RTSP server is shutting down.
	RtspShutdown = 4,

	/// Session manager guard token (trigger_shutdown_token, not a shutdown trigger).
	SessionManagerShutdown = 5,
}

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to the configuration file.
	config: PathBuf,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	// Parse command-line arguments.
	let args = Args::parse();

	// Initialize logging with tracing_subscriber, using the MOONSHINE_LOG environment variable for filtering.
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer())
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("error")))
		.init();

	// Load or create the configuration file.
	let mut config = Config::load_or_create(&args.config)?;
	tracing::debug!("Using configuration:\n{:#?}", config);

	// Scan for applications and add them to the configuration.
	let scanned_applications = app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);
	app_scanner::resolve_missing_boxart(&mut config.applications);

	// Spawn task to wait for CTRL+C and trigger a shutdown.
	let shutdown = ShutdownManager::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			let mut terminate = signal(SignalKind::terminate()).unwrap();

			tokio::select! {
				_ = tokio::signal::ctrl_c() => {
					tracing::info!("Received SIGINT, shutting down...");
				},
				_ = terminate.recv() => {
					tracing::info!("Received SIGTERM, shutting down...");
				}
			}

			shutdown.trigger_shutdown(ShutdownReason::AppQuit).ok();
		}
	});

	// Create the main application, which will initialize all subsystems.
	let moonshine = Moonshine::new(config, shutdown.clone())?;
	tracing::info!("Moonshine is ready and waiting for connections.");

	// Wait until something causes a shutdown trigger.
	shutdown.wait_shutdown_triggered().await;

	// Drop the main moonshine object, triggering subsystems to shutdown too.
	drop(moonshine);

	// Wait until everything was shutdown.
	let exit_code = shutdown.wait_shutdown_complete().await;
	tracing::debug!("Successfully waited for shutdown to complete.");
	std::process::exit(exit_code as i32);
}

/// The main application struct, responsible for initializing and managing all subsystems.
///
/// This struct acts as a drop guard for all subsystems. Dropping this struct will trigger application shutdown.
pub struct Moonshine {
	// RTSP server for handling streaming connections.
	_rtsp_server: RtspServer,
	// Manager for interacting with sessions.
	_session_manager: SessionManager,
	// Manager for saving and loading client state.
	_client_manager: ClientManager,
	// Server for handling HTTP(S) requests.
	_webserver: Webserver,
	// Zeroconf/mDNS discovery service.
	_discovery: ZeroconfDiscovery,
}

impl Moonshine {
	#[allow(clippy::result_unit_err)]
	pub fn new(config: Config, shutdown: ShutdownManager<ShutdownReason>) -> Result<Self, ()> {
		// Load or create the TLS certificate and private key for the webserver.
		let (cert, pkey) = webserver::tls::load_or_create_certificate(&config)?;

		let session_manager = SessionManager::new(config.clone(), shutdown.clone())?;
		let client_manager = ClientManager::new(cert.clone(), pkey.clone())?;

		Ok(Self {
			_rtsp_server: RtspServer::new(config.clone(), session_manager.clone(), shutdown.clone()),
			_session_manager: session_manager.clone(),
			_client_manager: client_manager.clone(),
			_webserver: Webserver::new(
				config.clone(),
				client_manager.persistent_state().get_uuid()?.to_string(),
				cert,
				client_manager,
				session_manager,
				shutdown.clone(),
			)?,
			_discovery: ZeroconfDiscovery::spawn(config.webserver.port, config.name, shutdown),
		})
	}
}
