use std::{collections::VecDeque, io};

use arrayvec::ArrayVec;
use byteorder::{BigEndian as BE, LittleEndian as LE, ReadBytesExt as _};
use pulse::sample_spec::MAX_CHANNELS;
use pulseaudio::protocol::{self as pulse, ChannelPosition};
use rubato::Resampler;
use rubato::audioadapter::AdapterMut;
use rubato::audioadapter_buffers::direct::InterleavedSlice;

use crate::session::stream::audio::{AudioChannels, SinkSpec};

/// Raw bytes from an app's playback stream go in; frames in the sink's
/// channel layout and rate come out.
pub(crate) struct PlaybackBuffer {
	/// What the app sends: any supported format, rate and channel count.
	stream_spec: pulse::SampleSpec,
	/// What the sink consumes: f32, 48 kHz, 2/6/8 channels.
	sink_spec: SinkSpec,
	buffer: Buffer,
	convert: RateConvert,
}

enum RateConvert {
	Passthrough,
	Resample {
		resampler: Box<rubato::Async<f32>>,

		// scratch buffers for resampling, to reuse the allocations
		input_frames: Vec<f32>,
		resampled_frames: Vec<f32>,
	},
}

impl PlaybackBuffer {
	pub fn passthrough(stream_spec: pulse::SampleSpec, stream_map: pulse::ChannelMap, sink_spec: SinkSpec) -> Self {
		Self {
			stream_spec,
			sink_spec,
			buffer: Buffer::new(stream_spec, stream_map),
			convert: RateConvert::Passthrough,
		}
	}

	pub fn resample(
		stream_spec: pulse::SampleSpec,
		stream_map: pulse::ChannelMap,
		sink_spec: SinkSpec,
		clock_rate: u32,
	) -> Self {
		let num_frames = (sink_spec.sample_rate as u32 / clock_rate) as usize;
		let resampler = new_resampler(stream_spec, sink_spec, num_frames);
		let convert = RateConvert::Resample {
			input_frames: vec![0.0; resampler.input_frames_max() * sink_spec.channels as usize],
			resampled_frames: vec![0.0; num_frames * sink_spec.channels as usize],
			resampler: Box::new(resampler),
		};
		Self {
			stream_spec,
			sink_spec,
			buffer: Buffer::new(stream_spec, stream_map),
			convert,
		}
	}

	pub fn stream_spec(&self) -> pulse::SampleSpec {
		self.stream_spec
	}

	fn buffer(&self) -> &Buffer {
		&self.buffer
	}

	fn buffer_mut(&mut self) -> &mut Buffer {
		&mut self.buffer
	}

	pub fn len_bytes(&self) -> usize {
		self.buffer().len_bytes()
	}

	pub fn len_frames(&self) -> usize {
		self.buffer().len_frames()
	}

	pub fn is_empty(&self) -> bool {
		self.len_frames() == 0
	}

	pub fn write(&mut self, payload: &[u8]) {
		let _ = io::Write::write_all(&mut self.buffer_mut().inner, payload);
	}

	/// Reads data from the buffer at the output sample rate, writing
	/// `num_frames` into `output` if there is sufficient data to do so.
	/// The frames are remixed to fit the requested channel spec.
	///
	/// Returns `false` on underrun (insufficient input).
	pub fn drain_and_mix(&mut self, num_frames: usize, output: &mut [f32], vol: &[f32]) -> bool {
		let sink_channels = self.sink_spec.channels;
		match &mut self.convert {
			RateConvert::Passthrough => {
				if self.buffer.len_frames() < num_frames {
					return false;
				}

				for out_frame in output.chunks_exact_mut(sink_channels as usize).take(num_frames) {
					let frame = self.buffer.read_frame_and_remix(sink_channels);
					for (ch, (out, sample)) in out_frame.iter_mut().zip(frame).enumerate() {
						*out += sample * vol[ch % vol.len()];
					}
				}
			},
			RateConvert::Resample {
				resampler,
				input_frames,
				resampled_frames,
			} => {
				if resampler.output_frames_max() != num_frames {
					**resampler = new_resampler(self.stream_spec, self.sink_spec, num_frames);
					input_frames.resize(resampler.input_frames_max() * sink_channels as usize, 0.0);
					resampled_frames.resize(num_frames * sink_channels as usize, 0.0);
				}

				// The resampler always yields `num_frames`, but how many input frames get
				// consumed varies per tick (e.g. 220 or 221 frames for a 44.1 kHz stream).
				let num_frames_required = resampler.input_frames_next();
				if self.buffer.len_frames() < num_frames_required {
					return false;
				}

				let mut input =
					InterleavedSlice::new_mut(input_frames.as_mut_slice(), sink_channels as usize, num_frames_required)
						.unwrap();

				for frame_idx in 0..num_frames_required {
					let frame = self.buffer.read_frame_and_remix(sink_channels);
					for (ch, sample) in frame.into_iter().enumerate() {
						input.write_sample(ch, frame_idx, &sample);
					}
				}

				let mut out = InterleavedSlice::new_mut(resampled_frames, sink_channels as usize, num_frames).unwrap();
				if let Err(err) = resampler.process_into_buffer(&input, &mut out, None) {
					tracing::error!("Failed to resample audio: {err}");
					return false;
				}

				for (i, (out, sample)) in output.iter_mut().zip(resampled_frames.iter()).enumerate() {
					*out += sample * vol[i % vol.len()];
				}
			},
		}

		true
	}

	pub fn clear(&mut self) {
		self.buffer_mut().inner.clear()
	}
}

fn new_resampler(stream_spec: pulse::SampleSpec, sink_spec: SinkSpec, num_frames: usize) -> rubato::Async<f32> {
	rubato::Async::<f32>::new_sinc(
		sink_spec.sample_rate as u32 as f64 / stream_spec.sample_rate as f64,
		1.0, // The ratio never changes
		&rubato::SincInterpolationParameters::default(),
		num_frames,
		sink_spec.channels as usize,
		rubato::FixedAsync::Output,
	)
	.expect("valid resampler parameters")
}

/// Raw bytes from the app, decoded according to the stream's spec.
pub(crate) struct Buffer {
	inner: VecDeque<u8>,
	stream_spec: pulse::SampleSpec,
	downmix: DownmixCoeffs,
}

impl Buffer {
	pub fn new(stream_spec: pulse::SampleSpec, stream_map: pulse::ChannelMap) -> Self {
		let downmix = DownmixCoeffs::from_channel_map(&stream_map, stream_spec.channels);
		Self {
			inner: VecDeque::new(),
			stream_spec,
			downmix,
		}
	}

	fn len_bytes(&self) -> usize {
		self.inner.len()
	}

	fn len_frames(&self) -> usize {
		let input_channels = self.stream_spec.channels as usize;
		self.inner.len() / (input_channels * self.stream_spec.format.bytes_per_sample())
	}

	fn read_frame(&mut self) -> ArrayVec<f32, { MAX_CHANNELS as usize }> {
		let num_channels = self.stream_spec.channels as usize;
		let mut frame = ArrayVec::new();
		if self.len_frames() == 0 {
			for _ in 0..num_channels {
				frame.push(0.0);
			}
		} else {
			let format = self.stream_spec.format;
			// Read all input samples for this frame.
			for _ in 0..num_channels {
				frame.push(read_sample(format, &mut self.inner).unwrap());
			}
		}
		frame
	}

	fn read_frame_and_remix(&mut self, sink_channels: AudioChannels) -> ArrayVec<f32, { MAX_CHANNELS as usize }> {
		let mut frame = self.read_frame();

		let out_ch = sink_channels as usize;
		match (self.stream_spec.channels, sink_channels) {
			// Passthrough — same channel count.
			(src_ch, _) if src_ch as usize == out_ch => (),

			// Downmix anything wider than stereo (3.0, quad, 5.1, etc) into a stereo sink.
			(src_ch, AudioChannels::Stereo) if src_ch > 2 => {
				// Stereo downmix using ITU-R BS.775 coefficients.
				let left = self.downmix.mix_left(&frame);
				let right = self.downmix.mix_right(&frame);
				frame.clear();
				frame.push(left);
				frame.push(right);
			},

			// Upmix: mono → both FL/FR; otherwise zero-fill remaining channels.
			(1, _) => {
				frame.push(frame[0]);
				while frame.len() < out_ch {
					frame.push(0.0);
				}
			},

			// upmix with silence
			(src_ch, _) if out_ch > src_ch as usize => {
				// Don't try to upmix surround, should we?
				while frame.len() < out_ch {
					frame.push(0.0);
				}
			},

			// Downmix: more input channels than output channels.
			// just drop the extra channels, should we downmix more smartly?
			_ => frame.truncate(out_ch),
		}

		frame
	}
}

fn read_sample<R: io::Read>(fmt: pulse::SampleFormat, mut r: R) -> Option<f32> {
	use dasp::Sample;

	match fmt {
		pulse::SampleFormat::Float32Le => r.read_f32::<LE>().ok(),
		pulse::SampleFormat::Float32Be => r.read_f32::<BE>().ok(),
		pulse::SampleFormat::S16Le => r.read_i16::<LE>().ok().map(Sample::from_sample),
		pulse::SampleFormat::S16Be => r.read_i16::<BE>().ok().map(Sample::from_sample),
		pulse::SampleFormat::U8 => r.read_u8().ok().map(Sample::from_sample),
		pulse::SampleFormat::S32Le => r.read_i32::<LE>().ok().map(Sample::from_sample),
		pulse::SampleFormat::S32Be => r.read_i32::<BE>().ok().map(Sample::from_sample),
		pulse::SampleFormat::S24Le => r.read_i24::<LE>().ok().map(Sample::from_sample),
		pulse::SampleFormat::S24Be => r.read_i24::<BE>().ok().map(Sample::from_sample),
		_ => unreachable!("unsupported sample format {:?}", fmt),
	}
}

/// Pre-computed per-channel downmix coefficients for stereo output.
///
/// Based on ITU-R BS.775 coefficients:
/// - Center → both L and R at 1/√2 ≈ 0.707
/// - Rear/Side → opposite stereo channel at 1/√2 ≈ 0.707
/// - LFE → both L and R at 1/√2
struct DownmixCoeffs {
	left: Vec<f32>,
	right: Vec<f32>,
}

const GAIN_CENTER: f32 = std::f32::consts::FRAC_1_SQRT_2; // 1/√2 ≈ 0.707

impl DownmixCoeffs {
	fn from_channel_map(channel_map: &pulse::ChannelMap, channels: u8) -> Self {
		let mut left = vec![0.0f32; channels as usize];
		let mut right = vec![0.0f32; channels as usize];

		for (i, pos) in channel_map.into_iter().enumerate() {
			if i >= channels as usize {
				break;
			}

			match pos {
				ChannelPosition::Mono => {
					left[i] = GAIN_CENTER;
					right[i] = GAIN_CENTER;
				},
				ChannelPosition::FrontLeft => {
					left[i] = 1.0;
				},
				ChannelPosition::FrontRight => {
					right[i] = 1.0;
				},
				ChannelPosition::FrontCenter => {
					left[i] = GAIN_CENTER;
					right[i] = GAIN_CENTER;
				},
				ChannelPosition::Lfe => {
					left[i] = GAIN_CENTER;
					right[i] = GAIN_CENTER;
				},
				ChannelPosition::RearLeft | ChannelPosition::SideLeft => {
					left[i] = GAIN_CENTER;
				},
				ChannelPosition::RearRight | ChannelPosition::SideRight => {
					right[i] = GAIN_CENTER;
				},
				ChannelPosition::RearCenter => {
					left[i] = 0.5;
					right[i] = 0.5;
				},
				ChannelPosition::FrontLeftOfCenter => {
					left[i] = 1.0;
					right[i] = GAIN_CENTER;
				},
				ChannelPosition::FrontRightOfCenter => {
					left[i] = GAIN_CENTER;
					right[i] = 1.0;
				},
				_ => {
					// Unknown position — mix equally into both channels at reduced gain.
					left[i] = 0.5;
					right[i] = 0.5;
				},
			}
		}

		Self { left, right }
	}

	fn mix_left(&self, samples: &[f32]) -> f32 {
		self.left
			.iter()
			.zip(samples.iter())
			.map(|(coeff, sample)| coeff * sample)
			.sum()
	}

	fn mix_right(&self, samples: &[f32]) -> f32 {
		self.right
			.iter()
			.zip(samples.iter())
			.map(|(coeff, sample)| coeff * sample)
			.sum()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::audio::pulse_server::DEFAULT_CLOCK_RATE_HZ;
	use pulseaudio::protocol as pulse;

	/// Single-tone SNR of the left channel in dB; higher is cleaner.
	fn snr_db(interleaved: &[f32], freq: f64, rate: f64) -> f64 {
		let left: Vec<f64> = interleaved.iter().step_by(2).map(|&x| x as f64).collect();
		let n = left.len() as f64;
		let (mut re, mut im) = (0.0f64, 0.0f64);
		for (i, &s) in left.iter().enumerate() {
			let ph = 2.0 * std::f64::consts::PI * freq * (i as f64) / rate;
			re += s * ph.cos();
			im += s * ph.sin();
		}
		let fund = 2.0 * (re * re + im * im) / (n * n);
		let total = left.iter().map(|&s| s * s).sum::<f64>() / n;
		10.0 * (fund / (total - fund).max(1e-12)).log10()
	}

	fn stream_spec(format: pulse::SampleFormat, channels: u8, sample_rate: u32) -> pulse::SampleSpec {
		pulse::SampleSpec {
			format,
			channels,
			sample_rate,
		}
	}

	fn f32_stream(channels: u8, sample_rate: u32) -> pulse::SampleSpec {
		stream_spec(pulse::SampleFormat::Float32Le, channels, sample_rate)
	}

	/// Builds a buffer the way `CreatePlaybackStream` does: passthrough when the
	/// stream already runs at the sink rate, resampling otherwise.
	fn playback(
		stream: pulse::SampleSpec,
		stream_map: pulse::ChannelMap,
		sink_channels: AudioChannels,
	) -> PlaybackBuffer {
		let sink = SinkSpec::new(sink_channels);
		if stream.sample_rate == sink.sample_rate as u32 {
			PlaybackBuffer::passthrough(stream, stream_map, sink)
		} else {
			PlaybackBuffer::resample(stream, stream_map, sink, DEFAULT_CLOCK_RATE_HZ)
		}
	}

	/// Push a 1 kHz tone at `src_rate`/`fmt` through the buffer to the 48 kHz
	/// stereo sink, draining in 240-frame ticks like the audio clock, and return
	/// interleaved stereo output.
	fn resample_tone(src_rate: u32, fmt: pulse::SampleFormat) -> Vec<f32> {
		let mut buf = playback(
			stream_spec(fmt, 2, src_rate),
			pulse::ChannelMap::stereo(),
			AudioChannels::Stereo,
		);

		let mut bytes = Vec::new();
		for i in 0..src_rate * 2 {
			let x = (2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / src_rate as f32).sin() * 0.6;
			for _ in 0..2 {
				match fmt {
					pulse::SampleFormat::S16Le => bytes.extend_from_slice(&((x * 32767.0) as i16).to_le_bytes()),
					_ => bytes.extend_from_slice(&x.to_le_bytes()),
				}
			}
		}
		buf.write(&bytes);

		let vol = [1.0f32, 1.0];
		let mut frame = vec![0.0f32; 240 * 2];
		let mut out = Vec::new();
		loop {
			frame.iter_mut().for_each(|s| *s = 0.0);
			if !buf.drain_and_mix(240, &mut frame, &vol) {
				break;
			}
			out.extend_from_slice(&frame);
		}
		out
	}

	/// Resampling 44.1 kHz → 48 kHz must stay high fidelity. The old dasp Sinc path
	/// collapsed to ~21 dB SNR (audibly robotic in games, especially dialog)
	/// rubato keeps it well above 60 dB. Guards against a resampler-quality regression.
	#[test]
	fn resampling_is_high_fidelity() {
		let out = resample_tone(44100, pulse::SampleFormat::S16Le);
		assert!(out.len() > 20000 * 2, "not enough output produced");
		// Skip resampler startup latency before measuring.
		let snr = snr_db(&out[9600 * 2..], 1000.0, 48000.0);
		assert!(snr > 60.0, "resampled SNR too low: {snr:.1} dB");
	}

	/// Passthrough (matching rate) must be effectively lossless.
	#[test]
	fn passthrough_is_clean() {
		let out = resample_tone(48000, pulse::SampleFormat::Float32Le);
		let snr = snr_db(&out[4800 * 2..], 1000.0, 48000.0);
		assert!(snr > 60.0, "passthrough SNR too low: {snr:.1} dB");
	}

	fn surround51_map() -> pulse::ChannelMap {
		AudioChannels::Surround51.map()
	}

	/// Writes `frames` interleaved frames produced by `frame(i)` as f32le.
	fn write_f32(buf: &mut PlaybackBuffer, frames: usize, frame: impl Fn(usize) -> Vec<f32>) {
		let mut bytes = Vec::new();
		for i in 0..frames {
			for s in frame(i) {
				bytes.extend_from_slice(&s.to_le_bytes());
			}
		}
		buf.write(&bytes);
	}

	/// The clock tick drains every stream into one shared output buffer, so a
	/// stream must add to what earlier streams wrote, and a muted stream must
	/// not silence the others.
	#[test]
	fn streams_mix_into_shared_output() {
		let spec = f32_stream(2, 48000);
		let mut a = playback(spec, pulse::ChannelMap::stereo(), AudioChannels::Stereo);
		let mut b = playback(spec, pulse::ChannelMap::stereo(), AudioChannels::Stereo);
		let mut muted = playback(spec, pulse::ChannelMap::stereo(), AudioChannels::Stereo);
		write_f32(&mut a, 240, |_| vec![0.25, 0.25]);
		write_f32(&mut b, 240, |_| vec![0.25, 0.25]);
		write_f32(&mut muted, 240, |_| vec![0.9, 0.9]);

		let mut out = vec![0.0f32; 240 * 2];
		assert!(a.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		assert!(b.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		assert!(muted.drain_and_mix(240, &mut out, &[0.0, 0.0]));
		assert!(
			out.iter().all(|&s| (s - 0.5).abs() < 1e-6),
			"streams did not sum: {:?}",
			&out[..4]
		);
	}

	/// Resampled streams must also add to the shared output: rubato overwrites
	/// whatever buffer it writes into, so it has to go through per-stream scratch.
	#[test]
	fn resampled_stream_mixes_into_shared_output() {
		let mut native = playback(f32_stream(2, 48000), pulse::ChannelMap::stereo(), AudioChannels::Stereo);
		let mut resampled = playback(f32_stream(2, 44100), pulse::ChannelMap::stereo(), AudioChannels::Stereo);
		write_f32(&mut native, 240, |_| vec![0.25, 0.25]);
		write_f32(&mut resampled, 44100, |_| vec![0.25, 0.25]);

		// Run past the resampler's start-up delay so its output has settled.
		let mut out = vec![0.0f32; 240 * 2];
		for _ in 0..10 {
			out.fill(0.0);
			assert!(resampled.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		}

		out.fill(0.0);
		assert!(native.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		assert!(resampled.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		assert!(
			out.iter().all(|&s| (s - 0.5).abs() < 1e-3),
			"streams did not sum: {:?}",
			&out[..4]
		);
	}

	/// A 5.1 stream played to a stereo client is downmixed, not reinterpreted.
	#[test]
	fn surround_stream_downmixes_to_stereo_output() {
		let mut buf = playback(f32_stream(6, 48000), surround51_map(), AudioChannels::Stereo);
		// Front-left only.
		write_f32(&mut buf, 240, |_| vec![0.5, 0.0, 0.0, 0.0, 0.0, 0.0]);

		let mut out = vec![0.0f32; 240 * 2];
		assert!(buf.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		for frame in out.chunks_exact(2) {
			assert_eq!(frame, [0.5, 0.0]);
		}
		assert!(buf.is_empty());
	}

	/// Layouts other than 5.1/7.1 are downmixed too: a 3.0 stream's centre
	/// (usually dialog) must reach both stereo channels instead of being dropped.
	#[test]
	fn three_channel_stream_downmixes_to_stereo_output() {
		use pulse::ChannelPosition::*;
		let map = pulse::ChannelMap::new([FrontLeft, FrontRight, FrontCenter]);
		let mut buf = playback(f32_stream(3, 48000), map, AudioChannels::Stereo);
		// Centre only.
		write_f32(&mut buf, 240, |_| vec![0.0, 0.0, 0.5]);

		let mut out = vec![0.0f32; 240 * 2];
		assert!(buf.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		for frame in out.chunks_exact(2) {
			assert_eq!(frame, [0.5 * GAIN_CENTER; 2]);
		}
		assert!(buf.is_empty());
	}

	/// A stereo stream played to a 5.1 client lands on FL/FR of every frame.
	#[test]
	fn stereo_stream_upmixes_to_surround_output() {
		let mut buf = playback(
			f32_stream(2, 48000),
			pulse::ChannelMap::stereo(),
			AudioChannels::Surround51,
		);
		write_f32(&mut buf, 240, |_| vec![0.5, 0.25]);

		let mut out = vec![0.0f32; 240 * 6];
		assert!(buf.drain_and_mix(240, &mut out, &[1.0; 6]));
		for frame in out.chunks_exact(6) {
			assert_eq!(frame, [0.5, 0.25, 0.0, 0.0, 0.0, 0.0]);
		}
	}

	/// Mono streams are consumed one sample per frame and fed to both channels.
	#[test]
	fn mono_stream_plays_on_both_channels() {
		let mut buf = playback(f32_stream(1, 48000), pulse::ChannelMap::mono(), AudioChannels::Stereo);
		write_f32(&mut buf, 240, |i| vec![i as f32 / 240.0]);

		let mut out = vec![0.0f32; 240 * 2];
		assert!(buf.drain_and_mix(240, &mut out, &[1.0, 1.0]));
		for (i, frame) in out.chunks_exact(2).enumerate() {
			assert_eq!(frame, [i as f32 / 240.0; 2]);
		}
		assert!(buf.is_empty());
	}

	/// One second of source audio must yield one second of output (200 ticks of
	/// 240 frames at 48 kHz), minus at most a few ticks of resampler latency.
	/// Consuming input faster or slower than its rate causes underruns or drift.
	#[test]
	fn resampling_consumes_input_at_source_rate() {
		for src_rate in [44100u32, 96000] {
			let mut buf = playback(
				f32_stream(2, src_rate),
				pulse::ChannelMap::stereo(),
				AudioChannels::Stereo,
			);
			write_f32(&mut buf, src_rate as usize, |_| vec![0.1, 0.1]);

			let mut out = vec![0.0f32; 240 * 2];
			let mut ticks = 0;
			while buf.drain_and_mix(240, &mut out, &[1.0, 1.0]) {
				ticks += 1;
				assert!(ticks <= 200, "{src_rate} Hz: produced more output than input");
			}
			assert!(ticks >= 197, "{src_rate} Hz: only {ticks}/200 ticks from 1s of input");
		}
	}

	/// Downsampling (96 kHz streams into the 48 kHz sink) must also stay clean.
	#[test]
	fn downsampling_is_high_fidelity() {
		let mut buf = playback(f32_stream(2, 96000), pulse::ChannelMap::stereo(), AudioChannels::Stereo);
		write_f32(&mut buf, 96000 * 2, |i| {
			let x = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 96000.0).sin() * 0.6;
			vec![x, x]
		});

		let mut out = Vec::new();
		let mut frame = vec![0.0f32; 240 * 2];
		loop {
			frame.fill(0.0);
			if !buf.drain_and_mix(240, &mut frame, &[1.0, 1.0]) {
				break;
			}
			out.extend_from_slice(&frame);
		}
		assert!(
			out.len() > 20000 * 2,
			"not enough output produced: {} frames",
			out.len() / 2
		);
		let snr = snr_db(&out[9600 * 2..], 1000.0, 48000.0);
		assert!(snr > 60.0, "downsampled SNR too low: {snr:.1} dB");
	}
}
