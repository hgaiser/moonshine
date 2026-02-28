use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{Cursor, Read};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time;

use bytes::BytesMut;
use mio::net::UnixListener;
use pulseaudio::protocol::{self as pulse, ClientInfoList};

use super::buffer::PlaybackBuffer;

type Error = Box<dyn std::error::Error + Send + Sync>;

const WAKER: mio::Token = mio::Token(0);
const LISTENER: mio::Token = mio::Token(1);
const CLOCK: mio::Token = mio::Token(2);

/// The server emits samples at this rate to the encoder.
pub const CAPTURE_SAMPLE_RATE: u32 = 48000;
pub const CAPTURE_CHANNEL_COUNT: u32 = 2;
pub const CAPTURE_SPEC: pulse::SampleSpec = pulse::SampleSpec {
	format: pulse::SampleFormat::Float32Le,
	channels: CAPTURE_CHANNEL_COUNT as u8,
	sample_rate: CAPTURE_SAMPLE_RATE,
};

/// Clock tick rate — 200 Hz = 5ms, matching the Moonlight client's expected
/// Opus frame duration.
const CLOCK_RATE_HZ: u32 = 200;

const SINK_NAME: &str = "moonshine";

/// A buffer of interleaved f32 samples ready for Opus encoding.
pub struct AudioFrame {
	/// Interleaved stereo f32 samples.
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
	buffer: PlaybackBuffer<[f32; 2]>,
	volume: [f32; 2],
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
	sink_volume: [f32; 2],
	sink_muted: bool,
}

pub struct PulseServer {
	listener: UnixListener,
	socket_path: PathBuf,
	poll: mio::Poll,
	clock: mio_timerfd::TimerFd,

	close_rx: crossbeam_channel::Receiver<()>,
	frame_tx: crossbeam_channel::Sender<AudioFrame>,
	frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,

	clients: BTreeMap<mio::Token, Client>,
	server_state: ServerState,

	epoch: time::Instant,
}

impl PulseServer {
	pub fn new(
		socket_path: impl AsRef<Path>,
		frame_tx: crossbeam_channel::Sender<AudioFrame>,
		frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
	) -> Result<(Self, crossbeam_channel::Sender<()>, mio::Waker), Error> {
		let socket_path = socket_path.as_ref();
		let listener = UnixListener::bind(socket_path)?;
		let poll = mio::Poll::new()?;
		let waker = mio::Waker::new(poll.registry(), WAKER)?;

		let mut clock = mio_timerfd::TimerFd::new(mio_timerfd::ClockId::Monotonic)?;
		clock.set_timeout_interval(&time::Duration::from_nanos(1_000_000_000 / CLOCK_RATE_HZ as u64))?;

		let sink_name = CString::new(SINK_NAME).unwrap();

		let mut dummy_sink = pulse::SinkInfo::new_dummy(1);
		dummy_sink.name = sink_name.clone();
		dummy_sink.description = Some(CString::new("Moonshine virtual output").unwrap());
		dummy_sink.sample_spec = CAPTURE_SPEC;

		let mut server_info = pulse::ServerInfo {
			server_name: Some(CString::new("Moonshine").unwrap()),
			server_version: Some(CString::new(env!("CARGO_PKG_VERSION")).unwrap()),
			host_name: Some(CString::new("moonshine").unwrap()),
			default_sink_name: Some(sink_name.clone()),
			default_source_name: Some(sink_name),
			sample_spec: CAPTURE_SPEC,
			..Default::default()
		};
		server_info.channel_map = dummy_sink.channel_map;

		dummy_sink.ports[0].port_type = pulse::port_info::PortType::Network;
		dummy_sink.ports[0].description = Some(CString::new("virtual output").unwrap());

		let mut format_props = pulse::Props::new();
		format_props.set(pulse::Prop::FormatChannels, CString::new("2").unwrap());
		format_props.set(
			pulse::Prop::FormatChannelMap,
			CString::new("front-left,front-right").unwrap(),
		);
		format_props.set(pulse::Prop::FormatSampleFormat, CString::new("float32le").unwrap());
		format_props.set(
			pulse::Prop::FormatRate,
			CString::new(CAPTURE_SAMPLE_RATE.to_string()).unwrap(),
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
				socket_path: socket_path.to_path_buf(),
				poll,
				clock,
				close_rx,
				frame_tx,
				frame_recycle_rx,
				clients: BTreeMap::new(),
				server_state: ServerState {
					server_info,
					sinks: vec![dummy_sink],
					default_format_info,
					next_playback_channel_index: 0,
					next_stream_index: 0,
					sink_volume: [1.0, 1.0],
					sink_muted: false,
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
					LISTENER => {
						let (mut socket, _) = self.listener.accept()?;
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

					match handle_command(client, &mut self.server_state, seq, cmd) {
						Ok(()) => (),
						Err(e) => {
							let _ = pulse::write_error(&mut client.socket, seq, &pulse::PulseError::Internal);
							return Err(e);
						},
					}
				} else {
					handle_stream_write(client, desc, &payload)?;
				}
			}
		}
	}

	fn clock_tick(&mut self) -> Result<(), Error> {
		let mut done_draining = Vec::new();

		let capture_ts = self.epoch.elapsed().as_millis() as u64;
		let num_frames = CAPTURE_SAMPLE_RATE / CLOCK_RATE_HZ;
		let encode_len = num_frames * CAPTURE_CHANNEL_COUNT;

		let mut frame = match self.frame_recycle_rx.try_recv() {
			Ok(mut frame) => {
				frame.buf.resize(encode_len as usize, 0.0);
				frame.buf.fill(0.0);
				Some(frame)
			},
			Err(crossbeam_channel::TryRecvError::Empty) => {
				// No one's listening, but we still need to drain audio from clients.
				None
			},
			Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
		};

		for client in self.clients.values_mut() {
			done_draining.clear();
			for (id, stream) in client.playback_streams.iter_mut() {
				if matches!(stream.state, StreamState::Playing | StreamState::Draining(_)) {
					let buffer_len = stream.buffer.len_bytes();

					let Some(frames) = stream.buffer.drain(num_frames as usize) else {
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
					};

					if let Some(ref mut frame) = frame {
						let vol = if stream.muted { [0.0, 0.0] } else { stream.volume };
						let mut resampled = dasp::Signal::into_interleaved_samples(frames).into_iter();
						for (i, sample) in frame.buf.iter_mut().enumerate() {
							let s = resampled.next().unwrap_or_default();
							*sample += s * vol[i % 2];
						}
					} else {
						drop(frames);
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

		if let Some(mut frame) = frame {
			// Apply sink-level volume.
			if self.server_state.sink_muted {
				frame.buf.fill(0.0);
			} else if self.server_state.sink_volume != [1.0, 1.0] {
				for (i, sample) in frame.buf.iter_mut().enumerate() {
					*sample *= self.server_state.sink_volume[i % 2];
				}
			}

			frame.capture_ts_ms = capture_ts;
			let _ = self.frame_tx.send(frame);
		}

		Ok(())
	}
}

impl Drop for PulseServer {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.socket_path);
	}
}

fn handle_command(client: &mut Client, server: &mut ServerState, seq: u32, cmd: pulse::Command) -> Result<(), Error> {
	tracing::trace!("got command [{}]: {:#?}", seq, cmd);

	match cmd {
		pulse::Command::Auth(pulse::AuthParams { version, .. }) => {
			let version = std::cmp::min(version, pulse::MAX_VERSION);
			client.protocol_version = version;
			tracing::trace!("client protocol version: {}", version);

			write_reply(
				&mut client.socket,
				seq,
				&pulse::AuthReply {
					version: pulse::MAX_VERSION,
					..Default::default()
				},
				client.protocol_version,
			)?;

			Ok(())
		},
		pulse::Command::SetClientName(props) => {
			client.props = Some(props);

			write_reply(
				&mut client.socket,
				seq,
				&pulse::SetClientNameReply { client_id: client.id },
				client.protocol_version,
			)?;

			Ok(())
		},
		pulse::Command::GetServerInfo => {
			write_reply(&mut client.socket, seq, &server.server_info, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetClientInfo(id) => {
			let reply = pulse::ClientInfo {
				index: id,
				..Default::default()
			};
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetClientInfoList => {
			let reply: ClientInfoList = Vec::new();
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetCardInfo(_) => {
			pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NoEntity)?;
			Ok(())
		},
		pulse::Command::GetCardInfoList => {
			let reply: Vec<pulse::CardInfo> = Vec::new();
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSinkInfo(_) => {
			write_reply(&mut client.socket, seq, &server.sinks[0], client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSinkInfoList => {
			write_reply(&mut client.socket, seq, &server.sinks, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSourceInfo(_) => {
			pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NoEntity)?;
			Ok(())
		},
		pulse::Command::GetSourceOutputInfoList => {
			let reply: pulse::SourceOutputInfoList = Vec::new();
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSourceInfoList => {
			let reply: pulse::SinkInfoList = Vec::new();
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::Subscribe(_) => {
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::CreatePlaybackStream(params) => {
			let mut sample_spec = params.sample_spec;
			if sample_spec.format == pulse::SampleFormat::Invalid {
				if let Some(format) = params.formats.iter().find_map(|f| match sample_spec_from_format(f) {
					Ok(ss) => Some(ss),
					Err(e) => {
						tracing::warn!("rejecting invalid format: {:#}", e);
						None
					},
				}) {
					sample_spec = format;
				}
			}

			let mut buffer_attr = params.buffer_attr;
			configure_buffer(&mut buffer_attr, &sample_spec);

			let target_length = buffer_attr.target_length;
			let flags = params.flags;

			let cvolume = params
				.cvolume
				.unwrap_or_else(|| pulse::ChannelVolume::norm(sample_spec.channels));
			let volume = cvolume_to_linear_stereo(&cvolume);
			let muted = params.flags.start_muted == Some(true);

			let mut stream = PlaybackStream {
				stream_index: server.next_stream_index,
				state: StreamState::Prebuffering(buffer_attr.pre_buffering as u64),
				buffer_attr,
				buffer: PlaybackBuffer::new(sample_spec, params.channel_map, CAPTURE_SPEC),
				volume,
				muted,
				requested_bytes: target_length as usize,
				played_bytes: 0,
				write_offset: 0,
				read_offset: 0,
			};

			if buffer_attr.pre_buffering == 0 || flags.start_corked {
				stream.state = StreamState::Corked;
			}

			let channel = server.next_playback_channel_index;
			server.next_playback_channel_index += 1;

			let stream_index = server.next_stream_index;
			server.next_stream_index += 1;

			client.playback_streams.insert(channel, stream);

			let sink_name = CString::new(SINK_NAME).unwrap();
			let reply = pulse::CreatePlaybackStreamReply {
				channel,
				stream_index,
				sample_spec,
				channel_map: params.channel_map,
				buffer_attr,
				requested_bytes: target_length,
				sink_name: Some(sink_name),
				format: server.default_format_info.clone(),
				stream_latency: 10000,
				..Default::default()
			};

			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::DrainPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				stream.state = StreamState::Draining(seq);
			}
			Ok(())
		},
		pulse::Command::GetPlaybackLatency(pulse::LatencyParams { channel, now, .. }) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				let reply = pulse::PlaybackLatency {
					sink_usec: 10000,
					source_usec: 0,
					playing: matches!(stream.state, StreamState::Playing),
					local_time: now,
					remote_time: time::SystemTime::now(),
					write_offset: stream.write_offset as i64,
					read_offset: stream.read_offset as i64,
					underrun_for: u64::MAX,
					playing_for: stream.played_bytes,
				};

				write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			}

			Ok(())
		},
		pulse::Command::UpdatePlaybackStreamProplist(_) => {
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::CorkPlaybackStream(params) => {
			if let Some(stream) = client.playback_streams.get_mut(&params.channel) {
				if params.cork {
					// Cork: pause the stream from any playing-like state.
					if matches!(stream.state, StreamState::Playing | StreamState::Prebuffering(_)) {
						stream.state = StreamState::Corked;
					}
				} else {
					// Uncork: resume from corked state.
					if stream.state == StreamState::Corked {
						let needed = stream
							.buffer_attr
							.target_length
							.saturating_sub(stream.buffer.len_bytes() as u32);

						stream.state = if needed > 0 {
							pulse::write_command_message(
								&mut client.socket,
								u32::MAX,
								&pulse::Command::Request(pulse::Request {
									channel: params.channel,
									length: needed,
								}),
								client.protocol_version,
							)?;

							stream.requested_bytes = needed as usize;
							StreamState::Prebuffering(needed as u64)
						} else {
							StreamState::Playing
						};
					}
				}
			}

			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::FlushPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				stream.buffer.clear();
				stream.requested_bytes = 0;
				stream.played_bytes = 0;
				stream.read_offset = stream.write_offset;
			}

			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::Extension(_) => {
			pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NoExtension)?;
			Ok(())
		},
		pulse::Command::SetSinkInputVolume(params) => {
			for stream in client.playback_streams.values_mut() {
				if stream.stream_index == params.index {
					stream.volume = cvolume_to_linear_stereo(&params.volume);
				}
			}
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::SetSinkInputMute(params) => {
			for stream in client.playback_streams.values_mut() {
				if stream.stream_index == params.index {
					stream.muted = params.mute;
				}
			}
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::SetSinkVolume(params) => {
			server.sink_volume = cvolume_to_linear_stereo(&params.volume);
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::SetSinkMute(params) => {
			server.sink_muted = params.mute;
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::DeletePlaybackStream(channel) => {
			client.playback_streams.remove(&channel);
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::LookupSink(name) => {
			let sink_name = CString::new(SINK_NAME).unwrap();
			if name == sink_name {
				write_reply(&mut client.socket, seq, &pulse::LookupReply(1), client.protocol_version)?;
			} else {
				pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NoEntity)?;
			}
			Ok(())
		},
		pulse::Command::Stat => {
			write_reply(
				&mut client.socket,
				seq,
				&pulse::StatInfo::default(),
				client.protocol_version,
			)?;
			Ok(())
		},
		pulse::Command::TriggerPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				if matches!(stream.state, StreamState::Prebuffering(_)) {
					stream.state = StreamState::Playing;
				}
			}
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::PrebufPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				if matches!(stream.state, StreamState::Playing) {
					stream.state = StreamState::Prebuffering(stream.buffer_attr.pre_buffering as u64);
				}
			}
			pulse::write_ack_message(&mut client.socket, seq)?;
			Ok(())
		},
		pulse::Command::GetSinkInputInfo(index) => {
			let info = client
				.playback_streams
				.values()
				.find(|s| s.stream_index == index)
				.map(|s| sink_input_info_from_stream(s, client.id));

			if let Some(info) = info {
				write_reply(&mut client.socket, seq, &info, client.protocol_version)?;
			} else {
				pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NoEntity)?;
			}
			Ok(())
		},
		pulse::Command::GetSinkInputInfoList => {
			let list: pulse::SinkInputInfoList = client
				.playback_streams
				.values()
				.map(|s| sink_input_info_from_stream(s, client.id))
				.collect();
			write_reply(&mut client.socket, seq, &list, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::SetPlaybackStreamBufferAttr(params) => {
			if let Some(stream) = client.playback_streams.get_mut(&params.index) {
				stream.buffer_attr = params.buffer_attr;
				let sample_spec = stream.buffer.buffer().sample_spec;
				configure_buffer(&mut stream.buffer_attr, &sample_spec);

				write_reply(
					&mut client.socket,
					seq,
					&pulse::SetPlaybackStreamBufferAttrReply {
						buffer_attr: stream.buffer_attr,
						configured_sink_latency: 10000,
					},
					client.protocol_version,
				)?;
			} else {
				pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NoEntity)?;
			}
			Ok(())
		},
		pulse::Command::GetModuleInfoList => {
			let reply: pulse::ModuleInfoList = Vec::new();
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSampleInfoList => {
			let reply: pulse::SampleInfoList = Vec::new();
			write_reply(&mut client.socket, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		_ => {
			tracing::warn!("ignoring command {:?}", cmd.tag());
			pulse::write_error(&mut client.socket, seq, &pulse::PulseError::NotImplemented)?;
			Ok(())
		},
	}
}

fn sample_spec_from_format(f: &pulse::FormatInfo) -> Result<pulse::SampleSpec, Error> {
	let format = f
		.props
		.get(pulse::Prop::FormatSampleFormat)
		.ok_or_else(|| -> Error { "missing sample format".into() })?;
	let rate = f
		.props
		.get(pulse::Prop::FormatRate)
		.ok_or_else(|| -> Error { "missing sample rate".into() })?;
	let channels = f
		.props
		.get(pulse::Prop::FormatChannels)
		.ok_or_else(|| -> Error { "missing channel count".into() })?;

	let format_str = sanitize_prop_str(format)?;
	let format = match format_str {
		"s16le" => pulse::SampleFormat::S16Le,
		"s16be" => pulse::SampleFormat::S16Be,
		"u8" => pulse::SampleFormat::U8,
		"s32le" => pulse::SampleFormat::S32Le,
		"s32be" => pulse::SampleFormat::S32Be,
		"s24le" => pulse::SampleFormat::S24Le,
		"s24be" => pulse::SampleFormat::S24Be,
		"float32le" => pulse::SampleFormat::Float32Le,
		"float32be" => pulse::SampleFormat::Float32Be,
		_ => return Err(format!("unsupported sample format: {format_str:?}").into()),
	};

	let rate = sanitize_prop_str(rate)?
		.parse()
		.map_err(|e| -> Error { format!("invalid sample rate {rate:?}: {e}").into() })?;

	let channels = sanitize_prop_str(channels)?
		.parse()
		.map_err(|e| -> Error { format!("invalid channel count {channels:?}: {e}").into() })?;

	Ok(pulse::SampleSpec {
		format,
		sample_rate: rate,
		channels,
	})
}

fn sanitize_prop_str(b: &[u8]) -> Result<&str, Error> {
	use std::ffi::CStr;

	let s = CStr::from_bytes_with_nul(b).map_err(|e| -> Error { format!("invalid string: {e}").into() })?;
	let s = s.to_str().map_err(|e| -> Error { format!("invalid utf-8: {e}").into() })?;
	Ok(s.trim_matches('"'))
}

fn handle_stream_write(client: &mut Client, desc: pulse::Descriptor, payload: &[u8]) -> Result<(), Error> {
	let stream = client
		.playback_streams
		.get_mut(&desc.channel)
		.ok_or_else(|| -> Error { format!("invalid channel {}", desc.channel).into() })?;

	if desc.offset != 0 {
		tracing::warn!("seeking not supported, ignoring offset {}", desc.offset);
	}

	let buffer_len = stream.buffer.len_bytes();
	let remaining = (stream.buffer_attr.max_length as usize).saturating_sub(buffer_len);
	let payload = if payload.len() > remaining {
		pulse::write_command_message(
			&mut client.socket,
			u32::MAX,
			&pulse::Command::Overflow(payload.len().saturating_sub(remaining) as u32),
			client.protocol_version,
		)?;
		&payload[..remaining]
	} else {
		payload
	};

	if let StreamState::Prebuffering(n) = stream.state {
		let needed = n.saturating_sub(payload.len() as u64);
		if needed > 0 {
			stream.state = StreamState::Prebuffering(needed);
		} else {
			tracing::debug!("Starting playback for stream {}", desc.channel);
			pulse::write_command_message(
				&mut client.socket,
				u32::MAX,
				&pulse::Command::Started(desc.channel),
				client.protocol_version,
			)?;
			stream.state = StreamState::Playing;
		}
	}

	stream.buffer.write(payload);
	stream.requested_bytes = stream.requested_bytes.saturating_sub(payload.len());
	stream.write_offset += payload.len() as u64;

	Ok(())
}

fn configure_buffer(attr: &mut pulse::stream::BufferAttr, spec: &pulse::SampleSpec) {
	let sample_size = spec.format.bytes_per_sample();
	let frame_size = spec.channels as usize * sample_size;
	let len_10ms = (frame_size * spec.sample_rate as usize / 100) as u32;

	if attr.max_length == u32::MAX {
		attr.max_length = len_10ms * 20;
	} else {
		attr.max_length = attr.max_length.next_multiple_of(frame_size as u32).min(len_10ms * 100);
	}

	if attr.minimum_request_length == u32::MAX {
		attr.minimum_request_length = (len_10ms / 2).next_multiple_of(frame_size as u32);
	} else {
		attr.minimum_request_length = attr
			.minimum_request_length
			.next_multiple_of(frame_size as u32)
			.max(len_10ms / 2);
	}

	if attr.target_length == u32::MAX {
		attr.target_length = (len_10ms * 2)
			.next_multiple_of(attr.minimum_request_length)
			.min(attr.max_length);
	} else {
		attr.target_length = attr
			.target_length
			.next_multiple_of(attr.minimum_request_length)
			.max(len_10ms)
			.min(attr.max_length);

		if attr.target_length < (attr.minimum_request_length * 2) {
			attr.target_length = attr.minimum_request_length * 2;
		}
	}

	if attr.pre_buffering == u32::MAX {
		attr.pre_buffering = attr.target_length;
	} else {
		attr.pre_buffering = attr
			.pre_buffering
			.next_multiple_of(attr.minimum_request_length)
			.min(attr.target_length);
	}
}

fn write_reply<T: pulse::CommandReply + std::fmt::Debug>(
	socket: &mut mio::net::UnixStream,
	seq: u32,
	reply: &T,
	version: u16,
) -> Result<(), Error> {
	tracing::trace!("sending reply [{}] ({}): {:#?}", seq, version, reply);
	pulse::write_reply_message(socket, seq, reply, version)?;
	Ok(())
}

/// Convert a PulseAudio `ChannelVolume` to a pair of linear gain values for
/// stereo output. If the volume has 1 channel, both outputs use that channel's
/// volume. If 2+, uses first two channels.
fn cvolume_to_linear_stereo(cv: &pulse::ChannelVolume) -> [f32; 2] {
	let vols = cv.channels();
	match vols.len() {
		0 => [1.0, 1.0],
		1 => {
			let v = vols[0].to_linear();
			[v, v]
		},
		_ => [vols[0].to_linear(), vols[1].to_linear()],
	}
}

fn linear_stereo_to_cvolume(vol: [f32; 2]) -> pulse::ChannelVolume {
	let mut cv = pulse::ChannelVolume::empty();
	cv.push(pulse::Volume::from_linear(vol[0]));
	cv.push(pulse::Volume::from_linear(vol[1]));
	cv
}

fn sink_input_info_from_stream(stream: &PlaybackStream, client_id: u32) -> pulse::SinkInputInfo {
	let sample_spec = stream.buffer.buffer().sample_spec;
	pulse::SinkInputInfo {
		index: stream.stream_index,
		name: CString::new(format!("stream-{}", stream.stream_index)).unwrap(),
		client_index: Some(client_id),
		sink_index: 1,
		sample_spec,
		cvolume: linear_stereo_to_cvolume(stream.volume),
		muted: stream.muted,
		corked: matches!(stream.state, StreamState::Corked),
		has_volume: true,
		volume_writable: true,
		..Default::default()
	}
}
