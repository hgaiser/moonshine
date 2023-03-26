use std::path::PathBuf;

use clap::Parser;

use crate::util::flatten;

mod config;
mod rtsp;
mod service_publisher;
mod util;
mod webserver;

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

	let config = config::Config::read_from_file(args.config)?;

	log::debug!("Using configuration:\n{:#?}", config);

	let rtsp_task = tokio::spawn(rtsp::run(config.address.clone(), config.rtsp.port, config.session.clone()));
	let publisher_task = tokio::spawn(service_publisher::run(config.webserver.port));
	let webserver_task = tokio::spawn(webserver::run(config.clone()));

	let result = tokio::try_join!(
		flatten(rtsp_task),
		flatten(publisher_task),
		flatten(webserver_task),
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
