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
use crate::session::SessionManager;
use crate::state::State;
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

/// Reasons for initiating a global shutdown, used as the type parameter
/// for `ShutdownManager<ShutdownReason>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
	/// Application quit signal (Ctrl+C or SIGTERM).
	/// Primary shutdown trigger initiated by the user.
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

impl ShutdownReason {
	/// Map a shutdown reason to a process exit code.
	pub const fn exit_code(self) -> i32 {
		self as i32
	}
}

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

	let mut config = Config::load_or_create(&args.config).await?;

	tracing::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);

	app_scanner::resolve_missing_boxart(&mut config.applications);

	// Spawn a task to wait for CTRL+C and trigger a shutdown.
	let shutdown = ShutdownManager::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			let mut terminate = signal(SignalKind::terminate()).unwrap();

			tokio::select! {
				_ = tokio::signal::ctrl_c() => {
					tracing::info!("Received CTRL+C, shutting down...");
				},
				_ = terminate.recv() => {
					tracing::info!("Received SIGTERM, shutting down...");
				}
			}

			shutdown.trigger_shutdown(ShutdownReason::AppQuit).ok();
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
	tracing::debug!("Successfully waited for shutdown to complete.");
	std::process::exit(exit_code.exit_code());
}

pub struct Moonshine {
	_rtsp_server: RtspServer,
	_session_manager: SessionManager,
	_client_manager: ClientManager,
	_webserver: Webserver,
	_discovery: ZeroconfDiscovery,
}

/// The main application struct, responsible for initializing and managing all subsystems.
///
/// Dropping this struct will trigger the shutdown of all subsystems, so it should be kept alive until the application is ready to exit.
impl Moonshine {
	pub async fn new(config: Config, shutdown: ShutdownManager<ShutdownReason>) -> Result<Self, ()> {
		let state = State::new()?;

		// Load or create the TLS certificate and private key for the webserver.
		let (cert, pkey) = webserver::tls::load_or_create_certificate(&config).await?;

		// Create a manager for interacting with sessions.
		let session_manager = SessionManager::new(config.clone(), shutdown.clone())?;

		// Create a manager for saving and loading client state.
		let client_manager = ClientManager::new(state.clone(), cert.clone(), pkey);

		// Run the RTSP server.
		let rtsp_server = RtspServer::new(config.clone(), session_manager.clone(), shutdown.clone());

		// Advertise the Moonshine service via mDNS/zeroconf.
		let discovery = ZeroconfDiscovery::spawn(config.webserver.port, config.name.clone(), shutdown.clone());

		// Run the webserver.
		let webserver = Webserver::new(
			config,
			state.get_uuid()?,
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
			_discovery: discovery,
		})
	}
}
