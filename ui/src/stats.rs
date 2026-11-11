//! Combines per-frame statistics into short buckets for the graphs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::protocol::{FrameSample, StatsEvent, StreamStart};

/// Length of one bucket.
pub(crate) const BUCKET: Duration = Duration::from_millis(250);

/// How many buckets the history keeps: 60 seconds.
const HISTORY: usize = 240;

/// How many buckets the frame rate and bitrate are measured over: 1 second.
///
/// A bucket holds a whole number of frames, so at 60 fps a single bucket reads 56 or 64 fps when
/// one frame lands on the other side of a bucket edge. Over a second, one frame is 1 fps.
const RATE_WINDOW: usize = 4;

/// The frames of one bucket, combined.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Bucket {
	/// Average time of each pipeline stage, in milliseconds.
	pub wait: f32,
	pub import: f32,
	pub convert: f32,
	pub encode: f32,
	pub packetize: f32,
	pub send: f32,
	/// Average total time, in milliseconds.
	pub total: f32,
	/// The frame with the longest total time in the bucket.
	pub slowest: Frame,
	/// Megabits per second, over the last second.
	pub mbps: f32,
	/// Frames per second, over the last second.
	pub fps: f32,
	/// Whether the bucket holds a keyframe.
	pub key: bool,
}

/// The timings of one frame, in milliseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Frame {
	pub wait: f32,
	pub import: f32,
	pub convert: f32,
	pub encode: f32,
	pub packetize: f32,
	pub send: f32,
	pub total: f32,
	/// Size of the encoded frame, in kilobytes.
	pub kb: f32,
	pub key: bool,
}

impl From<&FrameSample> for Frame {
	fn from(frame: &FrameSample) -> Self {
		let ms = |us: u32| us as f32 / 1000.0;
		Self {
			wait: ms(frame.wait_us),
			import: ms(frame.import_us),
			convert: ms(frame.convert_us),
			encode: ms(frame.encode_us),
			packetize: ms(frame.packetize_us),
			send: ms(frame.send_us),
			total: ms(frame.total_us),
			kb: frame.bytes as f32 / 1000.0,
			key: frame.key_frame,
		}
	}
}

/// A message for the browser.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StatsMessage {
	StatsStart { stream: StreamStart },
	Bucket { data: Bucket },
	StatsStop,
}

#[derive(Default)]
struct Accumulator {
	frames: u32,
	wait: u64,
	import: u64,
	convert: u64,
	encode: u64,
	packetize: u64,
	send: u64,
	total: u64,
	slowest: Option<FrameSample>,
	bytes: u64,
	key: bool,
}

impl Accumulator {
	fn add(&mut self, frame: &FrameSample) {
		self.frames += 1;
		self.wait += u64::from(frame.wait_us);
		self.import += u64::from(frame.import_us);
		self.convert += u64::from(frame.convert_us);
		self.encode += u64::from(frame.encode_us);
		self.packetize += u64::from(frame.packetize_us);
		self.send += u64::from(frame.send_us);
		self.total += u64::from(frame.total_us);
		if self.slowest.is_none_or(|slowest| frame.total_us > slowest.total_us) {
			self.slowest = Some(*frame);
		}
		self.bytes += u64::from(frame.bytes);
		self.key |= frame.key_frame;
	}

	/// End the bucket, which lasted `elapsed`, and measure the rates over the last buckets.
	fn take(&mut self, elapsed: Duration, rates: &mut RateWindow) -> Bucket {
		let accumulator = std::mem::take(self);
		let (fps, mbps) = rates.push(accumulator.frames, accumulator.bytes, elapsed);
		let average = |sum: u64| {
			if accumulator.frames == 0 {
				0.0
			} else {
				sum as f32 / accumulator.frames as f32 / 1000.0
			}
		};
		Bucket {
			wait: average(accumulator.wait),
			import: average(accumulator.import),
			convert: average(accumulator.convert),
			encode: average(accumulator.encode),
			packetize: average(accumulator.packetize),
			send: average(accumulator.send),
			total: average(accumulator.total),
			slowest: accumulator.slowest.as_ref().map(Frame::from).unwrap_or_default(),
			mbps,
			fps,
			key: accumulator.key,
		}
	}
}

/// The frames and bytes of the most recent buckets, with how long each bucket really lasted.
#[derive(Default)]
struct RateWindow {
	buckets: VecDeque<(u32, u64, Duration)>,
}

impl RateWindow {
	/// Add a bucket and return the frame rate and the megabits per second over the window.
	fn push(&mut self, frames: u32, bytes: u64, elapsed: Duration) -> (f32, f32) {
		if self.buckets.len() == RATE_WINDOW {
			self.buckets.pop_front();
		}
		self.buckets.push_back((frames, bytes, elapsed));

		let (frames, bytes, elapsed) = self
			.buckets
			.iter()
			.fold((0u64, 0u64, Duration::ZERO), |(f, b, e), (frames, bytes, elapsed)| {
				(f + u64::from(*frames), b + bytes, e + *elapsed)
			});
		let seconds = elapsed.as_secs_f32();
		if seconds <= 0.0 {
			return (0.0, 0.0);
		}
		(frames as f32 / seconds, bytes as f32 * 8.0 / seconds / 1_000_000.0)
	}
}

/// The current stream and the buckets of the last 60 seconds.
#[derive(Clone)]
pub(crate) struct StatsHistory {
	inner: Arc<Mutex<(Option<StreamStart>, VecDeque<Bucket>)>>,
	tx: broadcast::Sender<StatsMessage>,
}

impl StatsHistory {
	pub fn new() -> Self {
		Self {
			inner: Arc::new(Mutex::new((None, VecDeque::with_capacity(HISTORY)))),
			tx: broadcast::channel(64).0,
		}
	}

	/// The current stream and its recent buckets, and a receiver for every message after them.
	pub fn join(&self) -> (Option<StreamStart>, Vec<Bucket>, broadcast::Receiver<StatsMessage>) {
		let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
		let rx = self.tx.subscribe();
		(inner.0.clone(), inner.1.iter().cloned().collect(), rx)
	}

	fn start(&self, stream: StreamStart) {
		let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
		inner.0 = Some(stream.clone());
		inner.1.clear();
		let _ = self.tx.send(StatsMessage::StatsStart { stream });
	}

	fn push(&self, bucket: Bucket) {
		let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
		if inner.1.len() == HISTORY {
			inner.1.pop_front();
		}
		inner.1.push_back(bucket.clone());
		let _ = self.tx.send(StatsMessage::Bucket { data: bucket });
	}

	fn stop(&self) {
		let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
		inner.0 = None;
		let _ = self.tx.send(StatsMessage::StatsStop);
	}

	/// Combine the backend's statistics into buckets until the backend stops sending them.
	pub async fn run(self, current: Option<StreamStart>, mut events: broadcast::Receiver<StatsEvent>) {
		let mut streaming = current.is_some();
		if let Some(stream) = current {
			self.start(stream);
		}
		let mut accumulator = Accumulator::default();
		let mut rates = RateWindow::default();
		let mut tick = tokio::time::interval(BUCKET);
		tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		let mut bucket_start = Instant::now();

		loop {
			tokio::select! {
				_ = tick.tick() => {
					// Divide by the time the bucket really lasted; the timer is not exact.
					let now = Instant::now();
					let elapsed = now - bucket_start;
					bucket_start = now;
					if streaming {
						self.push(accumulator.take(elapsed, &mut rates));
					}
				},
				event = events.recv() => match event {
					Ok(StatsEvent::Started(stream)) => {
						accumulator = Accumulator::default();
						rates = RateWindow::default();
						bucket_start = Instant::now();
						streaming = true;
						self.start(stream);
					},
					Ok(StatsEvent::Frame(frame)) => accumulator.add(&frame),
					Ok(StatsEvent::Stopped) => {
						streaming = false;
						self.stop();
					},
					Err(broadcast::error::RecvError::Lagged(_)) => {},
					Err(broadcast::error::RecvError::Closed) => break,
				},
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_frame_across_a_bucket_edge_moves_the_rate_by_one_fps() {
		let mut rates = RateWindow::default();
		// 60 fps: buckets of 15 frames, but one frame lands in the next bucket.
		for frames in [15, 15, 16, 14] {
			rates.push(frames, 0, BUCKET);
		}
		let (fps, _) = rates.push(15, 0, BUCKET);
		assert!((fps - 60.0).abs() < 0.01, "{fps}");

		let mut rates = RateWindow::default();
		for _ in 0..3 {
			rates.push(15, 0, BUCKET);
		}
		let (fps, _) = rates.push(16, 0, BUCKET);
		assert!((fps - 61.0).abs() < 0.01, "{fps}");
	}

	#[test]
	fn rates_use_the_real_bucket_length() {
		let mut rates = RateWindow::default();
		let (fps, mbps) = rates.push(18, 1_250_000, Duration::from_millis(300));
		assert!((fps - 60.0).abs() < 0.01, "{fps}");
		assert!((mbps - 33.333).abs() < 0.01, "{mbps}");
	}

	#[test]
	fn buckets_average_the_stage_times() {
		let mut accumulator = Accumulator::default();
		for total_us in [2000, 4000, 3000] {
			accumulator.add(&FrameSample {
				encode_us: total_us / 2,
				total_us,
				bytes: 1000,
				..Default::default()
			});
		}
		let bucket = accumulator.take(BUCKET, &mut RateWindow::default());
		assert!((bucket.total - 3.0).abs() < 0.001);
		assert!((bucket.encode - 1.5).abs() < 0.001);
		assert!((bucket.slowest.total - 4.0).abs() < 0.001);
		assert!((bucket.slowest.encode - 2.0).abs() < 0.001);
		assert!((bucket.fps - 12.0).abs() < 0.001);
	}
}
