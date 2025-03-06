use std::{cell::RefCell, mem::MaybeUninit, ops::Deref, rc::Rc, sync::{atomic::{AtomicBool, Ordering}, Arc}, thread::JoinHandle};

use async_shutdown::TriggerShutdownToken;
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

		.ok_or_else(|| tracing::error!("Failed to create pulseaudio client."))?));

	let mut proplist = Proplist::new()
		.ok_or_else(|| tracing::error!("Failed to create pulseaudio proplist."))?;
    proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, "Moonshine")
        .map_err(|()| tracing::error!("Failed to set pulseaudio application name."))?;
	let context = Rc::new(RefCell::new(
		Context::new_with_proplist(mainloop.borrow().deref(), "Moonshine context", &proplist)
			.ok_or_else(|| tracing::error!("Failed to create pulseaudio context."))?
	));

	context.borrow_mut().connect(None, FlagSet::NOFLAGS, None)
		.map_err(|e| tracing::error!("Failed to connect to pulseaudio server: {e}"))?;

	// Wait for context to be ready.
	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				tracing::error!("Failed to run pulseaudio main loop.");
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
				tracing::error!("Failed to run context.");
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
					tracing::error!("Failed to receive default sink name.");
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
				tracing::error!("Failed to run pulseaudio main loop.");
				return Err(());
			},
			IterateResult::Success(_) => {}
		};
		match operation.get_state() {
			pulse::operation::State::Running => {}
			pulse::operation::State::Cancelled => {
				tracing::error!("Failed to get default sink name.");
				return Err(());
			}
			pulse::operation::State::Done => break
		}
	}

	result.take().ok_or_else(|| tracing::error!("Failed to get default sink name result."))
}

pub struct AudioCapture {
	sample_rate: u32,
	channels: u8,
	stop_flag: Arc<AtomicBool>,
	inner_handle: JoinHandle<()>,
}

impl AudioCapture {
	pub async fn new(
		audio_tx: Sender<Vec<f32>>,
		session_stop_token: TriggerShutdownToken<()>,
	) -> Result<Self, ()> {
		tracing::info!("Starting audio capturer.");

		// TODO: Make configurable.
		let channels = 2u8;
		let sample_rate = 48000u32;
		let sample_time_ms = 5;

		let default_sink_name = match get_default_sink_name() {
			Ok(name) => name,
			Err(()) => {
				return Err(());
			}
		};
		let monitor_name = format!("{default_sink_name}.monitor");

		let sample_spec = Spec {
			format: pulse::sample::Format::F32le,
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
				maxlength: u32::MAX,
				tlength: u32::MAX,
				prebuf: u32::MAX,
				minreq: u32::MAX,
				fragsize: std::mem::size_of::<f32>() as u32 * sample_rate * channels as u32 * sample_time_ms / 1000,
			}),
		).map_err(|e| tracing::error!("Failed to create audio capture device: {e}"));

		let stream = match stream {
			Ok(stream) => stream,
			Err(()) => {
				return Err(());
			},
		};

		tracing::info!("Recording from source: {monitor_name}");

		let inner = AudioCaptureInner { audio_tx };
		let stop_flag = Arc::new(AtomicBool::new(false));
		let inner_handle = std::thread::Builder::new().name("audio-capture".to_string()).spawn({
			let stop_flag = stop_flag.clone();
			move || inner.run(stream, stop_flag, session_stop_token)
		})
			.map_err(|e| tracing::error!("Failed to start audio capture thread: {e}"))?;

		Ok(Self { sample_rate, channels, stop_flag, inner_handle })
	}

	pub async fn stop(self) -> Result<(), ()> {
		tracing::info!("Requesting audio capture to stop.");
		self.stop_flag.store(true, Ordering::Relaxed);
		self.inner_handle.join()
			.map_err(|_| tracing::error!("Failed to join audio capture thread."))?;
		Ok(())
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
	audio_tx: Sender<Vec<f32>>,
}

impl AudioCaptureInner {
	fn run(
		self,
		stream: pulse_simple::Simple,
		stop_flag: Arc<AtomicBool>,
		session_stop_token: TriggerShutdownToken<()>,
	) {
		// TODO: Make configurable.
		const SAMPLE_RATE: usize = 48000;
		const SAMPLE_TIME_MS: usize = 5;
		const FRAME_SIZE: usize = std::mem::size_of::<f32>() * SAMPLE_RATE * SAMPLE_TIME_MS / 1000;

		// Start recording.
		while !stop_flag.load(Ordering::Relaxed) {
			// Allocate uninitialized buffer for recording.
			let buffer: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); FRAME_SIZE];
			let mut buffer = unsafe {
				std::mem::transmute::<std::vec::Vec<std::mem::MaybeUninit<u8>>, std::vec::Vec<u8>>(buffer)
			};

			match stream.read(&mut buffer) {
				Ok(()) => {
					// Convert Vec<u8> to Vec<f32>.
					let samples = unsafe {
						Vec::from_raw_parts(
							buffer.as_ptr() as *mut f32,
							buffer.len() / std::mem::size_of::<f32>(),
							buffer.len() / std::mem::size_of::<f32>(),
						)
					};

					// Forget about our buffer, ownership has been transferred to samples.
					std::mem::forget(buffer);

					match self.audio_tx.blocking_send(samples) {
						Ok(()) => {},
						Err(e) => {
							// If AudioStream is dropped, then AudioEncoder is dropped, which closes this channel.
							tracing::debug!("Received error while sending audio sample: {e}");
							tracing::info!("Closing audio capture because the channel was closed.");
							break;
						},
					}
				},
				Err(e) => {
					tracing::error!("Failed to read audio data: {}", e);
					break;
				}
			}
		}

		// If we were asked to stop, ignore the stop token, no need to panic.
		if stop_flag.load(Ordering::Relaxed) {
			session_stop_token.forget();
		}

		tracing::info!("Audio capture stopped.");
	}
}
