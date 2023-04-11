use tokio::sync::mpsc;

use crate::{config::SessionConfig, session::SessionContext};

use self::{
	audio::{run_audio_stream, AudioStreamContext},
	video::{run_video_stream, VideoStreamContext},
	control::run_control_stream,
};


mod audio;
mod control;
mod video;

pub struct Session {
	pub video_stream_context: VideoStreamContext,
	pub audio_stream_context: AudioStreamContext,
}

impl Session {
	pub(super) async fn new(config: SessionConfig) -> Result<Self, ()> {
		let video_stream_config = VideoStreamContext { codec_name: config.codec, fec_percentage: config.fec_percentage, ..Default::default() };

		Ok(Self {
			video_stream_context: video_stream_config,
			audio_stream_context: Default::default(),
		})
	}

	pub(super) fn description(&self) -> Result<sdp_types::Session, ()> {
		sdp_types::Session::parse(b"v=0
o=- 0 0 IN IP4 127.0.0.1
s=No Name
t=0 0
a=tool:libavformat LIBAVFORMAT_VERSION
m=video 0 RTP/AVP 96
b=AS:2000
a=rtpmap:96 H264/90000
a=fmtp:96 packetization-mode=1
a=control:streamid=0")
			.map_err(|e| log::error!("Failed to parse SDP session: {e}"))
	}

	pub(super) async fn run(
		&self,
		context: SessionContext,
	) -> Result<(), ()> {
		let (video_command_tx, video_command_rx) = mpsc::channel(10);

		let video_task = tokio::spawn(run_video_stream(
			"0.0.0.0",
			47998,
			self.video_stream_context.clone(),
			video_command_rx,
		));

		let audio_task = tokio::spawn(run_audio_stream(
			"0.0.0.0",
			48000,
			self.audio_stream_context.clone(),
		));

		let control_task = tokio::spawn(run_control_stream(
			"0.0.0.0",
			47999,
			video_command_tx,
			context,
		));

		if let Err(e) = tokio::try_join!(
			video_task,
			audio_task,
			control_task,
		) {
			log::error!("One or more tasks failed: {e}");
			return Err(());
		}

		Ok(())
	}
}
