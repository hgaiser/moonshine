use std::path::PathBuf;

use clap::Parser;

// use crate::encoder::{NvencEncoder, CodecType, VideoQuality};
use crate::util::flatten;

mod config;
mod cuda;
// mod encoder;
mod error;
mod rtsp;
mod service_publisher;
mod util;
// mod webserver;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), ()> {
	env_logger::init();

	let args = Args::parse();

	let config = std::fs::read_to_string(args.config)
		.map_err(|e| log::error!("Failed to open configuration file: {}", e))?;
	let config: config::Config = toml::from_str(&config)
		.map_err(|e| log::error!("Failed to parse configuration file: {}", e))?;

	log::debug!("Using configuration:\n{:#?}", config);

	let rtsp_task = tokio::spawn(rtsp::run(config.address, config.port));
	let publisher_task = tokio::spawn(service_publisher::run(config.port));
	// let webserver_task = tokio::spawn(webserver::run(config.clone()));

	let result = tokio::try_join!(
		flatten(rtsp_task),
		flatten(publisher_task),
		// flatten(webserver_task),
	);

	match result {
		Ok(_) => {
			println!("Finished without errors.");
		},
		Err(_) => {
			println!("Finished with errors.");
		}
	};

	Ok(())
}
