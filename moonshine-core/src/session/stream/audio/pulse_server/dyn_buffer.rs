use pulseaudio::protocol as pulse;

use crate::session::stream::audio::buffer::PlaybackBuffer;

/// Dynamic wrapper over `PlaybackBuffer<F>` for different output channel counts.
pub(super) enum DynPlaybackBuffer {
	Stereo(Box<PlaybackBuffer<[f32; 2]>>),
	Surround51(Box<PlaybackBuffer<[f32; 6]>>),
	Surround71(Box<PlaybackBuffer<[f32; 8]>>),
}

impl DynPlaybackBuffer {
	pub fn new(sample_spec: pulse::SampleSpec, channel_map: pulse::ChannelMap, output_spec: pulse::SampleSpec) -> Self {
		match output_spec.channels {
			6 => Self::Surround51(Box::new(PlaybackBuffer::new(sample_spec, channel_map, output_spec))),
			8 => Self::Surround71(Box::new(PlaybackBuffer::new(sample_spec, channel_map, output_spec))),
			_ => Self::Stereo(Box::new(PlaybackBuffer::new(sample_spec, channel_map, output_spec))),
		}
	}

	pub fn reconfigure_output(&mut self, output_spec: pulse::SampleSpec) {
		let (sample_spec, channel_map) = match self {
			Self::Stereo(b) => (b.buffer().sample_spec, b.buffer().channel_map),
			Self::Surround51(b) => (b.buffer().sample_spec, b.buffer().channel_map),
			Self::Surround71(b) => (b.buffer().sample_spec, b.buffer().channel_map),
		};
		*self = Self::new(sample_spec, channel_map, output_spec);
	}

	pub fn len_bytes(&self) -> usize {
		match self {
			Self::Stereo(b) => b.len_bytes(),
			Self::Surround51(b) => b.len_bytes(),
			Self::Surround71(b) => b.len_bytes(),
		}
	}

	pub fn is_empty(&self) -> bool {
		match self {
			Self::Stereo(b) => b.is_empty(),
			Self::Surround51(b) => b.is_empty(),
			Self::Surround71(b) => b.is_empty(),
		}
	}

	pub fn write(&mut self, payload: &[u8]) {
		match self {
			Self::Stereo(b) => b.write(payload),
			Self::Surround51(b) => b.write(payload),
			Self::Surround71(b) => b.write(payload),
		}
	}

	pub fn clear(&mut self) {
		match self {
			Self::Stereo(b) => b.clear(),
			Self::Surround51(b) => b.clear(),
			Self::Surround71(b) => b.clear(),
		}
	}

	pub fn sample_spec(&self) -> pulse::SampleSpec {
		match self {
			Self::Stereo(b) => b.buffer().sample_spec,
			Self::Surround51(b) => b.buffer().sample_spec,
			Self::Surround71(b) => b.buffer().sample_spec,
		}
	}

	/// Drain `num_frames` from the buffer, mix into `output` with per-channel volume.
	/// Returns `true` if frames were available, `false` on underrun.
	pub fn drain_and_mix(&mut self, num_frames: usize, output: &mut [f32], vol: &[f32]) -> bool {
		match self {
			Self::Stereo(b) => drain_mix_impl(b, num_frames, output, vol),
			Self::Surround51(b) => drain_mix_impl(b, num_frames, output, vol),
			Self::Surround71(b) => drain_mix_impl(b, num_frames, output, vol),
		}
	}
}

fn drain_mix_impl<F: dasp::Frame<Sample = f32>>(
	buffer: &mut PlaybackBuffer<F>,
	num_frames: usize,
	output: &mut [f32],
	vol: &[f32],
) -> bool {
	let Some(frames) = buffer.drain(num_frames) else {
		return false;
	};
	let mut resampled = dasp::Signal::into_interleaved_samples(frames).into_iter();
	for (i, sample) in output.iter_mut().enumerate() {
		let s = resampled.next().unwrap_or_default();
		*sample += s * vol[i % vol.len()];
	}
	true
}

#[cfg(test)]
mod epoch_tests {
	use super::*;
	#[test]
	fn output_layout_changes_discard_old_pcm_and_retain_source_format() {
		let spec = pulse::SampleSpec {
			format: pulse::SampleFormat::Float32Le,
			channels: 2,
			sample_rate: 48_000,
		};
		let mut buffer = DynPlaybackBuffer::new(spec, pulse::ChannelMap::stereo(), spec);
		for channels in [6, 8, 2] {
			buffer.write(&0.8f32.to_le_bytes().repeat(20));
			buffer.reconfigure_output(pulse::SampleSpec { channels, ..spec });
			assert!(buffer.is_empty());
			assert_eq!(buffer.sample_spec().channels, 2);
			let mut bytes = Vec::new();
			for _ in 0..10 {
				bytes.extend(0.2f32.to_le_bytes());
				bytes.extend(0.4f32.to_le_bytes());
			}
			buffer.write(&bytes);
			let mut out = vec![0.0; 10 * channels as usize];
			assert!(buffer.drain_and_mix(10, &mut out, &vec![1.0; channels as usize]));
			for frame in out.chunks_exact(channels as usize) {
				assert_eq!(&frame[..2], &[0.2, 0.4]);
				assert!(frame[2..].iter().all(|sample| *sample == 0.0));
			}
		}
	}
}
