use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::Parser;
use moonshine::config::Config;
use moonshine::{Moonshine, app_scanner};

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
