use cpal::{traits::{HostTrait, DeviceTrait, StreamTrait}, SupportedStreamConfig, Stream};
use tokio::sync::mpsc::Sender;

pub struct AudioCapture {
	config: SupportedStreamConfig,
	_stream: Stream,
}

impl AudioCapture {
	pub fn new(audio_tx: Sender<Vec<f32>>) -> Result<Self, ()> {
		let host = cpal::default_host();
		let device = match host.default_input_device() {
			Some(device) => device,
			None => {
				log::warn!("Failed to create audio input device.");
				return Err(());
			}
		};
		let config = match device.default_input_config() {
			Ok(config) => config,
			Err(e) => {
				log::warn!("Failed to get audio device config: {e}");
				return Err(());
			},
		};

		if config.sample_format() != cpal::SampleFormat::F32 {
			log::warn!("Input device has unsupported sample format: {}", config.sample_format());
			return Err(());
		}

		let err_fn = move |e| {
			log::error!("An error occurred while streaming: {e}");
		};

		let on_audio_data = move |data| {
			let _ = audio_tx.blocking_send(data)
				.map_err(|e| log::error!("Failed to send audio fragment: {e}"));
		};

		let stream = match device.build_input_stream(&config.clone().into(), move |data, _: &_| on_audio_data(data.to_owned()), err_fn, None) {
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

	pub fn stream_config(&self) -> SupportedStreamConfig {
		self.config.clone()
	}
}
