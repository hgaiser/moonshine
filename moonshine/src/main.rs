use nvfbc::{BufferFormat, CudaCapturer};
use nvfbc::cuda::CaptureMethod;

use std::path::PathBuf;

use clap::Parser;

use crate::encoder::{NvencEncoder, CodecType, VideoQuality};
use crate::util::flatten;

mod config;
mod cuda;
mod encoder;
mod error;
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

	let config = std::fs::read(args.config)
		.map_err(|e| log::error!("Failed to open configuration file: {}", e))?;
	let config: config::Config = toml::from_slice(&config)
		.map_err(|e| log::error!("Failed to parse configuration file: {}", e))?;

	log::debug!("Using configuration:\n{:#?}", config);

	let rtsp_task = tokio::spawn(rtsp::run(config.address, 2000));

	flatten(rtsp_task).await?;

	// let webserver_task = tokio::spawn(webserver::run(config.clone()));
	// let publisher_task = tokio::spawn(service_publisher::run(config.port));

	// let result = tokio::try_join!(flatten(webserver_task), flatten(publisher_task));
	// match result {
	// 	Ok(_) => {
	// 		println!("Finished without errors.");
	// 	},
	// 	Err(_) => {
	// 		println!("Finished with errors.");
	// 	}
	// };

	// let cuda_context = cuda::init_cuda(0)
	// 	.map_err(|e| log::error!("Failed to initialize CUDA: {}", e))?;

	// // Create a capturer that captures to CUDA context.
	// let mut capturer = CudaCapturer::new()
	// 	.map_err(|e| log::error!("Failed to create CUDA capture device: {}", e))?;

	// let status = capturer.status()
	// 	.map_err(|e| log::error!("Failed to get capturer status: {}", e))?;
	// println!("{:#?}", status);
	// if !status.can_create_now {
	// 	panic!("Can't create a CUDA capture session.");
	// }

	// let width = status.screen_size.w;
	// let height = status.screen_size.h;
	// let fps = 60;

	// capturer.start(BufferFormat::Bgra, fps)
	// 	.map_err(|e| log::error!("Failed to start frame capturer: {}", e))?;

	// let mut encoder = NvencEncoder::new(
	// 	width,
	// 	height,
	// 	CodecType::H264,
	// 	VideoQuality::Slowest,
	// 	cuda_context,
	// )?;

	// let start_time = std::time::Instant::now();
	// while start_time.elapsed().as_secs() < 20 {
	// 	let start = std::time::Instant::now();
	// 	let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
	// 		.map_err(|e| log::error!("Failed to capture frame: {}", e))?;
	// 	encoder.encode(frame_info.device_buffer, start_time.elapsed())
	// 		.map_err(|e| log::error!("Failed to encode frame: {}", e))?;
	// 	println!("Capture: {}msec", start.elapsed().as_millis());
	// }

	// encoder.stop()?;

	Ok(())
}
