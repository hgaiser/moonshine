mod control_stream;
use control_stream::ControlStream;

mod video_stream;
use video_stream::VideoStream;

mod audio_stream;
use audio_stream::AudioStream;

pub(super) struct Session { }

impl Session {
	pub(super) async fn new() -> Result<Self, ()> {
		Ok(Self { })
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
		let video_stream = VideoStream::new("127.0.0.1", 47998).await?;
		let _video_task = tokio::spawn(video_stream.run());

		let audio_stream = AudioStream::new("127.0.0.1", 48000).await?;
		let _audio_task = tokio::spawn(audio_stream.run());

		let control_stream = ControlStream::new("127.0.0.1", 47999)?;
		// let control_task = tokio::spawn(control_stream.run());
		control_stream.run()?;

		// tokio::try_join!(video_task, control_task, audio_task)
		// 	.map_err(|e| log::error!("One or more tasks failed: {e}"))?;

		Ok(())
	}
}
