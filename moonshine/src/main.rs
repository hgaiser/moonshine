// use nvfbc::{BufferFormat, CudaCapturer};
// use nvfbc::cuda::CaptureMethod;

// use crate::encoder::{NvencEncoder, CodecType, VideoQuality};
use crate::util::flatten;

mod config;
// mod cuda;
// mod encoder;
// mod error;
mod webserver;
mod service_publisher;
mod util;

#[tokio::main]
async fn main() -> Result<(), ()> {
	env_logger::init();

	let config = config::Config {
		name: "Moonshine PC".to_string(),
		address: "localhost".to_string(),
		port: 47989,
		tls: config::Tls {
			port: 47984,
			certificate_chain: "./cert/cert.pem".into(),
			private_key: "./cert/key.pem".into(),
		},
	};
	let webserver_task = tokio::spawn(webserver::run(config.clone()));
	let publisher_task = tokio::spawn(service_publisher::run(config.port));

	let result = tokio::try_join!(flatten(webserver_task), flatten(publisher_task));
	match result {
		Ok(_) => {
			println!("Finished without errors.");
		},
		Err(_) => {
			println!("Finished with errors.");
		}
	};

	// let cuda_context = cuda::init_cuda(0)?;

	// // Create a capturer that captures to CUDA context.
	// let mut capturer = CudaCapturer::new()?;

	// let status = capturer.status()?;
	// println!("{:#?}", status);
	// if !status.can_create_now {
	// 	panic!("Can't create a CUDA capture session.");
	// }

	// let width = status.screen_size.w;
	// let height = status.screen_size.h;
	// let fps = 60;

	// capturer.start(BufferFormat::Bgra, fps)?;

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
	// 	let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)?;
	// 	encoder.encode(frame_info.device_buffer, start_time.elapsed())?;
	// 	println!("Capture: {}msec", start.elapsed().as_millis());
	// }

	// encoder.stop()?;

	Ok(())
}
