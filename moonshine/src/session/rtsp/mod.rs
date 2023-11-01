use std::{net::{ToSocketAddrs, SocketAddr}, str::FromStr};
use async_shutdown::ShutdownManager;
use enet::Enet;
use rtsp_types::{headers::{self, Transport}, Method};
use tokio::{net::{TcpListener, TcpStream}, io::{AsyncReadExt, AsyncWriteExt}, sync::mpsc};

use crate::{
	config::Config,
	session::rtsp::stream::{VideoStreamContext, AudioStreamContext, run_control_stream, VideoStream, AudioStream}
};

use super::SessionContext;

mod stream;

enum RtspServerCommand {
	SetStreamContext(VideoStreamContext, AudioStreamContext),
	StartStream,
}

#[derive(Clone)]
pub struct RtspServer {
	config: Config,
	command_tx: mpsc::Sender<RtspServerCommand>,
	session_context: SessionContext,
	stop_signal: ShutdownManager<()>,
}

struct RtspServerInner {
	config: Config,
	session_context: SessionContext,
	video_stream_context: Option<VideoStreamContext>,
	audio_stream_context: Option<AudioStreamContext>,
	stop_signal: ShutdownManager<()>,
}

impl RtspServer {
	pub fn new(
		config: Config,
		session_context: SessionContext,
		enet: Enet,
	) -> Result<Self, ()> {
		let stop_signal = ShutdownManager::new();
		let stop_token = stop_signal.trigger_shutdown_token(());

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = RtspServerInner {
			config: config.clone(),
			session_context: session_context.clone(),
			video_stream_context: None,
			audio_stream_context: None,
			stop_signal: stop_signal.clone(),
		};
		tokio::spawn(async move { inner.run(command_rx, enet).await; drop(stop_token); });

		let server = Self { config: config.clone(), command_tx, session_context, stop_signal: stop_signal.clone() };

		tokio::spawn(stop_signal.wrap_trigger_shutdown((), stop_signal.wrap_cancel({
			let server = server.clone();
			let stop_signal = stop_signal.clone();
			async move {
				let address = (config.address.as_str(), config.stream.port).to_socket_addrs()
					.map_err(|e| log::error!("Failed to resolve address {}:{}: {}", config.address, config.stream.port, e))?
					.next()
					.ok_or_else(|| log::error!("Failed to resolve address {}:{}", config.address, config.stream.port))?;
				let listener = TcpListener::bind(address)
					.await
					.map_err(|e| log::error!("Failed to bind to address {}: {}", address, e))?;

				log::info!("RTSP server listening on {}", address);

				loop {
					let (connection, address) = listener.accept()
						.await
						.map_err(|e| log::error!("Failed to accept connection: {}", e))?;
					log::debug!("Accepted connection from {}", address);

					tokio::spawn(stop_signal.wrap_cancel({
						let server = server.clone();
						async move {
							let _ = server.handle_connection(
								connection,
								address,
							).await;
						}
					}));
				}

				Ok::<(), ()>(())
			}
		})));

		Ok(server)
	}

	pub fn context(&self) -> &SessionContext {
		&self.session_context
	}

	pub async fn set_stream_context(&self, video_stream_context: VideoStreamContext, audio_stream_context: AudioStreamContext) -> Result<(), ()> {
		self.command_tx.send(RtspServerCommand::SetStreamContext(video_stream_context, audio_stream_context)).await
			.map_err(|e| log::error!("Failed to send SetStreamContext command: {e}"))
	}

	pub fn description(&self) -> Result<sdp_types::Session, ()> {
		// TODO: Generate this based on settings.
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

	pub async fn start_stream(&self) -> Result<(), ()> {
		self.command_tx.send(RtspServerCommand::StartStream).await
			.map_err(|e| log::error!("Failed to send StartStream command: {e}"))
	}

	pub fn stop_stream(&self) {
		log::debug!("Stopping stream.");
		let _ = self.stop_signal.trigger_shutdown(());
	}

	fn handle_options_request(&self, request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.header(headers::PUBLIC, "OPTIONS DESCRIBE SETUP PLAY")
			.build(Vec::new())
	}

	fn handle_setup_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		let transports = match request.typed_header::<rtsp_types::headers::Transports>() {
			Ok(transports) => transports,
			Err(e) => {
				log::warn!("Failed to parse transport information from SETUP request: {e}");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			}
		};
		let transports = match transports {
			Some(transports) => transports,
			None => {
				log::warn!("No transport information in SETUP request.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			}
		};

		if let Some(transport) = (*transports).first() {
			match transport {
				Transport::Other(_transport) => {
					let request_uri = match request.request_uri() {
						Some(query) => query,
						None => {
							log::warn!("No request URI in SETUP request.");
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest)
						}
					};
					let query = match request_uri.query_pairs().next() {
						Some(query) => query,
						None => {
							log::warn!("No query in request URI in SETUP request.");
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest)
						}
					};
					if query.0 != "streamid" {
						log::warn!("Expected only one query parameter with 'streamid', but didn't find it.");
						return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
					}

					// Example query: streamid=control/13/0
					let (stream_id, port) = match query.1.split('/').next() {
						Some("video") => ("video", self.config.stream.video.port),
						Some("audio") => ("audio", self.config.stream.audio.port),
						Some("control") => ("control", self.config.stream.control.port),
						Some(stream) => {
							log::warn!("Unknown stream '{stream}'");
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
						}
						None => {
							log::warn!("Unexpected query format for query '{}'", query.1);
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
						},
					};

					log::info!("Responding with server_port={port} for stream '{stream_id}'.");

					return rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
						.header(headers::CSEQ, cseq.to_string())
						.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
						.header(headers::TRANSPORT, format!("server_port={port}"))
						.build(Vec::new())
					;
				}
				t => {
					log::warn!("Received request for unsupported transport: {:?}", t);
					return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
				}
			}
		}

		log::warn!("No transports found in SETUP request.");
		rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest)
	}

	async fn handle_describe_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		let mut buffer = Vec::new();
		let description = match self.description() {
			Ok(description) => description,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError);
			}
		};
		if let Err(e) = description.write(&mut buffer) {
			log::error!("Failed to write SDP data to buffer: {}", e);
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError);
		}

		let debug = match String::from_utf8(buffer.clone()) {
			Ok(debug) => debug,
			Err(e) => {
				log::error!("Failed to write SDP debug string: {e}");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError);
			}
		};
		log::trace!("SDP session data: \n{}", debug.trim());

		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.build(buffer)
	}

	async fn handle_announce_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		let sdp_session = match sdp_types::Session::parse(request.body()) {
			Ok(sdp_session) => sdp_session,
			Err(e) => {
				log::warn!("Failed to parse ANNOUNCE request as SDP session: {e}");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			}
		};

		log::info!("Received SDP session from ANNOUNCE request: {sdp_session:#?}");

		let width = match get_sdp_attribute(&sdp_session, "x-nv-video[0].clientViewportWd") {
			Ok(width) => width,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let height = match get_sdp_attribute(&sdp_session, "x-nv-video[0].clientViewportHt") {
			Ok(height) => height,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let fps = match get_sdp_attribute(&sdp_session, "x-nv-video[0].maxFPS") {
			Ok(fps) => fps,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let packet_size = match get_sdp_attribute(&sdp_session, "x-nv-video[0].packetSize") {
			Ok(packet_size) => packet_size,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let mut bitrate = match get_sdp_attribute(&sdp_session, "x-nv-vqos[0].bw.maximumBitrateKbps") {
			Ok(bitrate) => bitrate,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		bitrate *= 1024; // Convert from kbps to bps.
		let minimum_fec_packets = match get_sdp_attribute(&sdp_session, "x-nv-vqos[0].fec.minRequiredFecPackets") {
			Ok(minimum_fec_packets) => minimum_fec_packets,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};

		let video_stream_context = VideoStreamContext {
			width,
			height,
			fps,
			packet_size,
			bitrate,
			minimum_fec_packets,
		};

		let packet_duration = match get_sdp_attribute(&sdp_session, "x-nv-aqos.packetDuration") {
			Ok(packet_duration) => packet_duration,
			Err(()) => {
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let audio_stream_context = AudioStreamContext {
			packet_duration,
			remote_input_key: self.session_context.remote_input_key.clone(),
			remote_input_key_id: self.session_context.remote_input_key_id,
		};

		if self.set_stream_context(video_stream_context, audio_stream_context).await.is_err() {
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError)
		}

		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.build(Vec::new())
	}

	async fn handle_play_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		if self.start_stream().await.is_err() {
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError)
		}

		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.build(Vec::new())
	}

	async fn handle_connection(
		&self,
		mut connection: TcpStream,
		address: SocketAddr,
	) -> Result<(), ()> {
		let mut message_buffer = String::new();

		let message = loop {
			let mut buffer = [0u8; 2048];
			let bytes_read = connection.read(&mut buffer).await
				.map_err(|e| log::error!("Failed to read from connection '{}': {}", address, e))?;
			if bytes_read == 0 {
				log::warn!("Received empty RTSP request.");
				return Ok(());
			}
			message_buffer.push_str(std::str::from_utf8(&buffer[..bytes_read])
				.map_err(|e| log::error!("Failed to convert message to string: {e}"))?);

			// Hacky workaround to fix rtsp_types parsing SETUP/PLAY requests from Moonlight.
			let message_buffer = message_buffer.replace("streamid", "rtsp://localhost?streamid");
			let message_buffer = message_buffer.replace("PLAY /", "PLAY rtsp://localhost/");

			log::trace!("Request: {}", message_buffer);
			let result = rtsp_types::Message::parse(&message_buffer);

			break match result {
				Ok((message, _consumed)) => message,
				Err(rtsp_types::ParseError::Incomplete(_)) => {
					log::debug!("Incomplete RTSP message received, waiting for more data.");
					continue;
				},
				Err(e) => {
					log::error!("Failed to parse request as RTSP message: {}", e);
					return Err(());
				}
			};
		};

		// log::trace!("Consumed {} bytes into RTSP request: {:#?}", consumed, message);

		let response = match message {
			rtsp_types::Message::Request(ref request) => {
				log::debug!("Received RTSP {:?} request", request.method());

				let cseq: i32 = request.header(&headers::CSEQ)
					.ok_or_else(|| log::error!("RTSP request has no CSeq header"))?
					.as_str()
					.parse()
					.map_err(|e| log::error!("Failed to parse CSeq header: {}", e))?;

				match request.method() {
					Method::Announce => self.handle_announce_request(request, cseq).await,
					Method::Describe => self.handle_describe_request(request, cseq).await,
					Method::Options => self.handle_options_request(request, cseq),
					Method::Setup => self.handle_setup_request(request, cseq),
					Method::Play => self.handle_play_request(request, cseq).await,
					method => {
						log::warn!("Received request with unsupported method {:?}", method);
						rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest)
					}
				}
			},
			_ => {
				log::warn!("Unknown RTSP message type received");
				rtsp_response(0, rtsp_types::Version::V2_0, rtsp_types::StatusCode::BadRequest)
			}
		};

		log::debug!("Sending RTSP response");
		log::trace!("{:#?}", response);

		let mut buffer = Vec::new();
		response.write(&mut buffer)
			.map_err(|e| log::error!("Failed to serialize RTSP response: {}", e))?;

		connection.write_all(&buffer).await
			.map_err(|e| log::error!("Failed to send RTSP response: {}", e))?;

		// For some reason, Moonlight expects a connection per request, so we close the connection here.
		connection.shutdown()
			.await
			.map_err(|e| log::error!("Failed to shutdown the connection: {e}"))?;

		Ok(())
	}
}

impl RtspServerInner {
	async fn run(
		mut self,
		mut command_rx: mpsc::Receiver<RtspServerCommand>,
		enet: Enet,
	) {
		while let Some(command) = command_rx.recv().await {
			match command {
				RtspServerCommand::SetStreamContext(video_stream_context, audio_stream_context) => {
					if self.video_stream_context.is_some() || self.audio_stream_context.is_some() {
						log::warn!("Can't set stream context when it is already set.");
						continue;
					}

					self.video_stream_context = Some(video_stream_context);
					self.audio_stream_context = Some(audio_stream_context);
				},
				RtspServerCommand::StartStream => {
					let video_stream_context = match self.video_stream_context.clone() {
						Some(video_stream_context) => video_stream_context,
						None => {
							log::warn!("Can't start stream without a stream context.");
							continue
						}
					};
					let audio_stream_context = match self.audio_stream_context.clone() {
						Some(audio_stream_context) => audio_stream_context,
						None => {
							log::warn!("Can't start stream without a stream context.");
							continue
						}
					};

					let video_stream = VideoStream::new(self.config.clone(), video_stream_context, self.stop_signal.clone());
					let audio_stream = AudioStream::new(self.config.clone(), audio_stream_context, self.stop_signal.clone());

					// TODO: Figure out a way to just use tokio::spawn.
					//       This is not possible at the moment due to enet crate not being async.
					tokio::task::spawn_blocking({
						let config = self.config.clone();
						let session_context = self.session_context.clone();
						let enet = enet.clone();
						let video_stream = video_stream.clone();
						let audio_stream = audio_stream.clone();
						let stop_signal = self.stop_signal.clone();
						move || {
							tokio::runtime::Handle::current().block_on(run_control_stream(
								config,
								video_stream,
								audio_stream,
								session_context,
								enet,
								stop_signal,
							))
						}
					});
				},
			}
		}

		log::debug!("Command channel closed.");
	}
}

fn rtsp_response(cseq: i32, version: rtsp_types::Version, status: rtsp_types::StatusCode) -> rtsp_types::Response<Vec<u8>> {
	rtsp_types::Response::builder(version, status)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new())
}

fn get_sdp_attribute<F: FromStr>(sdp_session: &sdp_types::Session, attribute: &str) -> Result<F, ()> {
	sdp_session.get_first_attribute_value(attribute)
		.map_err(|e| log::warn!("Failed to attribute {attribute} from request: {e}"))?
		.ok_or_else(|| log::warn!("No {attribute} attribute in request"))?
		.trim()
		.parse()
		.map_err(|_| log::warn!("Attribute {attribute} can't be parsed."))
}
