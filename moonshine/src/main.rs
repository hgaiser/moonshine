use std::path::PathBuf;

use async_shutdown::Shutdown;
use clap::Parser;
use config::Config;
use session::{clients::ClientManager, SessionManager};
use tokio::{sync::mpsc, try_join};

use crate::util::flatten;

mod config;
mod session;
mod service_publisher;
mod util;
mod webserver;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,
}

async fn run(config: Config, shutdown: Shutdown) -> Result<(), ()> {
	// Run the session manager.
	let (session_command_tx, session_command_rx) = mpsc::channel(10);
	let session_manager = SessionManager::new(session_command_rx);
	let session_manager_task = tokio::spawn(shutdown.wrap_vital(session_manager.run(
		config.rtsp.port,
		config.session,
		shutdown.clone(),
	)));

	// Run the client manager.
	let (client_command_tx, client_command_rx) = mpsc::channel(10);
	let client_manager = ClientManager::from_state_or_default(
		&config.webserver.certificate_chain,
		&config.webserver.private_key,
		client_command_rx,
	)?;
	let client_manager_task = tokio::spawn(shutdown.wrap_vital(client_manager.run(shutdown.clone())));

	// Publish the Moonshine service using zeroconf.
	let service_publisher_task = tokio::spawn(shutdown.wrap_vital({
		let name = config.name.clone();
		let port = config.webserver.port;
		let shutdown = shutdown.clone();
		async move {
			service_publisher::run(port, name, shutdown)
		}
	}));

	// Run the webserver that communicates with clients.
	let webserver_task = tokio::spawn(shutdown.wrap_vital(webserver::run(
		config.name.clone(),
		(config.address.clone(), config.webserver.port),
		(config.address.clone(), config.webserver.port_https),
		config.webserver.certificate_chain.clone(),
		config.webserver.private_key.clone(),
		config.applications.clone(),
		client_command_tx,
		session_command_tx,
		shutdown.clone(),
	)));

	match try_join!(
		flatten(session_manager_task),
		flatten(client_manager_task),
		flatten(service_publisher_task),
		flatten(webserver_task),
	) {
		Ok(_) => Ok(()),
		Err(_) => Err(()),
	}
}

#[tokio::main]
async fn main() -> Result<(), ()> {
	env_logger::init();

	let args = Args::parse();

	let config = config::Config::read_from_file(args.config).map_err(|_| std::process::exit(1))?;

	log::debug!("Using configuration:\n{:#?}", config);

	// Spawn a task to wait for CTRL+C and trigger a shutdown.
	let shutdown = Shutdown::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			if let Err(e) = tokio::signal::ctrl_c().await {
				log::error!("Failed to wait for CTRL+C: {}", e);
				std::process::exit(1);
			} else {
				log::info!("Received interrupt signal. Shutting down server...");
				shutdown.shutdown();
			}
		}
	});

	let exit_code = match run(config, shutdown.clone()).await {
		Ok(()) => 0,
		Err(()) => 1,
	};

	shutdown.wait_shutdown_complete().await;

	log::trace!("Successfully waited for shutdown to complete.");

	std::process::exit(exit_code);
}
