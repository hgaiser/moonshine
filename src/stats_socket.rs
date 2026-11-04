//! A local socket that streams video statistics as they happen, one JSON object per line.
//!
//! Enabled by setting `socket` in the configuration's `[stats]` table. Every connection receives:
//!
//! ```text
//! {"type":"stream_started","width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}
//! {"type":"frame","frame_number":42,"captured_unix_us":1793750400000000,"content":"sdr","channel_wait_us":41,"import_us":28,"convert_us":190,"submit_us":35,"consumer_queue_us":12,"encode_wait_us":1630,"packetize_us":22,"send_us":48,"total_us":2010,"encoded_bytes":41666,"key_frame":false}
//! {"type":"stream_stopped"}
//! {"type":"lagged","missed":12}
//! ```
//!
//! A connection made during a stream starts with that stream's `stream_started`. `lagged` means
//! the reader fell behind and missed that many messages; the current stream's `stream_started`
//! (or `stream_stopped` between streams) follows it. The fields of `frame` are those of
//! [`FrameStats`], in microseconds: `frame_number` is the frame index the client receives, so a
//! reader can match the two sides' records of a frame, and `captured_unix_us` is when the frame was
//! captured, in microseconds since the Unix epoch. `content` is the captured content's color space (`sdr`, `hdr10`
//! or `scrgb`), absent on a keyframe re-encoded without a new capture, so a reader can count the stream's switches
//! between them.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use crate::ShutdownReason;
use crate::session::compositor::frame::FrameColorSpace;
use crate::session::manager::SessionManager;
use crate::session::stream::video::{FrameStats, StreamEvent, StreamInfo};

/// The `[stats]` table of the configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsConfig {
	/// Unix socket to stream per-frame video statistics on, as JSON lines. Off when unset.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub socket: Option<PathBuf>,
}

impl StatsConfig {
	pub(crate) fn is_default(&self) -> bool {
		*self == Self::default()
	}
}

/// One line on the socket.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
	StreamStarted(StreamInfo),
	Frame(Frame),
	StreamStopped,
	Lagged { missed: u64 },
}

impl From<&StreamEvent> for Line {
	fn from(event: &StreamEvent) -> Self {
		match event {
			StreamEvent::Started(info) => Line::StreamStarted(*info),
			StreamEvent::Frame(stats) => Line::Frame(Frame::from(stats)),
			StreamEvent::Stopped => Line::StreamStopped,
		}
	}
}

/// [`FrameStats`] as written: whole microseconds, with the unit in each name.
#[derive(Debug, PartialEq, Serialize)]
struct Frame {
	frame_number: u32,
	captured_unix_us: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	content: Option<&'static str>,
	channel_wait_us: u64,
	import_us: u64,
	convert_us: u64,
	submit_us: u64,
	consumer_queue_us: u64,
	encode_wait_us: u64,
	packetize_us: u64,
	send_us: u64,
	total_us: u64,
	encoded_bytes: usize,
	key_frame: bool,
}

impl From<&FrameStats> for Frame {
	fn from(stats: &FrameStats) -> Self {
		fn us(d: Duration) -> u64 {
			u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
		}
		Frame {
			frame_number: stats.frame_number,
			content: stats.content.map(|c| match c {
				FrameColorSpace::Srgb => "sdr",
				FrameColorSpace::Bt2020Pq => "hdr10",
				FrameColorSpace::ScrgbLinear => "scrgb",
			}),
			captured_unix_us: stats.captured_at.duration_since(std::time::UNIX_EPOCH).map_or(0, us),
			channel_wait_us: us(stats.channel_wait),
			import_us: us(stats.import),
			convert_us: us(stats.convert),
			submit_us: us(stats.submit),
			consumer_queue_us: us(stats.consumer_queue),
			encode_wait_us: us(stats.encode_wait),
			packetize_us: us(stats.packetize),
			send_us: us(stats.send),
			total_us: us(stats.total),
			encoded_bytes: stats.encoded_bytes,
			key_frame: stats.is_key_frame,
		}
	}
}

/// Listen on `path` and stream the session manager's statistics to every connection until
/// shutdown. The socket is only accessible to the user running Moonshine.
pub fn spawn(path: PathBuf, sessions: &SessionManager, shutdown: ShutdownManager<ShutdownReason>) -> Result<(), ()> {
	let listener = bind(&path)?;
	tracing::info!("Streaming video statistics on {}.", path.display());

	// The stream in progress, for connections made during it.
	let (current_tx, current_rx) = watch::channel(None);
	let mut events = sessions.bench_stats_receiver();
	tokio::spawn(async move {
		loop {
			match events.recv().await {
				Ok(StreamEvent::Started(info)) => current_tx.send_replace(Some(info)),
				Ok(StreamEvent::Stopped) => current_tx.send_replace(None),
				Ok(StreamEvent::Frame(_)) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
				Err(broadcast::error::RecvError::Closed) => break,
			};
		}
	});

	let sessions = sessions.clone();
	tokio::spawn(async move {
		let accept = async {
			loop {
				match listener.accept().await {
					Ok((stream, _)) => {
						// Subscribe before reading the current stream, so nothing falls in between.
						let events = sessions.bench_stats_receiver();
						tokio::spawn(serve(stream, events, current_rx.clone()));
					},
					Err(e) => {
						tracing::warn!("Failed to accept a statistics connection: {e}");
						tokio::time::sleep(Duration::from_secs(1)).await;
					},
				}
			}
		};
		let _ = shutdown.wrap_cancel(accept).await;
		let _ = std::fs::remove_file(&path);
	});
	Ok(())
}

/// Bind the socket, replacing one left by an earlier run, and make it private to this user.
fn bind(path: &Path) -> Result<UnixListener, ()> {
	if let Some(parent) = path.parent()
		&& !parent.as_os_str().is_empty()
		&& !parent.exists()
	{
		std::fs::DirBuilder::new()
			.recursive(true)
			.mode(0o700)
			.create(parent)
			.map_err(|e| tracing::error!("Failed to create {}: {e}", parent.display()))?;
	}
	match std::fs::symlink_metadata(path) {
		Ok(meta) if meta.file_type().is_socket() => {
			std::fs::remove_file(path)
				.map_err(|e| tracing::error!("Failed to remove the old statistics socket {}: {e}", path.display()))?;
		},
		Ok(_) => {
			tracing::error!(
				"Can't create the statistics socket: {} exists and isn't a socket.",
				path.display()
			);
			return Err(());
		},
		Err(_) => {},
	}
	let listener = UnixListener::bind(path)
		.map_err(|e| tracing::error!("Failed to create the statistics socket {}: {e}", path.display()))?;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
		.map_err(|e| tracing::error!("Failed to restrict the statistics socket {}: {e}", path.display()))?;
	Ok(listener)
}

/// Write statistics to one connection until it closes, starting with the stream in progress.
async fn serve(
	mut stream: UnixStream,
	mut events: broadcast::Receiver<StreamEvent>,
	current: watch::Receiver<Option<StreamInfo>>,
) {
	let mut lines: Vec<Line> = current.borrow().map(Line::StreamStarted).into_iter().collect();
	loop {
		for line in lines.drain(..) {
			if write(&mut stream, &line).await.is_err() {
				return;
			}
		}
		match events.recv().await {
			Ok(event) => lines.push(Line::from(&event)),
			Err(broadcast::error::RecvError::Lagged(missed)) => {
				lines.push(Line::Lagged { missed });
				// What was missed may include a stream starting or stopping: say where things are now.
				lines.push(current.borrow().map_or(Line::StreamStopped, Line::StreamStarted));
			},
			Err(broadcast::error::RecvError::Closed) => return,
		}
	}
}

async fn write(stream: &mut UnixStream, line: &Line) -> std::io::Result<()> {
	let mut buf = serde_json::to_vec(line).map_err(std::io::Error::other)?;
	buf.push(b'\n');
	stream.write_all(&buf).await
}

#[cfg(test)]
mod tests {
	use tokio::io::{AsyncBufReadExt, BufReader};

	use super::*;
	use crate::session::stream::video::{VideoChromaSampling, VideoDynamicRange, VideoFormat};

	fn info() -> StreamInfo {
		StreamInfo {
			width: 1920,
			height: 1080,
			fps: 60,
			bitrate: 20_000_000,
			video_format: VideoFormat::Hevc,
			dynamic_range: VideoDynamicRange::Sdr,
			chroma_sampling_type: VideoChromaSampling::Yuv420,
		}
	}

	fn frame(total_us: u64) -> FrameStats {
		FrameStats {
			frame_number: 42,
			content: Some(FrameColorSpace::Srgb),
			captured_at: std::time::UNIX_EPOCH + Duration::from_micros(1_793_750_400_000_000),
			channel_wait: Duration::from_micros(41),
			import: Duration::from_micros(28),
			convert: Duration::from_micros(190),
			submit: Duration::from_micros(35),
			consumer_queue: Duration::from_micros(12),
			encode_wait: Duration::from_micros(1630),
			packetize: Duration::from_micros(22),
			send: Duration::from_micros(48),
			total: Duration::from_micros(total_us),
			encoded_bytes: 41666,
			is_key_frame: false,
		}
	}

	#[test]
	fn configured_in_a_stats_table_and_absent_by_default() {
		let config: crate::config::Config =
			toml::from_str("[stats]\nsocket = \"/run/user/1000/stats.sock\"\n").unwrap();
		assert_eq!(config.stats.socket, Some(PathBuf::from("/run/user/1000/stats.sock")));
		let default = toml::to_string_pretty(&crate::config::Config::default()).unwrap();
		assert!(!default.contains("[stats]"));
	}

	#[test]
	fn lines_match_the_documented_format() {
		let started = serde_json::to_string(&Line::from(&StreamEvent::Started(info()))).unwrap();
		assert_eq!(
			started,
			r#"{"type":"stream_started","width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}"#
		);
		let frame = serde_json::to_string(&Line::from(&StreamEvent::Frame(frame(2010)))).unwrap();
		assert_eq!(
			frame,
			r#"{"type":"frame","frame_number":42,"captured_unix_us":1793750400000000,"content":"sdr","channel_wait_us":41,"import_us":28,"convert_us":190,"submit_us":35,"consumer_queue_us":12,"encode_wait_us":1630,"packetize_us":22,"send_us":48,"total_us":2010,"encoded_bytes":41666,"key_frame":false}"#
		);
		assert_eq!(
			serde_json::to_string(&Line::StreamStopped).unwrap(),
			r#"{"type":"stream_stopped"}"#
		);
		assert_eq!(
			serde_json::to_string(&Line::Lagged { missed: 12 }).unwrap(),
			r#"{"type":"lagged","missed":12}"#
		);
	}

	/// Each line's type, with `total_us` for frames.
	async fn read_types(client: UnixStream, n: usize) -> Vec<String> {
		let mut lines = BufReader::new(client).lines();
		let mut types = Vec::new();
		for _ in 0..n {
			let line = lines.next_line().await.unwrap().unwrap();
			let value: serde_json::Value = serde_json::from_str(&line).unwrap();
			let total = value.get("total_us").map(|v| v.to_string()).unwrap_or_default();
			types.push(format!("{}{total}", value["type"].as_str().unwrap()));
		}
		types
	}

	#[tokio::test]
	async fn a_connection_gets_the_current_stream_then_its_frames_in_order() {
		let (events_tx, events) = broadcast::channel(16);
		let (_current_tx, current) = watch::channel(Some(info()));
		let (client, server) = UnixStream::pair().unwrap();
		tokio::spawn(serve(server, events, current));

		events_tx.send(StreamEvent::Frame(frame(1))).unwrap();
		events_tx.send(StreamEvent::Frame(frame(2))).unwrap();
		events_tx.send(StreamEvent::Stopped).unwrap();

		assert_eq!(
			read_types(client, 4).await,
			["stream_started", "frame1", "frame2", "stream_stopped"]
		);
	}

	#[tokio::test]
	async fn after_falling_behind_a_connection_is_told_the_current_stream() {
		let (events_tx, events) = broadcast::channel(2);
		let (_current_tx, current) = watch::channel(Some(info()));
		// Five frames into room for two: the reader has missed three before it starts.
		for i in 1..=5 {
			events_tx.send(StreamEvent::Frame(frame(i))).unwrap();
		}
		let (client, server) = UnixStream::pair().unwrap();
		tokio::spawn(serve(server, events, current));

		assert_eq!(
			read_types(client, 5).await,
			["stream_started", "lagged", "stream_started", "frame4", "frame5"]
		);
	}

	#[test]
	fn bind_replaces_a_stale_socket_and_refuses_other_files() {
		let dir = tempfile::tempdir().unwrap();
		// Its directory doesn't exist yet.
		let path = dir.path().join("moonshine").join("stats.sock");
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		runtime.block_on(async {
			drop(bind(&path).unwrap());
			// The first listener's socket file is still there, as after a crash.
			let _listener = bind(&path).unwrap();
			let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
			assert_eq!(mode, 0o600);

			let file = dir.path().join("not-a-socket");
			std::fs::write(&file, "").unwrap();
			assert!(bind(&file).is_err());
		});
	}
}
