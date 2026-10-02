use std::ffi::{CStr, CString};
use std::time;

use pulseaudio::protocol::{self as pulse, ClientInfoList};

use crate::session::stream::audio::pulse_server::dyn_buffer::DynPlaybackBuffer;
use crate::session::stream::audio::pulse_server::{
	Client, ClientWriter, Error, PlaybackStream, SINK_NAME, ServerState, StreamState, pop_missing,
};

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
				client,
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
				client,
				seq,
				&pulse::SetClientNameReply { client_id: client.id },
				client.protocol_version,
			)?;

			Ok(())
		},
		pulse::Command::GetServerInfo => {
			write_reply(client, seq, &server.server_info, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetClientInfo(id) => {
			let reply = pulse::ClientInfo {
				index: id,
				..Default::default()
			};
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetClientInfoList => {
			let reply: ClientInfoList = Vec::new();
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetCardInfo(_) => {
			pulse::write_error(
				&mut ClientWriter(&mut client.outgoing),
				seq,
				&pulse::PulseError::NoEntity,
			)?;
			Ok(())
		},
		pulse::Command::GetCardInfoList => {
			let reply: Vec<pulse::CardInfo> = Vec::new();
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSinkInfo(_) => {
			write_reply(client, seq, &server.sinks[0], client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSinkInfoList => {
			write_reply(client, seq, &server.sinks, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSourceInfo(_) => {
			pulse::write_error(
				&mut ClientWriter(&mut client.outgoing),
				seq,
				&pulse::PulseError::NoEntity,
			)?;
			Ok(())
		},
		pulse::Command::GetSourceOutputInfoList => {
			let reply: pulse::SourceOutputInfoList = Vec::new();
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSourceInfoList => {
			let reply: pulse::SourceInfoList = Vec::new();
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::Subscribe(_) => {
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::CreatePlaybackStream(params) => {
			let mut sample_spec = params.sample_spec;
			let mut spec_from_formats = false;
			if sample_spec.format == pulse::SampleFormat::Invalid
				&& let Some(format) = params.formats.iter().find_map(|f| match sample_spec_from_format(f) {
					Ok(ss) => Some(ss),
					Err(e) => {
						tracing::warn!("rejecting invalid format: {:#}", e);
						None
					},
				}) {
				sample_spec = format;
				spec_from_formats = true;
			}

			if !is_supported_format(sample_spec.format) {
				tracing::warn!("rejecting unsupported sample format {:?}", sample_spec.format);
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::NotSupported,
				)?;
				return Ok(());
			}

			// When fix_channels is set, the client expects the stream to be fixed
			// to the sink's channel configuration (used by winepulse for probing).
			let (stream_spec, stream_channel_map) = if params.flags.fix_channels {
				let mut fixed_spec = sample_spec;
				fixed_spec.channels = server.capture_spec.channels;
				let fixed_map = server.sinks[0].channel_map;
				(fixed_spec, fixed_map)
			} else {
				(sample_spec, params.channel_map)
			};

			// Validate the effective spec (after format/fix_channels fallbacks,
			// which Wine relies on) before any arithmetic or allocation uses it.
			// Like PulseAudio, the channel map must match an explicit spec.
			let explicit_map = (!params.flags.fix_channels && !spec_from_formats).then_some(&stream_channel_map);
			if let Err(reason) = validate_sample_spec(&stream_spec, explicit_map) {
				tracing::warn!("rejecting playback stream: {reason}");
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::Invalid,
				)?;
				return Ok(());
			}

			let mut buffer_attr = params.buffer_attr;
			configure_buffer(&mut buffer_attr, &stream_spec);

			let target_length = buffer_attr.target_length;
			let flags = params.flags;

			let cvolume = params
				.cvolume
				.unwrap_or_else(|| pulse::ChannelVolume::norm(stream_spec.channels));
			let volume = cvolume_to_linear(&cvolume, server.capture_channels);
			let muted = params.flags.start_muted == Some(true);

			let mut stream = PlaybackStream {
				stream_index: server.next_stream_index,
				state: StreamState::Prebuffering(buffer_attr.pre_buffering as u64),
				buffer_attr,
				buffer: DynPlaybackBuffer::new(stream_spec, stream_channel_map, server.capture_spec),
				volume,
				muted,
				// Mirrors pa_memblockq_set_tlength: seed missing = tlength so the first
				// pop_missing() immediately issues the initial REQUEST for a full buffer-fill.
				missing: target_length as i64,
				requested: 0,
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

			// Pop the initial missing demand so the wire reply carries the correct seeding
			// REQUEST size, mirroring PA's playback_stream_new() → pop_missing() call.
			let initial_req = {
				let stream = client.playback_streams.get_mut(&channel).unwrap();
				let in_prebuf = matches!(stream.state, StreamState::Prebuffering(_));
				let min_req = stream.buffer_attr.minimum_request_length as usize;
				pop_missing(&mut stream.missing, &mut stream.requested, min_req, in_prebuf)
			};

			let sink_name = CString::new(SINK_NAME).unwrap();
			let reply = pulse::CreatePlaybackStreamReply {
				channel,
				stream_index,
				sample_spec: stream_spec,
				channel_map: stream_channel_map,
				buffer_attr,
				requested_bytes: initial_req as u32,
				sink_name: Some(sink_name),
				format: server.default_format_info.clone(),
				stream_latency: 10000,
				..Default::default()
			};

			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::DrainPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				stream.state = StreamState::Draining(seq);
			} else {
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::NoEntity,
				)?;
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

				write_reply(client, seq, &reply, client.protocol_version)?;
			} else {
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::NoEntity,
				)?;
			}

			Ok(())
		},
		pulse::Command::UpdatePlaybackStreamProplist(_) => {
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
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
					// Mirrors PA's PA_SINK_INPUT_MESSAGE_SET_STATE handler:
					//   prebuf_force() sets in_prebuf=true, then handle_seek() → pop_missing()
					//   immediately issues a seeding REQUEST from the command handler (not
					//   deferred to the audio clock) for low startup latency.
					if stream.state == StreamState::Corked {
						let buf_len = stream.buffer.len_bytes();
						let target = stream.buffer_attr.target_length as usize;
						let min_req = stream.buffer_attr.minimum_request_length as usize;

						// Reset accounting — any pre-cork in-flight requests are stale.
						stream.missing = (target.saturating_sub(buf_len)) as i64;
						stream.requested = 0;

						// pop_missing with in_prebuf=true bypasses the min_req gate,
						// ensuring an immediate REQUEST even when missing < min_req.
						let req = pop_missing(&mut stream.missing, &mut stream.requested, min_req, true);

						stream.state = if req > 0 {
							pulse::write_command_message(
								&mut ClientWriter(&mut client.outgoing),
								u32::MAX,
								&pulse::Command::Request(pulse::Request {
									channel: params.channel,
									length: req as u32,
								}),
								client.protocol_version,
							)?;
							StreamState::Prebuffering(req as u64)
						} else {
							StreamState::Playing
						};
					}
				}
			}

			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::FlushPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel) {
				stream.buffer.clear();
				// Discard all in-flight credit and accumulated demand, then re-seed
				// missing = target_length so pop_missing immediately issues a fresh
				// REQUEST on the next clock tick — mirroring PA's flush + handle_seek
				// path which calls pop_missing right after flushing the queue.
				stream.missing = stream.buffer_attr.target_length as i64;
				stream.requested = 0;
				stream.played_bytes = 0;
				stream.read_offset = stream.write_offset;
			}

			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::Extension(_) => {
			pulse::write_error(
				&mut ClientWriter(&mut client.outgoing),
				seq,
				&pulse::PulseError::NoExtension,
			)?;
			Ok(())
		},
		pulse::Command::SetSinkInputVolume(params) => {
			for stream in client.playback_streams.values_mut() {
				if stream.stream_index == params.index {
					stream.volume = cvolume_to_linear(&params.volume, server.capture_channels);
				}
			}
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::SetSinkInputMute(params) => {
			for stream in client.playback_streams.values_mut() {
				if stream.stream_index == params.index {
					stream.muted = params.mute;
				}
			}
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::SetSinkVolume(params) => {
			server.sink_volume = cvolume_to_linear(&params.volume, server.capture_channels);
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::SetSinkMute(params) => {
			server.sink_muted = params.mute;
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::DeletePlaybackStream(channel) => {
			client.playback_streams.remove(&channel);
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::LookupSink(name) => {
			let sink_name = CString::new(SINK_NAME).unwrap();
			if name == sink_name {
				write_reply(client, seq, &pulse::LookupReply(1), client.protocol_version)?;
			} else {
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::NoEntity,
				)?;
			}
			Ok(())
		},
		pulse::Command::Stat => {
			write_reply(client, seq, &pulse::StatInfo::default(), client.protocol_version)?;
			Ok(())
		},
		pulse::Command::TriggerPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel)
				&& matches!(stream.state, StreamState::Prebuffering(_))
			{
				stream.state = StreamState::Playing;
			}
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::PrebufPlaybackStream(channel) => {
			if let Some(stream) = client.playback_streams.get_mut(&channel)
				&& matches!(stream.state, StreamState::Playing)
			{
				stream.state = StreamState::Prebuffering(stream.buffer_attr.pre_buffering as u64);
				// Mirror the underflow re-entry path: seed missing so pop_missing
				// immediately issues a seeding REQUEST on the next clock tick, and
				// discard stale in-flight credit accumulated while Playing.
				stream.missing = stream.buffer_attr.pre_buffering as i64;
				stream.requested = 0;
			}
			pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), seq)?;
			Ok(())
		},
		pulse::Command::GetSinkInputInfo(index) => {
			let info = client
				.playback_streams
				.values()
				.find(|s| s.stream_index == index)
				.map(|s| sink_input_info_from_stream(s, client.id));

			if let Some(info) = info {
				write_reply(client, seq, &info, client.protocol_version)?;
			} else {
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::NoEntity,
				)?;
			}
			Ok(())
		},
		pulse::Command::GetSinkInputInfoList => {
			let list: pulse::SinkInputInfoList = client
				.playback_streams
				.values()
				.map(|s| sink_input_info_from_stream(s, client.id))
				.collect();
			write_reply(client, seq, &list, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::SetPlaybackStreamBufferAttr(params) => {
			if let Some(stream) = client.playback_streams.get_mut(&params.index) {
				stream.buffer_attr = params.buffer_attr;
				let sample_spec = stream.buffer.sample_spec();
				configure_buffer(&mut stream.buffer_attr, &sample_spec);

				// Re-seed missing based on the new target so the next pop_missing()
				// issues a correctly-sized REQUEST.
				let new_target = stream.buffer_attr.target_length as usize;
				let buf_len = stream.buffer.len_bytes();
				stream.missing = (new_target.saturating_sub(buf_len + stream.requested)) as i64;

				let buffer_attr = stream.buffer_attr;
				write_reply(
					client,
					seq,
					&pulse::SetPlaybackStreamBufferAttrReply {
						buffer_attr,
						configured_sink_latency: 10000,
					},
					client.protocol_version,
				)?;
			} else {
				pulse::write_error(
					&mut ClientWriter(&mut client.outgoing),
					seq,
					&pulse::PulseError::NoEntity,
				)?;
			}
			Ok(())
		},
		pulse::Command::GetModuleInfoList => {
			let reply: pulse::ModuleInfoList = Vec::new();
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		pulse::Command::GetSampleInfoList => {
			let reply: pulse::SampleInfoList = Vec::new();
			write_reply(client, seq, &reply, client.protocol_version)?;
			Ok(())
		},
		_ => {
			tracing::warn!("ignoring command {:?}", cmd.tag());
			pulse::write_error(
				&mut ClientWriter(&mut client.outgoing),
				seq,
				&pulse::PulseError::NotImplemented,
			)?;
			Ok(())
		},
	}
}

fn is_supported_format(format: pulse::SampleFormat) -> bool {
	matches!(
		format,
		pulse::SampleFormat::U8
			| pulse::SampleFormat::S16Le
			| pulse::SampleFormat::S16Be
			| pulse::SampleFormat::Float32Le
			| pulse::SampleFormat::Float32Be
			| pulse::SampleFormat::S32Le
			| pulse::SampleFormat::S32Be
			| pulse::SampleFormat::S24Le
			| pulse::SampleFormat::S24Be
	)
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
			&mut ClientWriter(&mut client.outgoing),
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
				&mut ClientWriter(&mut client.outgoing),
				u32::MAX,
				&pulse::Command::Started(desc.channel),
				client.protocol_version,
			)?;
			stream.state = StreamState::Playing;
		}
	}

	stream.buffer.write(payload);
	stream.requested = stream.requested.saturating_sub(payload.len());
	stream.write_offset += payload.len() as u64;

	Ok(())
}

/// PA_CHANNELS_MAX; also the width of the playback buffer's per-frame scratch.
const MAX_STREAM_CHANNELS: u8 = 32;
/// PA_RATE_MAX.
const MAX_STREAM_RATE: u32 = 48_000 * 16;

/// Check a stream sample spec (and, for an explicit spec, its channel map)
/// against the ranges PulseAudio itself accepts. A zero channel count or rate
/// would otherwise reach frame-size and resampler division.
fn validate_sample_spec(spec: &pulse::SampleSpec, channel_map: Option<&pulse::ChannelMap>) -> Result<(), String> {
	if !is_supported_format(spec.format) {
		return Err(format!("unsupported sample format {:?}", spec.format));
	}
	if !(1..=MAX_STREAM_CHANNELS).contains(&spec.channels) {
		return Err(format!(
			"channel count {} is outside 1..={MAX_STREAM_CHANNELS}",
			spec.channels
		));
	}
	if !(1..=MAX_STREAM_RATE).contains(&spec.sample_rate) {
		return Err(format!(
			"sample rate {} is outside 1..={MAX_STREAM_RATE}",
			spec.sample_rate
		));
	}
	if let Some(map) = channel_map
		&& map.num_channels() != spec.channels
	{
		return Err(format!(
			"channel map has {} channels but the sample spec has {}",
			map.num_channels(),
			spec.channels
		));
	}
	Ok(())
}

/// Negotiate playback buffer attributes, as PulseAudio's
/// `fix_playback_buffer_attr` does, for a spec accepted by `validate_sample_spec`.
///
/// `u32::MAX` selects a default. Arithmetic is done in `u64` so client values
/// cannot overflow, and the result always satisfies:
/// every value is a whole number of frames; `frame <= minreq`;
/// `2 * minreq <= tlength <= maxlength <= 1 s`; `prebuf <= tlength`.
fn configure_buffer(attr: &mut pulse::stream::BufferAttr, spec: &pulse::SampleSpec) {
	let frame = (spec.channels as u64 * spec.format.bytes_per_sample() as u64).max(1);
	let align_up = |bytes: u64, unit: u64| bytes.div_ceil(unit) * unit;
	let align_down = |bytes: u64| bytes / frame * frame;
	// One 10 ms block, a whole number of frames (at least one).
	let len_10ms = align_down(frame * u64::from(spec.sample_rate) / 100).max(frame);
	// Half a block, but at least one frame (a block may be a single frame).
	let half_10ms = align_up(len_10ms / 2, frame).max(frame);
	let cap = len_10ms * 100;

	let max_length = match attr.max_length {
		u32::MAX => len_10ms * 20,
		requested => align_up(u64::from(requested), frame),
	}
	// Room for at least two minimum requests.
	.clamp(2 * half_10ms, cap);

	let minimum_request_length = match attr.minimum_request_length {
		u32::MAX => half_10ms,
		requested => align_up(u64::from(requested), frame).max(half_10ms),
	}
	.min(align_down(max_length / 2));

	let target_length = match attr.target_length {
		u32::MAX => align_up(len_10ms * 6, minimum_request_length),
		requested => align_up(u64::from(requested), minimum_request_length).max(len_10ms * 6),
	}
	.min(max_length)
	.max(2 * minimum_request_length);

	let pre_buffering = match attr.pre_buffering {
		u32::MAX => target_length,
		requested => align_up(u64::from(requested), minimum_request_length).min(target_length),
	};

	// Every value is bounded by `cap` (1 s of at most 32 x 4-byte channels at
	// PA_RATE_MAX), which fits in u32.
	let narrow = |bytes: u64| u32::try_from(bytes).unwrap_or(u32::MAX);
	attr.max_length = narrow(max_length);
	attr.minimum_request_length = narrow(minimum_request_length);
	attr.target_length = narrow(target_length);
	attr.pre_buffering = narrow(pre_buffering);
}

fn write_reply<T: pulse::CommandReply + std::fmt::Debug>(
	client: &mut Client,
	seq: u32,
	reply: &T,
	version: u16,
) -> Result<(), Error> {
	tracing::trace!("sending reply [{}] ({}): {:#?}", seq, version, reply);
	pulse::write_reply_message(&mut ClientWriter(&mut client.outgoing), seq, reply, version)?;
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

#[cfg(test)]
mod tests {
	use super::*;

	fn spec(format: pulse::SampleFormat, channels: u8, sample_rate: u32) -> pulse::SampleSpec {
		pulse::SampleSpec {
			format,
			channels,
			sample_rate,
		}
	}

	fn stereo() -> pulse::SampleSpec {
		spec(pulse::SampleFormat::S16Le, 2, 48_000)
	}

	fn defaults() -> pulse::stream::BufferAttr {
		pulse::stream::BufferAttr {
			max_length: u32::MAX,
			target_length: u32::MAX,
			pre_buffering: u32::MAX,
			minimum_request_length: u32::MAX,
			fragment_size: u32::MAX,
		}
	}

	fn assert_invariants(attr: &pulse::stream::BufferAttr, spec: &pulse::SampleSpec) {
		let frame = spec.channels as u32 * spec.format.bytes_per_sample() as u32;
		for value in [
			attr.max_length,
			attr.minimum_request_length,
			attr.target_length,
			attr.pre_buffering,
		] {
			assert_eq!(value % frame, 0, "{attr:?} for {spec:?}");
		}
		assert!(attr.minimum_request_length >= frame, "{attr:?}");
		assert!(attr.target_length >= 2 * attr.minimum_request_length, "{attr:?}");
		assert!(attr.target_length <= attr.max_length, "{attr:?}");
		assert!(attr.pre_buffering <= attr.target_length, "{attr:?}");
		let one_second = spec.sample_rate as u64 * frame as u64;
		assert!(
			u64::from(attr.max_length) <= one_second.max(100 * frame as u64),
			"{attr:?}"
		);
	}

	#[test]
	fn default_stereo_negotiation_is_unchanged() {
		let mut attr = defaults();
		configure_buffer(&mut attr, &stereo());
		// 10 ms of 48 kHz S16 stereo is 1920 bytes.
		assert_eq!(attr.max_length, 1920 * 20);
		assert_eq!(attr.minimum_request_length, 960);
		assert_eq!(attr.target_length, 1920 * 6);
		assert_eq!(attr.pre_buffering, 1920 * 6);
	}

	#[test]
	fn invalid_specs_are_rejected_before_buffer_arithmetic() {
		for invalid in [
			spec(pulse::SampleFormat::S16Le, 0, 48_000),
			spec(pulse::SampleFormat::S16Le, 33, 48_000),
			spec(pulse::SampleFormat::S16Le, u8::MAX, 48_000),
			spec(pulse::SampleFormat::S16Le, 2, 0),
			spec(pulse::SampleFormat::S16Le, 2, MAX_STREAM_RATE + 1),
			spec(pulse::SampleFormat::S16Le, 2, u32::MAX),
			spec(pulse::SampleFormat::Invalid, 2, 48_000),
			spec(pulse::SampleFormat::Alaw, 2, 48_000),
		] {
			assert!(validate_sample_spec(&invalid, None).is_err(), "{invalid:?}");
		}
		for valid in [
			stereo(),
			spec(pulse::SampleFormat::Float32Le, 1, 8_000),
			spec(pulse::SampleFormat::S24Le, 6, 96_000),
			spec(pulse::SampleFormat::S32Be, 8, 192_000),
			spec(pulse::SampleFormat::U8, 32, MAX_STREAM_RATE),
			spec(pulse::SampleFormat::S16Le, 2, 1),
		] {
			assert!(validate_sample_spec(&valid, None).is_ok(), "{valid:?}");
		}
	}

	#[test]
	fn explicit_channel_maps_must_match_the_spec() {
		let stereo_map = pulse::ChannelMap::stereo();
		assert!(validate_sample_spec(&stereo(), Some(&stereo_map)).is_ok());
		let surround = spec(pulse::SampleFormat::S16Le, 6, 48_000);
		assert!(validate_sample_spec(&surround, Some(&stereo_map)).is_err());
		// Format-derived and fix_channels specs are not checked against the map.
		assert!(validate_sample_spec(&surround, None).is_ok());
	}

	#[test]
	fn extreme_attributes_keep_invariants_without_overflow() {
		let edge = [0, 1, 2, 3, 959, 960, 961, u32::MAX - 2, u32::MAX - 1];
		let specs = [
			stereo(),
			spec(pulse::SampleFormat::U8, 1, 1),
			spec(pulse::SampleFormat::S24Le, 3, 44_100),
			spec(pulse::SampleFormat::S24Le, 7, 11_025),
			spec(pulse::SampleFormat::Float32Le, 32, MAX_STREAM_RATE),
			spec(pulse::SampleFormat::S16Le, 2, 99),
		];
		for spec in specs {
			for max_length in edge.iter().copied().chain([u32::MAX]) {
				for minimum_request_length in edge.iter().copied().chain([u32::MAX]) {
					for target_length in edge.iter().copied().chain([u32::MAX]) {
						for pre_buffering in [0, 1, u32::MAX - 1, u32::MAX] {
							let mut attr = pulse::stream::BufferAttr {
								max_length,
								target_length,
								pre_buffering,
								minimum_request_length,
								fragment_size: u32::MAX,
							};
							configure_buffer(&mut attr, &spec);
							assert_invariants(&attr, &spec);
						}
					}
				}
			}
		}
	}

	#[test]
	fn renegotiation_is_idempotent() {
		let mut attr = pulse::stream::BufferAttr {
			max_length: 7777,
			target_length: 3333,
			pre_buffering: 1111,
			minimum_request_length: 1001,
			fragment_size: u32::MAX,
		};
		configure_buffer(&mut attr, &stereo());
		let first = attr;
		configure_buffer(&mut attr, &stereo());
		assert_eq!(attr, first);
	}
}
