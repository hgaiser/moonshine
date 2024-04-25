use std::{cell::RefCell, ops::Deref, rc::Rc};

use anyhow::{anyhow, bail, Context, Result};
use pulse::{
	context::{Context as PulseContext, FlagSet},
	def::BufferAttr,
	mainloop::standard::{IterateResult, Mainloop},
	proplist::Proplist,
	sample::Spec,
};
use tokio::sync::mpsc::Sender;

fn get_default_sink_name() -> Result<String> {
	// Create a new PulseAudio context
	let mainloop = Rc::new(RefCell::new(
		Mainloop::new().context("Failed to create pulseaudio client.")?,
	));

	let mut proplist = Proplist::new().context("Failed to create pulseaudio proplist.")?;
	proplist
		.set_str(pulse::proplist::properties::APPLICATION_NAME, "Moonshine")
		.map_err(|_| anyhow!("Failed to create pulseaudio proplist."))?;
	let context = Rc::new(RefCell::new(
		PulseContext::new_with_proplist(mainloop.borrow().deref(), "Moonshine context", &proplist)
			.context("Failed to create pulseaudio context.")?,
	));

	context
		.borrow_mut()
		.connect(None, FlagSet::NOFLAGS, None)
		.context("Failed to connect to pulseaudio server")?;

	// Wait for context to be ready.
	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				bail!("Failed to run pulseaudio main loop.");
			},
			IterateResult::Success(_) => {},
		}

		match context.borrow().get_state() {
			pulse::context::State::Unconnected
			| pulse::context::State::Connecting
			| pulse::context::State::Authorizing
			| pulse::context::State::SettingName => {},
			pulse::context::State::Failed | pulse::context::State::Terminated => {
				bail!("Failed to run context.");
			},
			pulse::context::State::Ready => break,
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
				},
			};
			*result.borrow_mut() = Some(name.to_string());
		})
	};

	// Wait for operation to finish.
	loop {
		match mainloop.borrow_mut().iterate(false) {
			IterateResult::Quit(_) | IterateResult::Err(_) => {
				bail!("Failed to run pulseaudio main loop.")
			},
			IterateResult::Success(_) => {},
		};
		match operation.get_state() {
			pulse::operation::State::Running => {},
			pulse::operation::State::Cancelled => {
				bail!("Failed to get default sink name.")
			},
			pulse::operation::State::Done => break,
		}
	}

	result.take().context("Failed to get default sink name result.")
}

pub struct AudioCapture {
	sample_rate: u32,
	channels: u8,
}

impl AudioCapture {
	pub async fn new(audio_tx: Sender<Vec<i16>>) -> Result<Self> {
		let channels = 2u8;
		let sample_rate = 48000u32;

		let default_sink_name = get_default_sink_name()?;
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
				tlength: u32::MAX,
				prebuf: u32::MAX,
				minreq: u32::MAX,
				fragsize: u32::MAX,
			}),
		)
		.context("Failed to create audio capture device")?;

		tracing::info!("Recording from source: {monitor_name}");

		let inner = AudioCaptureInner { audio_tx };
		tokio::task::spawn_blocking(move || tokio::runtime::Handle::current().block_on(inner.run(stream)));

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
	async fn run(self, stream: pulse_simple::Simple) -> Result<()> {
		// Start recording.
		loop {
			// Allocate buffer for recording.
			let mut buffer = vec![0u8; 480];

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

					match self.audio_tx.send(samples).await {
						Ok(()) => {},
						Err(e) => {
							tracing::info!("Closing audio capture because the receiving end was dropped.");
							bail!("Received error while sending audio sample: {e}")
						},
					}
				},
				Err(e) => {
					bail!("Failed to read audio data: {}", e)
				},
			}
		}
	}
}
