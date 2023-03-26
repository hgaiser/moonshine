mod control_stream;
use control_stream::ControlStream;

mod video_stream;
use tokio::sync::mpsc;
use video_stream::{VideoStream, VideoStreamConfig};

mod audio_stream;
use audio_stream::AudioStream;

use crate::config::SessionConfig;

use self::audio_stream::AudioStreamConfig;

mod rtp;

pub struct Session {
	pub video_stream_config: VideoStreamConfig,
	pub audio_stream_config: AudioStreamConfig,
}

impl Session {
	pub(super) async fn new(config: SessionConfig) -> Result<Self, ()> {
		let video_stream_config = VideoStreamConfig { codec_name: config.codec, fec_percentage: config.fec_percentage, ..Default::default() };

		Ok(Self {
			video_stream_config,
			audio_stream_config: AudioStreamConfig::default(),
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

	pub(super) async fn run(&self) -> Result<(), ()> {
		let (video_command_tx, video_command_rx) = mpsc::channel(10);

		let video_stream = VideoStream::new("127.0.0.1", 47998, self.video_stream_config.clone()).await?;
		let video_task = tokio::spawn(video_stream.run(video_command_rx));

		let audio_stream = AudioStream::new("127.0.0.1", 48000, self.audio_stream_config.clone()).await?;
		let audio_task = tokio::spawn(audio_stream.run());

		let control_stream = ControlStream::new("127.0.0.1", 47999)?;
		let control_task = tokio::spawn(async move {
			control_stream.run(video_command_tx).await
		});

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
