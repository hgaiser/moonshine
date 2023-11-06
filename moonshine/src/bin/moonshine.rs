use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::Parser;
use moonshine::config::Config;
use moonshine::Moonshine;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	env_logger::builder()
		.format_timestamp_millis()
		.init();

	let args = Args::parse();

	let config = Config::read_from_file(args.config).map_err(|_| std::process::exit(1))?;

	log::debug!("Using configuration:\n{:#?}", config);

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
	let moonshine = Moonshine::new(config, shutdown.clone())?;

	// Wait until something causes a shutdown trigger.
	shutdown.wait_shutdown_triggered().await;

	// Drop the main moonshine object, triggering other systems to shutdown too.
	drop(moonshine);

	// Wait until everything was shutdown.
	let exit_code = shutdown.wait_shutdown_complete().await;
	log::trace!("Successfully waited for shutdown to complete.");
	std::process::exit(exit_code);
}
