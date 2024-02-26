use pulse::sample::Spec;
use tokio::sync::{mpsc::Sender, oneshot};

mod backend;
use backend::pulseaudio::PulseAudio;

// fn get_default_sink_name() -> Result<String, ()> {
// 	// Create a new PulseAudio context
// 	let mainloop = Rc::new(RefCell::new(Mainloop::new()
// 		.ok_or_else(|| log::error!("Failed to create pulseaudio client."))?));

// 	let mut proplist = Proplist::new()
// 		.ok_or_else(|| log::error!("Failed to create pulseaudio proplist."))?;
//     proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, "Moonshine")
//         .map_err(|()| log::error!("Failed to set pulseaudio application name."))?;
// 	let context = Rc::new(RefCell::new(
// 		Context::new_with_proplist(mainloop.borrow().deref(), "Moonshine context", &proplist)
// 			.ok_or_else(|| log::error!("Failed to create pulseaudio context."))?
// 	));

// 	context.borrow_mut().connect(None, FlagSet::NOFLAGS, None)
// 		.map_err(|e| log::error!("Failed to connect to pulseaudio server: {e}"))?;

// 	// Wait for context to be ready.
// 	loop {
// 		match mainloop.borrow_mut().iterate(false) {
// 			IterateResult::Quit(_) | IterateResult::Err(_) => {
// 				log::error!("Failed to run pulseaudio main loop.");
// 				return Err(());
// 			},
// 			IterateResult::Success(_) => {}
// 		}

// 		match context.borrow().get_state() {
// 			pulse::context::State::Unconnected
// 			| pulse::context::State::Connecting
// 			| pulse::context::State::Authorizing
// 			| pulse::context::State::SettingName => {}
// 			pulse::context::State::Failed | pulse::context::State::Terminated => {
// 				log::error!("Failed to run context.");
// 				return Err(());
// 			}
// 			pulse::context::State::Ready => break
// 		}
// 	}

// 	// Start operation to get server info.
// 	let result = Rc::new(RefCell::new(None));
// 	let operation = {
// 		let result = result.clone();
// 		context.borrow().introspect().get_server_info(move |info| {
// 			let name = match info.default_sink_name.as_ref() {
// 				Some(name) => name,
// 				None => {
// 					log::error!("Failed to receive default sink name.");
// 					return;
// 				}
// 			};
// 			*result.borrow_mut() = Some(name.to_string());
// 		})
// 	};

// 	// Wait for operation to finish.
// 	loop {
// 		match mainloop.borrow_mut().iterate(false) {
// 			IterateResult::Quit(_) | IterateResult::Err(_) => {
// 				log::error!("Failed to run pulseaudio main loop.");
// 				return Err(());
// 			},
// 			IterateResult::Success(_) => {}
// 		};
// 		match operation.get_state() {
// 			pulse::operation::State::Running => {}
// 			pulse::operation::State::Cancelled => {
// 				log::error!("Failed to get default sink name.");
// 				return Err(());
// 			}
// 			pulse::operation::State::Done => break
// 		}
// 	}

// 	result.take().ok_or_else(|| log::error!("Failed to get default sink name result."))
// }

// fn get_default_sink_name() -> Result<String, ()> {
// 	let mut sink_controller = SinkController::new_with_name("Moonshine")
// 		.map_err(|e| log::error!("Failed to create pulseaudio sink controller: {e}"))?;
// 	let server_info = sink_controller.handler().get_server_info()
// 		.map_err(|e| log::error!("Failed to get server info: {e}"))?;
// 	let default_sink_name = server_info.default_sink_name
// 		.ok_or_else(|| log::error!("No default sink found."))?;

// 	Ok(default_sink_name)
// }

pub struct AudioCapture {
	sample_rate: u32,
	channels: u8,
}

impl AudioCapture {
	pub async fn new(audio_tx: Sender<Vec<i16>>) -> Result<Self, ()> {
		let channels = 2u8;
		let sample_rate = 48000u32;

		let (ready_tx, ready_rx) = oneshot::channel();
		let inner = AudioCaptureInner { sample_rate, channels, audio_tx };
		tokio::task::spawn_blocking(move || {
			tokio::runtime::Handle::current().block_on(inner.run(ready_tx))
		});
		ready_rx
			.await
			.map_err(|e| log::error!("Failed to wait for pulseaudio connection: {e}"))??;

		Ok(Self { sample_rate, channels })
	}

	pub fn sample_rate(&self) -> u32 {
		self.sample_rate
	}

	pub fn channels(&self) -> u8 {
		self.channels
	}
}

struct AudioCaptureInner {
	sample_rate: u32,
	channels: u8,

	/// Channel to communicate audio fragments over.
	audio_tx: Sender<Vec<i16>>,
}

impl AudioCaptureInner {
	async fn run(self, ready_tx: oneshot::Sender<Result<(), ()>>) -> Result<(), ()> {
		let sample_spec = Spec {
			format: pulse::sample::Format::S16le,
			channels: self.channels,
			rate: self.sample_rate,
		};
		let mut audio_client = match PulseAudio::new("Moonshine") {
			Ok(audio_client) => audio_client,
			Err(()) => {
				let _ = ready_tx.send(Err(()));
				return Err(());
			},
		};

		let default_sink_name = match audio_client.get_server_info() {
			Ok(server_info) => {
				match server_info.default_sink_name {
					Some(default_sink_name) => default_sink_name,
					None => {
						log::error!("Failed to get default sink name.");
						let _ = ready_tx.send(Err(()));
						return Err(());
					},
				}
			}
			Err(_) => todo!(),
		};

		log::debug!("Found default sink name: {default_sink_name}");

		let source_name = format!("{default_sink_name}.monitor");
		if audio_client.start_recording(&source_name, sample_spec).is_err() {
			let _ = ready_tx.send(Err(()));
			return Err(());
		}
		let _ = ready_tx.send(Ok(()));
		log::info!("Recording from source: {source_name}");

		// Start recording.
		loop {
			let buffer = audio_client.read()?;

			// Convert Vec<u8> to Vec<i16>.
			let samples = unsafe {
				Vec::from_raw_parts(
					buffer.as_ptr() as *mut i16,
					buffer.len() / std::mem::size_of::<i16>(),
					buffer.len() / std::mem::size_of::<i16>(),
				)
			};

			// Forget about our buffer, ownership has been transferred to samples.
			std::mem::forget(buffer);

			match self.audio_tx.send(samples).await {
				Ok(()) => {},
				Err(e) => {
					log::debug!("Received error while sending audio sample: {e}");
					log::info!("Closing audio capture because the receiving end was dropped.");
					break;
				},
			}
		}

		Ok(())

		// // Connect to the PulseAudio server.
		// let stream = psimple::Simple::new(
		// 	None,                             // Use default server.
		// 	"Moonshine audio capture",        // Stream description.
		// 	pulse::stream::Direction::Record, // Direction of audio (recording vs playback).
		// 	Some(&source_name),               // Specify input device.
		// 	"moonshine",                      // Stream name.
		// 	&sample_spec,                     // Sample specification.
		// 	None,                             // Use default channel map.
		// 	Some(&BufferAttr {
		// 		maxlength: sample_rate * channels as u32 * 5 / 1000,
		// 		tlength: std::u32::MAX,
		// 		prebuf: std::u32::MAX,
		// 		minreq: std::u32::MAX,
		// 		fragsize: std::u32::MAX,
		// 	}),
		// ).map_err(|e| log::error!("Failed to create audio capture device: {e}"))?;

		// // Start recording.
		// loop {
		// 	// Allocate buffer for recording.
		// 	let mut buffer = vec![0u8; 960];

		// 	match self.stream.read(&mut buffer) {
		// 		Ok(()) => {
		// 			// Convert Vec<u8> to Vec<i16>.
		// 			let samples = unsafe {
		// 				Vec::from_raw_parts(
		// 					buffer.as_ptr() as *mut i16,
		// 					buffer.len() / std::mem::size_of::<i16>(),
		// 					buffer.len() / std::mem::size_of::<i16>(),
		// 				)
		// 			};

		// 			// Forget about our buffer, ownership has been transferred to samples.
		// 			std::mem::forget(buffer);

		// 			match self.audio_tx.send(samples).await {
		// 				Ok(()) => {},
		// 				Err(e) => {
		// 					log::debug!("Received error while sending audio sample: {e}");
		// 					log::info!("Closing audio capture because the receiving end was dropped.");
		// 					break;
		// 				},
		// 			}
		// 		},
		// 		Err(e) => {
		// 			log::error!("Failed to read audio data: {}", e);
		// 			break;
		// 		}
		// 	}
		// }
	}
}
