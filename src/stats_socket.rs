//! A local socket that streams video statistics as they happen, one JSON object per line.
//!
//! Enabled by setting `socket` in the configuration's `[stats]` table. Every connection receives:
//!
//! ```text
//! {"type":"stream_started","stats_version":1,"width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}
//! {"type":"frame","frame_number":42,"captured_at":1793750400500000,"content":"srgb","channel_wait":41,"import":28,"convert":190,"submit":35,"consumer_queue":12,"encode_wait":1630,"gpu_encode":1120,"bitstream_ready":1450,"packetize":22,"send":48,"total":2010,"encoded_bytes":41666,"is_key_frame":false}
//! {"type":"stream_stopped"}
//! {"type":"lagged","missed":12}
//! ```
//!
//! A connection made during a stream starts with that stream's `stream_started`; after that it
//! gets every event as it happens. `lagged` means the reader fell behind and missed that many
//! messages, which may include a stream's start or end. `stream_started` carries `stats_version`,
//! the format the stream's `frame` lines follow. A `frame` line is [`FrameStats`] written directly:
//! its own field names, in declaration order, with every duration as whole microseconds.
//! `frame_number` is the frame index the client receives, so a reader can match the two sides'
//! records of a frame; `captured_at` is when the frame was captured, in microseconds since the Unix
//! epoch; `content` is the captured content's color space, absent on a keyframe re-encoded without
//! a new capture. Absent durations are omitted, which is how `gpu_encode` and `bitstream_ready`
//! appear only when the video queue reports timestamps; both are parts of `encode_wait`, not extra
//! stages.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use crate::ShutdownReason;
use crate::session::manager::SessionManager;
use crate::session::stream::video::{FrameStats, STATS_VERSION, StreamEvent, StreamInfo};

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
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
	StreamStarted {
		stats_version: u8,
		#[serde(flatten)]
		info: StreamInfo,
	},
	Frame(FrameStats),
	StreamStopped,
	Lagged {
		missed: u64,
	},
}

impl Line {
	fn started(info: StreamInfo) -> Self {
		Line::StreamStarted {
			stats_version: STATS_VERSION,
			info,
		}
	}
}

impl From<StreamEvent> for Line {
	fn from(event: StreamEvent) -> Self {
		match event {
			StreamEvent::Started(info) => Line::started(info),
			StreamEvent::Frame(stats) => Line::Frame(stats),
			StreamEvent::Stopped => Line::StreamStopped,
		}
	}
}

/// Listen on `path` and stream the session manager's statistics to every connection until
/// shutdown. The socket is only accessible to the user running Moonshine.
pub fn spawn(path: PathBuf, sessions: &SessionManager, shutdown: ShutdownManager<ShutdownReason>) -> Result<(), ()> {
	let BoundSocket { listener, dev, ino } = bind(&path)?;
	tracing::info!("Streaming video statistics on {}.", path.display());
	let sessions = sessions.clone();
	tokio::spawn(async move {
		let accept = async {
			loop {
				match listener.accept().await {
					Ok((stream, _)) => {
						let (current, events) = sessions.join_stats();
						tokio::spawn(serve(stream, current, events));
					},
					Err(e) => {
						tracing::warn!("Failed to accept a statistics connection: {e}");
						tokio::time::sleep(Duration::from_secs(1)).await;
					},
				}
			}
		};
		let _ = shutdown.wrap_cancel(accept).await;
		// Only unlink the path if it is still the socket we bound. Another instance's socket at
		// the same path is not ours to remove. This happens while the listener is still open: it
		// keeps our socket file's inode allocated, so no other file can have its number yet.
		if owns(&path, dev, ino) {
			let _ = std::fs::remove_file(&path);
		}
		drop(listener);
	});
	Ok(())
}

/// A statistics socket this instance bound, and the file it is bound to.
struct BoundSocket {
	listener: UnixListener,
	/// The device and inode of the socket file, so cleanup can tell this socket from a later
	/// one bound at the same path by someone else.
	dev: u64,
	ino: u64,
}

/// Whether `path` is the socket file this instance bound, identified by the device and inode
/// recorded when it was created. Only meaningful while our listener is open: once it closes, the
/// inode can be freed and its number given to a new file, as ext4 readily does.
fn owns(path: &Path, dev: u64, ino: u64) -> bool {
	std::fs::symlink_metadata(path).is_ok_and(|meta| meta.dev() == dev && meta.ino() == ino)
}

/// Bind the socket, replacing a stale one left by an ended run, and make it private to this user.
///
/// Refuses a path another instance is listening on: unlinking that would cut its readers off, and
/// the two instances' cleanup would then remove each other's socket.
fn bind(path: &Path) -> Result<BoundSocket, ()> {
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
			if listener_is_live(path) {
				tracing::error!(
					"Can't create the statistics socket: {} is in use by a running instance. Give one of the two instances its own path.",
					path.display()
				);
				return Err(());
			}
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
	let meta = std::fs::metadata(path)
		.map_err(|e| tracing::error!("Failed to identify the statistics socket {}: {e}", path.display()))?;
	Ok(BoundSocket {
		listener,
		dev: meta.dev(),
		ino: meta.ino(),
	})
}

/// Whether something is listening on `path`. A live listener answers a connection; a socket file
/// left by a run that ended refuses one.
///
/// The probe is non-blocking so a listener with a full accept queue can't stall us; anything other
/// than a clean refusal is treated as live, which is the safe answer.
fn listener_is_live(path: &Path) -> bool {
	let probe = || -> std::io::Result<()> {
		let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
		socket.set_nonblocking(true)?;
		socket.connect(&socket2::SockAddr::unix(path)?)
	};
	match probe() {
		Ok(()) => true,
		Err(e) => !matches!(
			e.kind(),
			std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
		),
	}
}

/// Write statistics to one connection until it closes: the stream in progress, if any, then
/// every event after it.
async fn serve(mut stream: UnixStream, current: Option<StreamInfo>, mut events: broadcast::Receiver<StreamEvent>) {
	if let Some(info) = current
		&& write(&mut stream, &Line::started(info)).await.is_err()
	{
		return;
	}
	loop {
		let line = match events.recv().await {
			Ok(event) => Line::from(event),
			Err(broadcast::error::RecvError::Lagged(missed)) => Line::Lagged { missed },
			Err(broadcast::error::RecvError::Closed) => return,
		};
		if write(&mut stream, &line).await.is_err() {
			return;
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
	use crate::session::stream::video::{StatsSender, VideoChromaSampling, VideoDynamicRange, VideoFormat};

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

	fn stats() -> FrameStats {
		FrameStats {
			frame_number: 42,
			captured_at: std::time::UNIX_EPOCH + Duration::from_millis(1_793_750_400_500),
			content: Some(crate::session::stream::video::FrameColorSpace::Srgb),
			channel_wait: Duration::from_micros(41),
			import: Duration::from_micros(28),
			convert: Duration::from_micros(190),
			submit: Duration::from_micros(35),
			consumer_queue: Duration::from_micros(12),
			encode_wait: Duration::from_micros(1630),
			gpu_encode: Some(Duration::from_micros(1120)),
			bitstream_ready: Some(Duration::from_micros(1450)),
			packetize: Duration::from_micros(22),
			send: Duration::from_micros(48),
			total: Duration::from_micros(2010),
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
		let json = |line: Line| serde_json::to_string(&line).unwrap();
		let mut without_gpu = stats();
		without_gpu.gpu_encode = None;
		without_gpu.bitstream_ready = None;
		without_gpu.content = None;
		let cases = [
			(
				Line::started(info()),
				r#"{"type":"stream_started","stats_version":1,"width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}"#,
			),
			(
				Line::Frame(stats()),
				r#"{"type":"frame","frame_number":42,"captured_at":1793750400500000,"content":"srgb","channel_wait":41,"import":28,"convert":190,"submit":35,"consumer_queue":12,"encode_wait":1630,"gpu_encode":1120,"bitstream_ready":1450,"packetize":22,"send":48,"total":2010,"encoded_bytes":41666,"is_key_frame":false}"#,
			),
			// Without GPU video queue timestamps those two durations are left out, as is the content
			// of a keyframe re-encoded without a new capture.
			(
				Line::Frame(without_gpu),
				r#"{"type":"frame","frame_number":42,"captured_at":1793750400500000,"channel_wait":41,"import":28,"convert":190,"submit":35,"consumer_queue":12,"encode_wait":1630,"packetize":22,"send":48,"total":2010,"encoded_bytes":41666,"is_key_frame":false}"#,
			),
			(Line::StreamStopped, r#"{"type":"stream_stopped"}"#),
			(Line::Lagged { missed: 12 }, r#"{"type":"lagged","missed":12}"#),
		];
		for (line, expected) in cases {
			assert_eq!(json(line), expected);
		}
	}

	#[test]
	fn a_reader_is_told_of_a_start_either_on_joining_or_as_an_event_never_both() {
		let sender = StatsSender::new(4);
		let (current, mut before) = sender.join();
		assert_eq!(current, None);
		sender.started(info());
		let (current, mut during) = sender.join();
		assert_eq!(current, Some(info()));
		assert!(matches!(before.try_recv(), Ok(StreamEvent::Started(_))));
		assert!(during.try_recv().is_err());

		sender.stopped();
		let (current, mut after) = sender.join();
		assert_eq!(current, None);
		assert!(matches!(during.try_recv(), Ok(StreamEvent::Stopped)));
		assert!(after.try_recv().is_err());
	}

	#[tokio::test]
	async fn a_connection_gets_the_stream_in_progress_then_its_events() {
		// Three events into room for two: the reader has missed one before it starts. Nothing is
		// sent once it runs, so how far behind it is can't race the test.
		let sender = StatsSender::new(2);
		sender.started(info());
		let (current, events) = sender.join();
		sender.frame(stats());
		sender.frame(stats());
		sender.stopped();
		let (client, server) = UnixStream::pair().unwrap();
		tokio::spawn(serve(server, current, events));

		let mut lines = BufReader::new(client).lines();
		let mut types = Vec::new();
		for _ in 0..4 {
			let line: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
			types.push(line["type"].as_str().unwrap().to_string());
		}
		assert_eq!(types, ["stream_started", "lagged", "frame", "stream_stopped"]);
	}

	#[tokio::test]
	async fn a_socket_path_is_shared_safely_between_instances() {
		let dir = tempfile::tempdir().unwrap();
		// Its directory doesn't exist yet.
		let path = dir.path().join("moonshine").join("stats.sock");

		let first = bind(&path).unwrap();
		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
		// A second instance would cut the first one's readers off mid-stream.
		assert!(bind(&path).is_err());
		assert!(owns(&path, first.dev, first.ino));

		// The path is removed from under the first instance and a second one binds it. The first
		// instance's shutdown, which checks before closing its listener, must leave it alone.
		std::fs::remove_file(&path).unwrap();
		let second = bind(&path).unwrap();
		assert!(!owns(&path, first.dev, first.ino));
		assert!(owns(&path, second.dev, second.ino));

		// Once no listener is left the file is stale, as after a crash, and the next run replaces it.
		drop(first);
		drop(second);
		assert!(bind(&path).is_ok());

		let file = dir.path().join("not-a-socket");
		std::fs::write(&file, "").unwrap();
		assert!(bind(&file).is_err());
	}
}
