use cpal::{traits::{HostTrait, DeviceTrait, StreamTrait}, SupportedStreamConfig, Stream, StreamConfig};
use tokio::sync::mpsc::Sender;

pub struct AudioCapture {
	config: StreamConfig,
	_stream: Stream,
}

impl AudioCapture {
	pub fn new(audio_tx: Sender<Vec<i16>>) -> Result<Self, ()> {
		let host = cpal::default_host();
		let device = host.output_devices()
			.map_err(|e| println!("Failed to get output devices: {e}"))?
			.find(|x| x.name().map(|y| y == "pulse").unwrap_or(false))
			.ok_or_else(|| log::error!("No pulse audio backend found, currently only pulse is supported."))?;

		let config = SupportedStreamConfig::new(
			2,
			cpal::SampleRate(48000),
			cpal::SupportedBufferSize::Range { min: 1920, max: 1920 },
			cpal::SampleFormat::I16
		);
		let mut config: StreamConfig = config.into();
		config.buffer_size = cpal::BufferSize::Fixed(std::mem::size_of::<i16>() as u32 * config.sample_rate.0 * config.channels as u32 * 5 / 1000);

		let err_fn = move |e| {
			log::warn!("An error occurred while streaming: {e}");
		};

		let on_audio_data = move |data| {
			let _ = audio_tx.blocking_send(data)
				.map_err(|e| log::error!("Failed to send audio fragment: {e}"));
		};

		let stream = match device.build_input_stream(&config, move |data, _: &_| on_audio_data(data.to_owned()), err_fn, None) {
			Ok(stream) => stream,
			Err(e) => {
				log::warn!("Failed to build input stream: {e}");
				return Err(());
			}
		};

		if let Err(e) = stream.play() {
			log::warn!("Failed to stream audio: {e}");
			return Err(());
		}

		Ok(Self { config, _stream: stream })
	}

	pub fn stream_config(&self) -> StreamConfig {
		self.config.clone()
	}
}
