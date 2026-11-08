//! A local socket that streams video statistics as they happen, one JSON object per line.
//!
//! Enabled by setting `socket` in the configuration's `[stats]` table. Every connection receives:
//!
//! ```text
//! {"type":"stream_started","generation":1,"stats_version":1,"width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}
//! {"type":"frame","generation":1,"channel_wait":0.000041,"import":0.000028,"convert":0.00019,"submit":0.000035,"consumer_queue":0.000012,"encode_wait":0.00163,"gpu_encode":0.00112,"bitstream_ready":0.00145,"packetize":0.000022,"send":0.000048,"total":0.00201,"encoded_bytes":41666,"is_key_frame":false}
//! {"type":"stream_stopped","generation":1}
//! {"type":"lagged","missed":12}
//! ```
//!
//! A connection starts with the current stream's `stream_started`, or `stream_stopped` naming the
//! last stream when none is in progress. `generation` names the stream a line belongs to and
//! increases with every stream. `lagged` means the reader fell behind and missed that many
//! messages; the current stream's `stream_started` (or `stream_stopped` between streams) follows
//! it. `stream_started` carries `stats_version`, the format the stream's `frame` lines follow. A
//! `frame` line is [`FrameStats`] written directly: its own field names, in declaration order,
//! with every duration as seconds. Absent durations are omitted, which is how `gpu_encode` and
//! `bitstream_ready` appear only when the video queue reports timestamps; both are parts of
//! `encode_wait`, not extra stages.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use crate::ShutdownReason;
use crate::session::manager::SessionManager;
use crate::session::stream::video::{FrameStats, STATS_VERSION, StreamEvent, StreamInfo, StreamState};

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
	StreamStarted {
		generation: u64,
		stats_version: u8,
		#[serde(flatten)]
		info: StreamInfo,
	},
	Frame {
		generation: u64,
		#[serde(flatten)]
		stats: FrameStats,
	},
	StreamStopped {
		generation: u64,
	},
	Lagged {
		missed: u64,
	},
}

impl From<&StreamState> for Line {
	fn from(state: &StreamState) -> Self {
		match state.info {
			Some(info) => Line::from(&StreamEvent::Started {
				generation: state.generation,
				info,
			}),
			None => Line::StreamStopped {
				generation: state.generation,
			},
		}
	}
}

impl From<&StreamEvent> for Line {
	fn from(event: &StreamEvent) -> Self {
		match event {
			StreamEvent::Started { generation, info } => Line::StreamStarted {
				generation: *generation,
				stats_version: STATS_VERSION,
				info: *info,
			},
			StreamEvent::Frame { generation, stats } => Line::Frame {
				generation: *generation,
				stats: stats.clone(),
			},
			StreamEvent::Stopped { generation } => Line::StreamStopped {
				generation: *generation,
			},
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
						// Subscribe before reading the current stream: the pipeline updates the
						// state before broadcasting, so every change reaches this reader one way
						// or the other.
						let events = sessions.bench_stats_receiver();
						let current = sessions.stats_state_receiver();
						tokio::spawn(serve(stream, events, current));
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

/// Which lines one reader gets: the stream it joined at, then every event except those it has in
/// effect been told already.
///
/// The reader's event subscription is made before the state it joins at is read. An event the
/// pipeline broadcast before that read is then already in the state, and one broadcast after is
/// received; a change landing in between arrives both ways, and is written once. Events for a
/// stream older than the one the reader was last told about are dropped.
struct Reader {
	/// The latest stream this reader has been told about, and whether it is still running.
	told: StreamState,
}

impl Reader {
	/// Join at `state`, with the line announcing it.
	fn join(state: StreamState) -> (Self, Line) {
		(Self { told: state }, Line::from(&state))
	}

	fn event(&mut self, event: &StreamEvent) -> Option<Line> {
		let told = self.told;
		let fresh = match *event {
			StreamEvent::Started { generation, info } => {
				let fresh = generation > told.generation;
				if fresh {
					self.told = StreamState {
						generation,
						info: Some(info),
					};
				}
				fresh
			},
			StreamEvent::Frame { generation, .. } => generation == told.generation && told.info.is_some(),
			StreamEvent::Stopped { generation } => {
				let fresh = generation > told.generation || (generation == told.generation && told.info.is_some());
				if fresh {
					self.told = StreamState { generation, info: None };
				}
				fresh
			},
		};
		fresh.then(|| Line::from(event))
	}

	/// The reader fell behind by `missed` events, which may include a stream starting or stopping:
	/// say so, then where things are now.
	fn lagged(&mut self, missed: u64, state: StreamState) -> [Line; 2] {
		self.told = state;
		[Line::Lagged { missed }, Line::from(&state)]
	}
}

/// Write statistics to one connection until it closes, starting with the stream in progress.
/// `events` must be subscribed before `current` is read; see [`Reader`].
async fn serve(
	mut stream: UnixStream,
	mut events: broadcast::Receiver<StreamEvent>,
	current: watch::Receiver<StreamState>,
) {
	let (mut reader, first) = Reader::join(*current.borrow());
	let mut lines = vec![first];
	loop {
		for line in lines.drain(..) {
			if write(&mut stream, &line).await.is_err() {
				return;
			}
		}
		match events.recv().await {
			Ok(event) => lines.extend(reader.event(&event)),
			Err(broadcast::error::RecvError::Lagged(missed)) => lines.extend(reader.lagged(missed, *current.borrow())),
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

	fn stats() -> FrameStats {
		FrameStats {
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

	fn started(generation: u64) -> StreamEvent {
		StreamEvent::Started {
			generation,
			info: info(),
		}
	}

	fn frame(generation: u64) -> StreamEvent {
		StreamEvent::Frame {
			generation,
			stats: stats(),
		}
	}

	fn stopped(generation: u64) -> StreamEvent {
		StreamEvent::Stopped { generation }
	}

	fn live(generation: u64) -> StreamState {
		StreamState {
			generation,
			info: Some(info()),
		}
	}

	fn idle(generation: u64) -> StreamState {
		StreamState { generation, info: None }
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
		let cases = [
			(
				Line::from(&started(1)),
				r#"{"type":"stream_started","generation":1,"stats_version":1,"width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}"#,
			),
			(
				Line::from(&frame(1)),
				r#"{"type":"frame","generation":1,"channel_wait":0.000041,"import":0.000028,"convert":0.00019,"submit":0.000035,"consumer_queue":0.000012,"encode_wait":0.00163,"gpu_encode":0.00112,"bitstream_ready":0.00145,"packetize":0.000022,"send":0.000048,"total":0.00201,"encoded_bytes":41666,"is_key_frame":false}"#,
			),
			// Without GPU video queue timestamps, those two durations are left out.
			(
				Line::Frame {
					generation: 1,
					stats: without_gpu,
				},
				r#"{"type":"frame","generation":1,"channel_wait":0.000041,"import":0.000028,"convert":0.00019,"submit":0.000035,"consumer_queue":0.000012,"encode_wait":0.00163,"packetize":0.000022,"send":0.000048,"total":0.00201,"encoded_bytes":41666,"is_key_frame":false}"#,
			),
			(Line::from(&stopped(1)), r#"{"type":"stream_stopped","generation":1}"#),
			(Line::Lagged { missed: 12 }, r#"{"type":"lagged","missed":12}"#),
		];
		for (line, expected) in cases {
			assert_eq!(json(line), expected);
		}
	}

	/// What a reader is handed after joining.
	enum Step {
		Event(StreamEvent),
		/// The reader fell behind by this many events; the state is what it reads afterwards.
		Lag(u64, StreamState),
	}
	use Step::{Event as E, Lag};

	/// A line as `S`, `F` or `X` (started, frame, stopped) plus its stream's generation, or `L`
	/// plus the number missed.
	fn short(line: &Line) -> String {
		match line {
			Line::StreamStarted { generation, .. } => format!("S{generation}"),
			Line::Frame { generation, .. } => format!("F{generation}"),
			Line::StreamStopped { generation } => format!("X{generation}"),
			Line::Lagged { missed } => format!("L{missed}"),
		}
	}

	#[test]
	fn a_reader_is_told_each_stream_once_and_in_order() {
		let cases: Vec<(&str, StreamState, Vec<Step>, &[&str])> = vec![
			(
				"joins mid-stream",
				live(1),
				vec![E(frame(1)), E(frame(1)), E(stopped(1))],
				&["S1", "F1", "F1", "X1"],
			),
			(
				"joins before any stream",
				idle(0),
				vec![E(started(1)), E(frame(1))],
				&["X0", "S1", "F1"],
			),
			(
				"a stream starts as it joins: the state and the event both say so",
				live(1),
				vec![E(started(1)), E(frame(1))],
				&["S1", "F1"],
			),
			(
				"a stream stops as it joins: its last frame and its end are stale",
				idle(1),
				vec![E(frame(1)), E(stopped(1)), E(started(2))],
				&["X1", "S2"],
			),
			(
				"an older stream's tail arrives after the next one started",
				live(2),
				vec![E(frame(1)), E(stopped(1)), E(frame(2))],
				&["S2", "F2"],
			),
			(
				"falls behind across streams and is re-anchored to the live one",
				live(1),
				vec![E(frame(1)), Lag(7, live(3)), E(frame(3)), E(stopped(3))],
				&["S1", "F1", "L7", "S3", "F3", "X3"],
			),
			(
				"falls behind and a stream ended meanwhile",
				live(1),
				vec![Lag(5, idle(2)), E(frame(2)), E(started(3))],
				&["S1", "L5", "X2", "S3"],
			),
		];
		for (name, joined, steps, expected) in cases {
			let (mut reader, first) = Reader::join(joined);
			let mut lines = vec![first];
			for step in steps {
				match step {
					E(event) => lines.extend(reader.event(&event)),
					Lag(missed, state) => lines.extend(reader.lagged(missed, state)),
				}
			}
			assert_eq!(lines.iter().map(short).collect::<Vec<_>>(), expected, "{name}");
		}
	}

	#[tokio::test]
	async fn a_connection_receives_json_lines_and_recovers_from_falling_behind() {
		// Three frames into room for two: the reader has missed one before it starts. Nothing is
		// sent once it runs, so how far behind it is can't race the test.
		let (events_tx, events) = broadcast::channel(2);
		for _ in 0..3 {
			events_tx.send(frame(1)).unwrap();
		}
		let (_current_tx, current) = watch::channel(live(1));
		let (client, server) = UnixStream::pair().unwrap();
		tokio::spawn(serve(server, events, current));

		let mut lines = BufReader::new(client).lines();
		let mut types = Vec::new();
		for _ in 0..5 {
			let line: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
			types.push(line["type"].as_str().unwrap().to_string());
		}
		assert_eq!(types, ["stream_started", "lagged", "stream_started", "frame", "frame"]);
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
