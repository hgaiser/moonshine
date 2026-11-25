//! A browser as the client of a session, through the control panel.
//!
//! A session has one encoder. A browser attaches to the session: the video and audio sinks switch
//! to the browser's channels, and the browser's input goes to the session's input handler. When the
//! browser goes away, it detaches, and the session stays until `stream_timeout` passes without a
//! client, as it does for Moonlight. A Moonlight client that resumes the session takes it over
//! again, and the other way around.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_shutdown::ShutdownManager;
use tokio::sync::{Notify, mpsc};

use crate::session::manager::SessionShutdownReason;
use crate::session::stream::audio::{AudioSink, OpusStreamConfig};
use crate::session::stream::video::{EncodedFrame, VideoFormat, VideoSink};
use crate::session::{ActiveSession, SessionControls};

/// What the browser sends.
#[derive(Debug)]
pub enum BrowserCommand {
	/// The browser is ready for the stream (`StartB` in GameStream).
	Start,
	/// A Moonlight input packet: the event type (`u32`, little-endian) and its payload.
	Input(Vec<u8>),
	/// The browser lost a frame and needs a key frame.
	RequestKeyFrame,
	/// The user stopped the session.
	Stop,
}

/// What the session sends to the browser, besides video and audio.
#[derive(Debug)]
pub enum BrowserEvent {
	/// A Moonlight feedback packet (rumble, LED, trigger effects).
	Feedback(Vec<u8>),
	/// The stream ended. Holds the reason, for the user.
	Ended(String),
}

/// A browser's stream of a session.
pub struct BrowserStream {
	pub video_format: VideoFormat,
	/// Whether the video is 10-bit HDR.
	pub hdr: bool,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	/// Video bitrate in bits per second.
	pub bitrate: usize,
	/// The Opus multistream layout; its mapping is in the sink's channel order (WAVE: FL FR FC LFE
	/// RL RR SL SR).
	pub audio_layout: OpusStreamConfig,
	pub audio_sample_rate: u32,
	pub video: mpsc::Receiver<EncodedFrame>,
	/// Opus packets.
	pub audio: mpsc::Receiver<Vec<u8>>,
	pub events: mpsc::Receiver<BrowserEvent>,
	/// Dropping this sender detaches the browser.
	pub commands: mpsc::Sender<BrowserCommand>,
}

/// The browser that is attached to an active session.
pub(crate) struct Attachment {
	/// Tells the browser's task that another client took over.
	detach: Arc<Notify>,
}

/// Frames that wait for the browser's sender. Few: the pipeline drops captures instead.
const VIDEO_QUEUE: usize = 2;

/// Opus packets that wait for the browser's sender (10 ms each).
const AUDIO_QUEUE: usize = 16;

/// How often an attached browser keeps the session alive.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

impl ActiveSession {
	/// Attach a browser that can decode `codecs` (and `codecs_10bit` in 10 bits). A browser that
	/// is attached already, or the Moonlight client, loses the stream.
	pub(crate) fn attach_browser(
		&mut self,
		codecs: &[VideoFormat],
		codecs_10bit: &[VideoFormat],
		bitrate: usize,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<BrowserStream, String> {
		let decodable = if self.ten_bit { codecs_10bit } else { codecs };
		if !decodable.contains(&self.video_format) {
			return Err(format!(
				"The session streams {}{:?}, which this browser cannot decode. Stop the session and start it from the browser.",
				if self.ten_bit { "10-bit HDR " } else { "" },
				self.video_format
			));
		}

		if let Some(previous) = self.browser.take() {
			previous.detach.notify_one();
		}
		let generation = self.attachment.fetch_add(1, Ordering::Relaxed) + 1;

		let (video_tx, video) = mpsc::channel(VIDEO_QUEUE);
		let (audio_tx, audio) = mpsc::channel(AUDIO_QUEUE);
		let (events_tx, events) = mpsc::channel(32);
		let (commands, commands_rx) = mpsc::channel(256);

		self.controls.kick_gamestream.store(true, Ordering::Relaxed);
		self.controls.keepalive.touch();
		self.controls.audio.set_sink(AudioSink::Browser(audio_tx));
		self.controls.video.set_sink(VideoSink::Browser(video_tx));

		let detach = Arc::new(Notify::new());
		tokio::spawn(run_browser_client(
			commands_rx,
			events_tx,
			self.controls.clone(),
			detach.clone(),
			self.attachment.clone(),
			generation,
			stop,
		));
		self.browser = Some(Attachment { detach });

		tracing::info!("A browser attached to the session.");
		Ok(BrowserStream {
			video_format: self.video_format,
			width: self.context.resolution.0,
			height: self.context.resolution.1,
			fps: self.context.refresh_rate,
			bitrate,
			hdr: self.ten_bit,
			audio_layout: self.audio_layout.clone(),
			audio_sample_rate: 48_000,
			video,
			audio,
			events,
			commands,
		})
	}

	/// A Moonlight client resumes the session: an attached browser loses the stream.
	pub(crate) fn attach_gamestream(&mut self) {
		if let Some(browser) = self.browser.take() {
			tracing::info!("A Moonlight client took over the session from the browser.");
			browser.detach.notify_one();
			self.attachment.fetch_add(1, Ordering::Relaxed);
			self.controls.audio.set_sink(AudioSink::GameStream);
			self.controls.video.set_sink(VideoSink::GameStream);
		}
	}
}

/// Drive the session for one browser until it detaches, another client takes over or the session
/// ends.
async fn run_browser_client(
	mut commands: mpsc::Receiver<BrowserCommand>,
	events: mpsc::Sender<BrowserEvent>,
	controls: SessionControls,
	detach: Arc<Notify>,
	attachment: Arc<AtomicU64>,
	generation: u64,
	stop: ShutdownManager<SessionShutdownReason>,
) {
	let (feedback_tx, mut feedback_rx) = mpsc::channel(16);
	let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);

	loop {
		tokio::select! {
			_ = stop.wait_shutdown_triggered() => {
				let _ = events.try_send(BrowserEvent::Ended("The session ended.".to_string()));
				return;
			},
			_ = detach.notified() => {
				let _ = events.try_send(BrowserEvent::Ended("Another client took over the session.".to_string()));
				return;
			},
			_ = keepalive.tick() => controls.keepalive.touch(),
			command = commands.recv() => match command {
				Some(BrowserCommand::Start) => {
					controls.video.trigger();
					controls.audio_start.trigger();
				},
				Some(BrowserCommand::Input(event)) => {
					let _ = controls.input.handle_raw_input(&event, feedback_tx.clone()).await;
				},
				Some(BrowserCommand::RequestKeyFrame) => controls.video.request_idr_frame(),
				Some(BrowserCommand::Stop) => {
					tracing::info!("The browser stopped the session.");
					let _ = stop.trigger_shutdown(SessionShutdownReason::UserStopped);
					return;
				},
				None => break,
			},
			Some(feedback) = feedback_rx.recv() => {
				let _ = events.try_send(BrowserEvent::Feedback(feedback.as_packet()));
			},
		}
	}

	// The browser went away. Unless another client took over meanwhile, the frames go back to
	// GameStream, where they wait for a Moonlight client, and the session stops after
	// `stream_timeout` without one.
	if attachment.load(Ordering::Relaxed) == generation {
		tracing::info!("The browser detached from the session.");
		controls.audio.set_sink(AudioSink::GameStream);
		controls.video.set_sink(VideoSink::GameStream);
	}
}
