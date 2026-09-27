use std::ffi::CStr;
use std::fmt;
use std::time::Duration;

use pulseaudio::protocol::stream::{BufferAttr, StreamFlags};
use pulseaudio::protocol::{Prop, Props, SampleSpec};

use crate::session::stream::audio::{AudioConfig, AudioStreamContext, SinkSpec};

pub(crate) fn stream_start(ctx: &AudioStreamContext) {
	let AudioStreamContext {
		audio_config: AudioConfig {
			channels,
			channel_mask,
			high_quality,
			stream_config,
		},
		encrypt_audio,
		qos,
		packet_duration_ms,
	} = ctx;
	let quality = if *high_quality { "high quality" } else { "low quality" };
	tracing::info!(
		"Starting audio stream: ({channels} channels, mask {channel_mask:#x}), {quality}, {} Opus stream(s) / {} coupled, {} bps, {packet_duration_ms}ms packets, encryption {encrypt_audio}, QoS {qos}.",
		stream_config.streams,
		stream_config.coupled_streams,
		stream_config.bitrate,
	);
}

/// Identifies which client and stream a playback stream log line refers to.
pub(crate) struct StreamOwner<'a> {
	pub client_id: u32,
	pub client_props: Option<&'a Props>,
	/// Playback channel, the id used by the underrun warning.
	pub channel: u32,
}

impl fmt::Display for StreamOwner<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let prop = |p| self.client_props.and_then(|props| prop_str(props, p));
		write!(f, "stream {} (client {}", self.channel, self.client_id)?;
		if let Some(name) = prop(Prop::ApplicationName) {
			write!(f, " {name:?}")?;
		}
		if let Some(binary) = prop(Prop::ApplicationProcessBinary) {
			write!(f, " binary={binary:?}")?;
		}
		if let Some(pid) = prop(Prop::ApplicationProcessId) {
			write!(f, " pid={pid}")?;
		}
		f.write_str(")")
	}
}

fn prop_str(props: &Props, prop: Prop) -> Option<&str> {
	CStr::from_bytes_with_nul(props.get(prop)?).ok()?.to_str().ok()
}

pub(crate) fn pulse_create_stream_cmd(
	owner: StreamOwner,
	stream_props: &Props,
	stream_spec: SampleSpec,
	sink_spec: SinkSpec,
	flags: StreamFlags,
	buff_attr: BufferAttr,
) {
	tracing::info!(
		"Audio playback {owner} opened, media {:?}: app {:?} {} Hz {} ch -> sink {} Hz {} ch [{}]{}, buffer target={}B min_req={}B prebuf={}B, muted={}",
		prop_str(stream_props, Prop::MediaName).unwrap_or(""),
		stream_spec.format,
		stream_spec.sample_rate,
		stream_spec.channels,
		sink_spec.sample_rate as usize,
		sink_spec.channels as u8,
		if stream_spec.sample_rate == sink_spec.sample_rate as u32 {
			"passthrough"
		} else {
			"resampling"
		},
		if flags.fix_channels { " fix_channels" } else { "" },
		buff_attr.target_length,
		buff_attr.minimum_request_length,
		buff_attr.pre_buffering,
		flags.start_muted == Some(true),
	);
}

pub(crate) fn pulse_stream_closed(
	owner: StreamOwner,
	reason: &str,
	stream_spec: SampleSpec,
	open_for: Duration,
	played_bytes: u64,
) {
	let bytes_per_sec =
		stream_spec.sample_rate as u64 * stream_spec.channels as u64 * stream_spec.format.bytes_per_sample() as u64;
	let played_secs = played_bytes as f64 / bytes_per_sec.max(1) as f64;
	tracing::info!("Audio playback {owner} closed ({reason}) after {open_for:.3?}, played {played_secs:.3}s of audio");
}
