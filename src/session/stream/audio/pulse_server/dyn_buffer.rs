use pulseaudio::protocol as pulse;

use super::super::buffer::PlaybackBuffer;

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

	/// Drain frames without mixing (just drop them).
	pub fn drain_discard(&mut self, num_frames: usize) -> bool {
		match self {
			Self::Stereo(b) => b.drain(num_frames).is_some(),
			Self::Surround51(b) => b.drain(num_frames).is_some(),
			Self::Surround71(b) => b.drain(num_frames).is_some(),
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
