use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use moonshine_core::clients::ClientManager;
use moonshine_core::config::Config;
use moonshine_core::discovery::ZeroconfDiscovery;
use moonshine_core::rtsp::RtspServer;
use moonshine_core::session::manager::SessionManager;
use moonshine_core::webserver::Webserver;
pub use moonshine_core::ShutdownReason;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to the configuration file.
	config: PathBuf,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	let args = Args::parse();

	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer())
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("error")))
		.init();

	let mut config = Config::load_or_create(&args.config)?;
	tracing::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = moonshine_core::app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);
	moonshine_core::app_scanner::resolve_missing_boxart(&mut config.applications);

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

	let moonshine = Moonshine::new(config, shutdown.clone())?;
	tracing::info!("Moonshine is ready and waiting for connections.");

	shutdown.wait_shutdown_triggered().await;
	drop(moonshine);

	let exit_code = shutdown.wait_shutdown_complete().await;
	tracing::debug!("Successfully waited for shutdown to complete.");
	std::process::exit(exit_code as i32);
}

pub struct Moonshine {
	_rtsp_server: RtspServer,
	_session_manager: SessionManager,
	_client_manager: ClientManager,
	_webserver: Webserver,
	_discovery: ZeroconfDiscovery,
}

impl Moonshine {
	#[allow(clippy::result_unit_err)]
	pub fn new(config: Config, shutdown: ShutdownManager<ShutdownReason>) -> Result<Self, ()> {
		let (cert, pkey) = moonshine_core::tls::load_or_create_certificate(&config)?;

		let session_manager = SessionManager::new(
			config.compositor.clone(),
			config.stream.video.clone(),
			config.stream.audio.clone(),
			config.stream.control.clone(),
			config.address.clone(),
			config.stream.timeout,
			shutdown.clone(),
		)?;
		let client_manager = ClientManager::new(cert.clone(), pkey.clone())?;

		Ok(Self {
			_rtsp_server: RtspServer::new(
				config.address.clone(),
				config.stream.port,
				config.stream.video.clone(),
				config.stream.audio.clone(),
				config.stream.control.clone(),
				session_manager.clone(),
				shutdown.clone(),
			),
			_session_manager: session_manager.clone(),
			_client_manager: client_manager.clone(),
			_webserver: Webserver::new(
				config.name.clone(),
				config.address.clone(),
				config.stream.port,
				config.webserver.clone(),
				config.applications.clone(),
				config.compositor.clone(),
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
