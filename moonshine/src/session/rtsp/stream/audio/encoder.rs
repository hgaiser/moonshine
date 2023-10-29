use std::{sync::{Arc, Mutex}, io::BufWriter, fs::File};

use cpal::{SupportedStreamConfig, FromSample, Sample};
use tokio::sync::mpsc::Receiver;

pub struct AudioEncoder {
}

impl AudioEncoder {
	pub fn new(config: SupportedStreamConfig, mut audio_rx: Receiver<Vec<f32>>) -> Result<Self, ()> {
		// The WAV file we're recording to.
		const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/recorded.wav");
		let spec = wav_spec_from_config(&config);
		let writer = hound::WavWriter::create(PATH, spec)
			.map_err(|e| println!("Failed to create WavWriter: {e}"))?;
		let writer = Arc::new(Mutex::new(Some(writer)));

		tokio::spawn(async move {
			while let Some(audio_fragment) = audio_rx.recv().await {
				write_input_data::<f32, f32>(&audio_fragment, &writer);
			}

			log::debug!("Audio capture channel closed.");
		});

		Ok(Self {  })
	}
}

fn sample_format(format: cpal::SampleFormat) -> hound::SampleFormat {
	if format.is_float() {
		hound::SampleFormat::Float
	} else {
		hound::SampleFormat::Int
	}
}

fn wav_spec_from_config(config: &cpal::SupportedStreamConfig) -> hound::WavSpec {
	hound::WavSpec {
		channels: config.channels() as _,
		sample_rate: config.sample_rate().0 as _,
		bits_per_sample: (config.sample_format().sample_size() * 8) as _,
		sample_format: sample_format(config.sample_format()),
	}
}

type WavWriterHandle = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;

fn write_input_data<T, U>(input: &[T], writer: &WavWriterHandle)
where
	T: Sample,
	U: Sample + hound::Sample + FromSample<T>,
{
	if let Ok(mut guard) = writer.try_lock() {
		if let Some(writer) = guard.as_mut() {
			for &sample in input.iter() {
				let sample: U = U::from_sample(sample);
				writer.write_sample(sample).ok();
			}
		}
	}
}
