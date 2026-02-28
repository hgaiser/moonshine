use std::ffi::{CStr, CString};
use std::time;

use pulseaudio::protocol::{self as pulse, ClientInfoList};

use super::dyn_buffer::DynPlaybackBuffer;
use super::{Client, Error, PlaybackStream, ServerState, StreamState, SINK_NAME};

pub(super) fn handle_command(
	client: &mut Client,
	server: &mut ServerState,
	seq: u32,
	cmd: pulse::Command,
) -> Result<(), Error> {
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
			let volume = cvolume_to_linear(&cvolume, server.capture_channels);
			let muted = params.flags.start_muted == Some(true);

			let mut stream = PlaybackStream {
				stream_index: server.next_stream_index,
				state: StreamState::Prebuffering(buffer_attr.pre_buffering as u64),
				buffer_attr,
				buffer: DynPlaybackBuffer::new(sample_spec, params.channel_map, server.capture_spec),
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
					stream.volume = cvolume_to_linear(&params.volume, server.capture_channels);
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
			server.sink_volume = cvolume_to_linear(&params.volume, server.capture_channels);
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
				let sample_spec = stream.buffer.sample_spec();
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
	let s = CStr::from_bytes_with_nul(b).map_err(|e| -> Error { format!("invalid string: {e}").into() })?;
	let s = s
		.to_str()
		.map_err(|e| -> Error { format!("invalid utf-8: {e}").into() })?;
	Ok(s.trim_matches('"'))
}

pub(super) fn handle_stream_write(client: &mut Client, desc: pulse::Descriptor, payload: &[u8]) -> Result<(), Error> {
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

/// Convert a PulseAudio `ChannelVolume` to N linear gain values.
/// If the volume has fewer channels than `out_channels`, the last
/// volume value is repeated. If more, only the first `out_channels` are taken.
fn cvolume_to_linear(cv: &pulse::ChannelVolume, out_channels: u8) -> Vec<f32> {
	let vols = cv.channels();
	let n = out_channels as usize;
	(0..n)
		.map(|i| {
			if i < vols.len() {
				vols[i].to_linear()
			} else if !vols.is_empty() {
				vols[vols.len() - 1].to_linear()
			} else {
				1.0
			}
		})
		.collect()
}

fn linear_to_cvolume(vol: &[f32]) -> pulse::ChannelVolume {
	let mut cv = pulse::ChannelVolume::empty();
	for &v in vol {
		cv.push(pulse::Volume::from_linear(v));
	}
	cv
}

fn sink_input_info_from_stream(stream: &PlaybackStream, client_id: u32) -> pulse::SinkInputInfo {
	let sample_spec = stream.buffer.sample_spec();
	pulse::SinkInputInfo {
		index: stream.stream_index,
		name: CString::new(format!("stream-{}", stream.stream_index)).unwrap(),
		client_index: Some(client_id),
		sink_index: 1,
		sample_spec,
		cvolume: linear_to_cvolume(&stream.volume),
		muted: stream.muted,
		corked: matches!(stream.state, StreamState::Corked),
		has_volume: true,
		volume_writable: true,
		..Default::default()
	}
}
