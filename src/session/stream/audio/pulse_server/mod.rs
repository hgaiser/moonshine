mod commands;
mod dyn_buffer;

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time;

use bytes::BytesMut;
use mio::net::UnixListener;
use pulseaudio::protocol::{self as pulse};

use dyn_buffer::DynPlaybackBuffer;

type Error = Box<dyn std::error::Error + Send + Sync>;

const WAKER: mio::Token = mio::Token(0);
const LISTENER: mio::Token = mio::Token(1);
const CLOCK: mio::Token = mio::Token(2);

/// The server emits samples at this rate to the encoder.
pub const CAPTURE_SAMPLE_RATE: u32 = 48000;

/// Clock tick rate — determines audio frame size sent to the encoder.
/// For 5ms frames: 200 Hz; for 10ms frames: 100 Hz.
const DEFAULT_CLOCK_RATE_HZ: u32 = 200;

const SINK_NAME: &str = "moonshine";

/// Pre-allocated zero-volume slice for muted streams (up to 8 channels).
const ZERO_VOL: [f32; 8] = [0.0; 8];

/// A buffer of interleaved f32 samples ready for Opus encoding.
pub struct AudioFrame {
	/// Interleaved f32 samples for the negotiated channel count.
	pub buf: Vec<f32>,

	/// Capture timestamp in milliseconds since process start.
	pub capture_ts_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
	Prebuffering(u64),
	Corked,
	Playing,
	Draining(u32),
}

struct PlaybackStream {
	stream_index: u32,
	state: StreamState,
	buffer_attr: pulse::stream::BufferAttr,
	buffer: DynPlaybackBuffer,
	volume: Vec<f32>,
	muted: bool,
	requested_bytes: usize,
	played_bytes: u64,
	write_offset: u64,
	read_offset: u64,
}

struct Client {
	id: u32,
	socket: mio::net::UnixStream,
	protocol_version: u16,
	props: Option<pulse::Props>,
	incoming: BytesMut,
	playback_streams: BTreeMap<u32, PlaybackStream>,
}

struct ServerState {
	server_info: pulse::ServerInfo,
	sinks: Vec<pulse::SinkInfo>,
	default_format_info: pulse::FormatInfo,
	next_playback_channel_index: u32,
	next_stream_index: u32,
	sink_volume: Vec<f32>,
	sink_muted: bool,
	capture_channels: u8,
	capture_spec: pulse::SampleSpec,
}

pub struct PulseServer {
	listener: UnixListener,
	socket_path: PathBuf,
	poll: mio::Poll,
	clock: mio_timerfd::TimerFd,
	clock_rate_hz: u32,

	close_rx: crossbeam_channel::Receiver<()>,
	frame_tx: crossbeam_channel::Sender<AudioFrame>,
	frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
	spare_frame: Option<AudioFrame>,

	clients: BTreeMap<mio::Token, Client>,
	server_state: ServerState,

	epoch: time::Instant,
}

impl PulseServer {
	pub fn new(
		listener: std::os::unix::net::UnixListener,
		channels: u8,
		packet_duration_ms: u32,
		frame_tx: crossbeam_channel::Sender<AudioFrame>,
		frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
	) -> Result<(Self, crossbeam_channel::Sender<()>, mio::Waker), Error> {
		let socket_path = listener
			.local_addr()?
			.as_pathname()
			.ok_or("listener has no pathname")?
			.to_path_buf();
		listener.set_nonblocking(true)?;
		let listener = UnixListener::from_std(listener);
		let poll = mio::Poll::new()?;
		let waker = mio::Waker::new(poll.registry(), WAKER)?;

		let clock_rate_hz = match packet_duration_ms {
			5 | 10 => 1000 / packet_duration_ms,
			_ => {
				if packet_duration_ms != 0 {
					tracing::warn!(
						"Unsupported packet_duration_ms {}, falling back to default {}Hz",
						packet_duration_ms,
						DEFAULT_CLOCK_RATE_HZ,
					);
				}
				DEFAULT_CLOCK_RATE_HZ
			},
		};

		let mut clock = mio_timerfd::TimerFd::new(mio_timerfd::ClockId::Monotonic)?;
		clock.set_timeout_interval(&time::Duration::from_nanos(1_000_000_000 / clock_rate_hz as u64))?;

		let sink_name = std::ffi::CString::new(SINK_NAME).unwrap();

		let capture_spec = pulse::SampleSpec {
			format: pulse::SampleFormat::Float32Le,
			channels,
			sample_rate: CAPTURE_SAMPLE_RATE,
		};

		let mut dummy_sink = pulse::SinkInfo::new_dummy(1);
		dummy_sink.name = sink_name.clone();
		dummy_sink.description = Some(std::ffi::CString::new("Moonshine virtual output").unwrap());
		dummy_sink.sample_spec = capture_spec;

		let mut server_info = pulse::ServerInfo {
			server_name: Some(std::ffi::CString::new("Moonshine").unwrap()),
			server_version: Some(std::ffi::CString::new(env!("CARGO_PKG_VERSION")).unwrap()),
			host_name: Some(std::ffi::CString::new("moonshine").unwrap()),
			default_sink_name: Some(sink_name.clone()),
			default_source_name: Some(sink_name),
			sample_spec: capture_spec,
			..Default::default()
		};
		server_info.channel_map = dummy_sink.channel_map;

		dummy_sink.ports[0].port_type = pulse::port_info::PortType::Network;
		dummy_sink.ports[0].description = Some(std::ffi::CString::new("virtual output").unwrap());

		let channel_map_str = match channels {
			6 => "front-left,front-right,front-center,lfe,rear-left,rear-right",
			8 => "front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right",
			_ => "front-left,front-right",
		};

		let mut format_props = pulse::Props::new();
		format_props.set(
			pulse::Prop::FormatChannels,
			std::ffi::CString::new(channels.to_string()).unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatChannelMap,
			std::ffi::CString::new(channel_map_str).unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatSampleFormat,
			std::ffi::CString::new("float32le").unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatRate,
			std::ffi::CString::new(CAPTURE_SAMPLE_RATE.to_string()).unwrap(),
		);

		let default_format_info = pulse::FormatInfo {
			encoding: pulse::FormatEncoding::Pcm,
			props: format_props,
		};

		dummy_sink.formats[0] = default_format_info.clone();

		let (close_tx, close_rx) = crossbeam_channel::bounded(1);

		Ok((
			Self {
				listener,
				socket_path,
				poll,
				clock,
				clock_rate_hz,
				close_rx,
				frame_tx,
				frame_recycle_rx,
				spare_frame: None,
				clients: BTreeMap::new(),
				server_state: ServerState {
					server_info,
					sinks: vec![dummy_sink],
					default_format_info,
					next_playback_channel_index: 0,
					next_stream_index: 0,
					sink_volume: vec![1.0; channels as usize],
					sink_muted: false,
					capture_channels: channels,
					capture_spec,
				},
				epoch: time::Instant::now(),
			},
			close_tx,
			waker,
		))
	}

	pub fn run(&mut self) -> Result<(), Error> {
		let mut next_client_token = 1024u64;

		self.poll.registry().register(
			&mut mio::unix::SourceFd(&self.clock.as_raw_fd()),
			CLOCK,
			mio::Interest::READABLE,
		)?;
		self.poll
			.registry()
			.register(&mut self.listener, LISTENER, mio::Interest::READABLE)?;

		let mut events = mio::Events::with_capacity(1024);

		loop {
			match self.poll.poll(&mut events, Some(time::Duration::from_secs(1))) {
				Ok(_) => (),
				Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
				Err(e) => return Err(e.into()),
			}

			match self.close_rx.try_recv() {
				Ok(()) | Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
				_ => (),
			}

			for event in events.iter() {
				match event.token() {
					CLOCK => {
						self.clock.read()?;
						self.clock_tick()?;
					},
					LISTENER => loop {
						let (mut socket, _) = match self.listener.accept() {
							Ok(conn) => conn,
							Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
							Err(e) => return Err(e.into()),
						};
						let id = next_client_token as u32;
						let token = mio::Token(next_client_token as usize);
						next_client_token += 1;

						tracing::debug!("PulseAudio client connected (id={})", id);

						self.poll
							.registry()
							.register(&mut socket, token, mio::Interest::READABLE)?;

						self.clients.insert(
							token,
							Client {
								id,
								socket,
								protocol_version: pulse::MAX_VERSION,
								props: None,
								incoming: BytesMut::new(),
								playback_streams: BTreeMap::new(),
							},
						);
					},
					client_token if event.is_read_closed() => {
						if let Some(mut client) = self.clients.remove(&client_token) {
							tracing::debug!("PulseAudio client disconnected (id={})", client.id);
							let _ = self.poll.registry().deregister(&mut client.socket);
						}
					},
					client_token if event.is_readable() && self.clients.contains_key(&client_token) => {
						if let Err(e) = self.recv(client_token) {
							tracing::error!("PulseAudio client error: {:#}", e);
							if let Some(mut client) = self.clients.remove(&client_token) {
								let _ = self.poll.registry().deregister(&mut client.socket);
							}
						}
					},
					_ => (),
				}
			}
		}
	}

	fn recv(&mut self, client_token: mio::Token) -> Result<(), Error> {
		let client = self.clients.get_mut(&client_token).unwrap();

		let mut read_size = 8192;

		'read: loop {
			let off = client.incoming.len();
			client.incoming.resize(off + read_size, 0);
			let n = match client.socket.read(&mut client.incoming[off..]) {
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					client.incoming.truncate(off);
					return Ok(());
				},
				v => v.map_err(|e| -> Error { format!("recv error: {e}").into() })?,
			};

			client.incoming.truncate(off + n);

			loop {
				if client.incoming.len() < pulse::DESCRIPTOR_SIZE {
					read_size = 8192;
					continue 'read;
				}

				let desc = pulse::read_descriptor(&mut Cursor::new(&client.incoming[..pulse::DESCRIPTOR_SIZE]))?;

				// Guard against excessively large payloads (max 4 MiB).
				const MAX_PAYLOAD_SIZE: u32 = 4 * 1024 * 1024;
				if desc.length > MAX_PAYLOAD_SIZE {
					return Err(format!("payload too large: {} bytes", desc.length).into());
				}

				if client.incoming.len() < (desc.length as usize + pulse::DESCRIPTOR_SIZE) {
					read_size = desc.length as usize + pulse::DESCRIPTOR_SIZE - client.incoming.len();
					continue 'read;
				}

				let _desc_bytes = client.incoming.split_to(pulse::DESCRIPTOR_SIZE);
				let payload = client.incoming.split_to(desc.length as usize).freeze();

				if desc.channel == u32::MAX {
					let (seq, cmd) =
						match pulse::Command::read_tag_prefixed(&mut Cursor::new(payload), client.protocol_version) {
							Err(pulse::ProtocolError::Unimplemented(seq, cmd)) => {
								tracing::warn!("Unimplemented PA command: {:?}", cmd);
								pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NotImplemented)?;
								continue;
							},
							v => v.map_err(|e| -> Error { format!("decoding command: {e}").into() })?,
						};

					match commands::handle_command(client, &mut self.server_state, seq, cmd) {
						Ok(()) => (),
						Err(e) => {
							let _ = pulse::write_error(&mut client.socket, seq, &pulse::PulseError::Internal);
							return Err(e);
						},
					}
				} else {
					commands::handle_stream_write(client, desc, &payload)?;
				}
			}
		}
	}

	fn clock_tick(&mut self) -> Result<(), Error> {
		let mut done_draining = Vec::new();

		let capture_ts = self.epoch.elapsed().as_millis() as u64;
		let channels = self.server_state.capture_channels as u32;
		let num_frames = CAPTURE_SAMPLE_RATE / self.clock_rate_hz;
		let encode_len = num_frames * channels;

		let mut frame = match self.frame_recycle_rx.try_recv() {
			Ok(mut frame) => {
				frame.buf.resize(encode_len as usize, 0.0);
				frame.buf.fill(0.0);
				frame
			},
			Err(crossbeam_channel::TryRecvError::Empty) => {
				if let Some(mut frame) = self.spare_frame.take() {
					frame.buf.resize(encode_len as usize, 0.0);
					frame.buf.fill(0.0);
					frame
				} else {
					// Recycle pool temporarily exhausted; allocate a fresh frame to avoid
					// deadlocking the encoder which is blocked on frame_rx.recv().
					AudioFrame {
						buf: vec![0.0; encode_len as usize],
						capture_ts_ms: 0,
					}
				}
			},
			Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
		};

		for client in self.clients.values_mut() {
			done_draining.clear();
			for (id, stream) in client.playback_streams.iter_mut() {
				if matches!(stream.state, StreamState::Playing | StreamState::Draining(_)) {
					let buffer_len = stream.buffer.len_bytes();

					let drained = if stream.muted {
						stream.buffer.drain_and_mix(
							num_frames as usize,
							&mut frame.buf,
							&ZERO_VOL[..stream.volume.len()],
						)
					} else {
						stream
							.buffer
							.drain_and_mix(num_frames as usize, &mut frame.buf, &stream.volume)
					};

					if !drained {
						tracing::warn!("Buffer underrun for stream {}", id);
						pulse::write_command_message(
							&mut client.socket,
							u32::MAX,
							&pulse::Command::Underflow(pulse::Underflow {
								channel: *id,
								offset: 0,
							}),
							client.protocol_version,
						)?;

						if stream.buffer_attr.pre_buffering > 0 && matches!(stream.state, StreamState::Playing) {
							stream.state = StreamState::Prebuffering(stream.buffer_attr.pre_buffering as u64);
						}

						continue;
					}

					let read_len = buffer_len - stream.buffer.len_bytes();
					stream.read_offset += read_len as u64;
					stream.played_bytes += read_len as u64;

					if matches!(stream.state, StreamState::Draining(_)) && stream.buffer.is_empty() {
						done_draining.push(*id);
					}
				}

				let bytes_needed = (stream.buffer_attr.target_length as usize)
					.saturating_sub(stream.buffer.len_bytes() + stream.requested_bytes);
				if matches!(stream.state, StreamState::Playing | StreamState::Corked)
					&& bytes_needed >= stream.buffer_attr.minimum_request_length as usize
				{
					stream.requested_bytes += bytes_needed;
					pulse::write_command_message(
						&mut client.socket,
						u32::MAX,
						&pulse::Command::Request(pulse::Request {
							channel: *id,
							length: bytes_needed as u32,
						}),
						client.protocol_version,
					)?;
				}
			}

			for id in done_draining.iter() {
				let stream = client.playback_streams.remove(id).unwrap();
				if let StreamState::Draining(drain_seq) = stream.state {
					pulse::write_ack_message(&mut client.socket, drain_seq)?;
				}
			}
		}

		// Apply sink-level volume.
		if self.server_state.sink_muted {
			frame.buf.fill(0.0);
		} else if self.server_state.sink_volume.iter().any(|&v| v != 1.0) {
			let vol = &self.server_state.sink_volume;
			for (i, sample) in frame.buf.iter_mut().enumerate() {
				*sample *= vol[i % vol.len()];
			}
		}

		frame.capture_ts_ms = capture_ts;
		match self.frame_tx.try_send(frame) {
			Ok(()) => {},
			Err(crossbeam_channel::TrySendError::Full(frame)) => {
				// Encoder is behind; stash the frame so we don't lose the allocation.
				self.spare_frame = Some(frame);
			},
			Err(crossbeam_channel::TrySendError::Disconnected(_)) => return Ok(()),
		}

		Ok(())
	}
}

impl Drop for PulseServer {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.socket_path);
	}
}
