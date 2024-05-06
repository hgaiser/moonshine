use std::{cell::RefCell, mem::MaybeUninit, ops::Deref, rc::Rc};

use pulse::{
	context::{Context, FlagSet},
	def::BufferAttr,
	mainloop::standard::{IterateResult, Mainloop},
	proplist::Proplist,
	sample::Spec
};
use tokio::sync::mpsc::Sender;

fn get_default_sink_name() -> Result<String, ()> {
	// Create a new PulseAudio context
	let mainloop = Rc::new(RefCell::new(Mainloop::new()

		.ok_or_else(|| log::error!("Failed to create pulseaudio client."))?));

	let mut proplist = Proplist::new()
		.ok_or_else(|| log::error!("Failed to create pulseaudio proplist."))?;
    proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, "Moonshine")
        .map_err(|()| log::error!("Failed to set pulseaudio application name."))?;
	let context = Rc::new(RefCell::new(
		Context::new_with_proplist(mainloop.borrow().deref(), "Moonshine context", &proplist)
			.ok_or_else(|| log::error!("Failed to create pulseaudio context."))?
	));

	context.borrow_mut().connect(None, FlagSet::NOFLAGS, None)
		.map_err(|e| log::error!("Failed to connect to pulseaudio server: {e}"))?;

	// Wait for context to be ready.
	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				log::error!("Failed to run pulseaudio main loop.");
				return Err(());
			},
			IterateResult::Success(_) => {}
		}

		match context.borrow().get_state() {
			pulse::context::State::Unconnected
			| pulse::context::State::Connecting
			| pulse::context::State::Authorizing
			| pulse::context::State::SettingName => {}
			pulse::context::State::Failed | pulse::context::State::Terminated => {
				log::error!("Failed to run context.");
				return Err(());
			}
			pulse::context::State::Ready => break
		}
	}

	// Start operation to get server info.
	let result = Rc::new(RefCell::new(None));
	let operation = {
		let result = result.clone();
		context.borrow().introspect().get_server_info(move |info| {
			let name = match info.default_sink_name.as_ref() {
				Some(name) => name,
				None => {
					log::error!("Failed to receive default sink name.");
					return;
				}
			};
			*result.borrow_mut() = Some(name.to_string());
		})
	};

	// Wait for operation to finish.
	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				log::error!("Failed to run pulseaudio main loop.");
				return Err(());
			},
			IterateResult::Success(_) => {}
		};
		match operation.get_state() {
			pulse::operation::State::Running => {}
			pulse::operation::State::Cancelled => {
				log::error!("Failed to get default sink name.");
				return Err(());
			}
			pulse::operation::State::Done => break
		}
	}

	result.take().ok_or_else(|| log::error!("Failed to get default sink name result."))
}

pub struct AudioCapture {
	sample_rate: u32,
	channels: u8,
}

impl AudioCapture {
	pub async fn new(audio_tx: Sender<Vec<i16>>) -> Result<Self, ()> {
		let channels = 2u8;
		let sample_rate = 48000u32;

		let default_sink_name = match get_default_sink_name() {
			Ok(name) => name,
			Err(()) => {
				return Err(());
			}
		};
		let monitor_name = format!("{default_sink_name}.monitor");

		let sample_spec = Spec {
			format: pulse::sample::Format::S16le,
			channels,
			rate: sample_rate,
		};

		// Connect to the PulseAudio server.
		let stream = pulse_simple::Simple::new(
			None,                             // Use default server.
			"Moonshine audio capture",        // Stream description.
			pulse::stream::Direction::Record, // Direction of audio (recording vs playback).
			Some(&monitor_name),              // Specify input device.
			"moonshine",                      // Stream name.
			&sample_spec,                     // Sample specification.
			None,                             // Use default channel map.
			Some(&BufferAttr {
				maxlength: std::mem::size_of::<i16>() as u32 * sample_rate * channels as u32 * 5 / 1000,
				tlength: std::u32::MAX,
				prebuf: std::u32::MAX,
				minreq: std::u32::MAX,
				fragsize: std::u32::MAX,
			}),
		).map_err(|e| log::error!("Failed to create audio capture device: {e}"));

		let stream = match stream {
			Ok(stream) => stream,
			Err(()) => {
				return Err(());
			},
		};

		log::info!("Recording from source: {monitor_name}");

		let inner = AudioCaptureInner { audio_tx };
		std::thread::Builder::new().name("audio-capture".to_string()).spawn(move ||
			inner.run(stream)
		)
			.map_err(|e| log::error!("Failed to start audio capture thread: {e}"))?;

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
	/// Channel to communicate audio fragments over.
	audio_tx: Sender<Vec<i16>>,
}

impl AudioCaptureInner {
	fn run(self, stream: pulse_simple::Simple) -> Result<(), ()> {
		// Start recording.
		loop {
			// Allocate uninitialized buffer for recording.
			let buffer: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); 480];
			let mut buffer = unsafe { std::mem::transmute::<_, Vec<u8>>(buffer) };

			match stream.read(&mut buffer) {
				Ok(()) => {
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

					match self.audio_tx.blocking_send(samples) {
						Ok(()) => {},
						Err(e) => {
							log::debug!("Received error while sending audio sample: {e}");
							log::info!("Closing audio capture because the receiving end was dropped.");
							return Err(());
						},
					}
				},
				Err(e) => {
					log::error!("Failed to read audio data: {}", e);
					return Err(());
				}
			}
		}
	}
}
