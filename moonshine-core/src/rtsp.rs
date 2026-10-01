use crate::session::stream::video::pyrowave_protocol::{BITSTREAM_ID, PyroWaveDialect};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use async_shutdown::ShutdownManager;
use rtsp_types::Method;
use rtsp_types::headers;
use rtsp_types::headers::Transport;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use crate::ShutdownReason;
use crate::healthcheck::{
	CODEC_PYROWAVE, CODEC_PYROWAVE_444, CODEC_PYROWAVE_HDR, CODEC_PYROWAVE_MASK, supports_video_format,
};
use crate::session::manager::SessionManager;
use crate::session::stream::audio::ALL_AUDIO_CONFIGS;
use crate::session::stream::audio::AudioChannels;
use crate::session::stream::audio::AudioConfig;
use crate::session::stream::audio::AudioStreamConfig;
use crate::session::stream::audio::AudioStreamContext;
use crate::session::stream::control::ControlStreamConfig;
use crate::session::stream::video::VideoDynamicRange;
use crate::session::stream::video::VideoFormat;
use crate::session::stream::video::VideoStreamConfig;
use crate::session::stream::video::VideoStreamContext;
use crate::session::stream::video::{BitDepth, ColorRange, NegotiatedVideoFormat, VideoChromaSampling, VideoCodec};

#[repr(u8)]
enum ServerCapabilities {
	PenTouchEvents = 0x01,
	ControllerTouchEvents = 0x02,
}

#[repr(u8)]
enum EncryptionFlags {
	ControlV2 = 0x01,
	Video = 0x02,
	Audio = 0x04,
}

#[derive(Clone)]
pub struct RtspServer {
	address: String,
	rtsp_port: u16,
	video_config: VideoStreamConfig,
	audio_config: AudioStreamConfig,
	control_config: ControlStreamConfig,
	session_manager: SessionManager,
	supported_codecs: u32,
}

impl RtspServer {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		address: String,
		rtsp_port: u16,
		video_config: VideoStreamConfig,
		audio_config: AudioStreamConfig,
		control_config: ControlStreamConfig,
		session_manager: SessionManager,
		supported_codecs: u32,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Self {
		let server = Self {
			address,
			rtsp_port,
			video_config: video_config.clone(),
			audio_config: audio_config.clone(),
			control_config: control_config.clone(),
			session_manager,
			supported_codecs,
		};

		tokio::spawn({
			let server = server.clone();
			async move {
				let _ = shutdown
					.wrap_cancel(shutdown.wrap_trigger_shutdown(ShutdownReason::RtspShutdown, {
						let server = server.clone();
						async move {
							let ip = server
								.address
								.parse::<IpAddr>()
								.map_err(|e| tracing::error!("Failed to parse address '{}': {}", server.address, e))?;
							let socket_addr = SocketAddr::new(ip, server.rtsp_port);
							let listener = TcpListener::bind(socket_addr)
								.await
								.map_err(|e| tracing::error!("Failed to bind to address {}: {}", socket_addr, e))?;

							tracing::debug!("RTSP server listening on {}", socket_addr);

							loop {
								let (connection, address) = listener
									.accept()
									.await
									.map_err(|e| tracing::error!("Failed to accept connection: {}", e))?;
								tracing::trace!("Accepted connection from {}", address);

								tokio::spawn({
									let server = server.clone();
									async move {
										let _ = server.handle_connection(connection, address).await;
									}
								});
							}

							// Is there another way to define the return type of this function?
							#[allow(unreachable_code)]
							Ok::<(), ()>(())
						}
					}))
					.await;

				tracing::debug!("RTSP server shutting down.");
			}
		});

		server
	}

	fn capabilities(&self) -> u8 {
		ServerCapabilities::PenTouchEvents as u8 | ServerCapabilities::ControllerTouchEvents as u8
	}

	fn encryption_flags_supported(&self) -> u8 {
		let mut flags = EncryptionFlags::ControlV2 as u8 | EncryptionFlags::Audio as u8;
		if self.video_config.encrypt {
			flags |= EncryptionFlags::Video as u8;
		}
		flags
	}

	#[allow(clippy::result_unit_err)]
	pub fn description(&self) -> String {
		// This is a very simple SDP description, the minimal that Moonlight requires.
		// TODO: Fill this based on server settings.
		// TODO: Use:
		//       "a=x-ss-general.featureFlags: <FEATURE FLAGS>"
		//       "x-nv-video[0].refPicInvalidation=1"
		//       "a=rtpmap:98 AV1/90000" (For AV1 support)
		//       "a=fmtp:97 surround-params=<SURROUND PARAMS>"
		//       "<AUDIO STREAM MAPPING>"
		let mut result = String::new();

		result.push_str(&format!("a=x-ss-general.featureFlags:{}\r\n", self.capabilities()));
		result.push_str(&format!(
			"a=x-ss-general.encryptionSupported:{}\r\n",
			self.encryption_flags_supported()
		));
		result.push_str("sprop-parameter-sets=AAAAAU\r\n");
		result.push_str("a=x-nv-video[0].refPicInvalidation:1\r\n");
		result.push_str("a=rtpmap:98 AV1/90000\r\n");
		result.push_str("a=fmtp:96 packetization-mode=1\r\n");
		if self.supported_codecs & CODEC_PYROWAVE_MASK != 0 {
			result.push_str("a=x-ss-pyrowave.version:1\r\na=rtpmap:99 PYROWAVE/90000\r\n");
			result.push_str(&format!("a=x-ss-pyrowave.bitstream:{BITSTREAM_ID}\r\na=x-ss-pyrowave.dialects:native-wire-v1 record-framed\r\na=x-ss-pyrowave.profiles:{}\r\n", pyrowave_profiles(self.supported_codecs)));
		}

		// Emit surround-params for each Opus configuration.
		// Moonlight selects the appropriate config at ANNOUNCE based on channel count and quality.
		// Values are packed as single digits with no separators, matching moonlight-common-c's
		// parseOpusConfigFromParamString() which reads one digit character at a time.
		for (i, config) in ALL_AUDIO_CONFIGS.iter().enumerate() {
			// GFE advertises an incorrect mapping for normal quality surround configurations,
			// rotating LFE (index 3) to the end. Moonlight undoes this rotation after parsing.
			// We must apply the same rotation for normal quality 5.1 and 7.1 configs.
			let mut mapping = config.mapping;
			let is_normal_surround = i == 2 || i == 4; // SURROUND51 or SURROUND71
			if is_normal_surround && config.channels >= AudioChannels::Surround51 {
				mapping[3..config.channels as usize].rotate_left(1);
			}

			let mut params = format!(
				"{}{}{}",
				config.channels as usize, config.streams, config.coupled_streams
			);
			for &m in mapping.iter().take(config.channels as usize) {
				params.push_str(&format!("{}", m));
			}
			result.push_str(&format!("a=fmtp:97 surround-params={}\r\n", params));
		}

		result
	}

	fn handle_options_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.header(headers::PUBLIC, "OPTIONS DESCRIBE SETUP PLAY")
			.build(Vec::new())
	}

	fn handle_setup_request(&self, request: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
		let transports = match request.typed_header::<rtsp_types::headers::Transports>() {
			Ok(transports) => transports,
			Err(e) => {
				tracing::warn!("Failed to parse transport information from SETUP request: {e}");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let transports = match transports {
			Some(transports) => transports,
			None => {
				tracing::warn!("No transport information in SETUP request.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};

		if let Some(transport) = (*transports).first() {
			match transport {
				Transport::Other(_transport) => {
					let request_uri = match request.request_uri() {
						Some(query) => query,
						None => {
							tracing::warn!("No request URI in SETUP request.");
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
						},
					};
					let query = match request_uri.query_pairs().next() {
						Some(query) => query,
						None => {
							tracing::warn!("No query in request URI in SETUP request.");
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
						},
					};
					if query.0 != "streamid" {
						tracing::warn!("Expected only one query parameter with 'streamid', but didn't find it.");
						return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
					}

					// Example query: streamid=control/13/0
					let (stream_id, port) = match query.1.split('/').next() {
						Some("video") => ("video", self.video_config.port),
						Some("audio") => ("audio", self.audio_config.port),
						Some("control") => ("control", self.control_config.port),
						Some(stream) => {
							tracing::warn!("Unknown stream '{stream}'");
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
						},
						None => {
							tracing::warn!("Unexpected query format for query '{}'", query.1);
							return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
						},
					};

					tracing::debug!("Responding with server_port={port} for stream '{stream_id}'.");

					return rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
						.header(headers::CSEQ, cseq.to_string())
						.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
						.header(headers::TRANSPORT, format!("server_port={port}"))
						.build(Vec::new());
				},
				t => {
					tracing::warn!("Received request for unsupported transport: {:?}", t);
					return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
				},
			}
		}

		tracing::warn!("No transports found in SETUP request.");
		rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest)
	}

	async fn handle_describe_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		let description = self.description();
		tracing::debug!("SDP session data: \n{}", description.trim());
		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.build(description.into_bytes())
	}

	async fn handle_announce_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
	) -> rtsp_types::Response<Vec<u8>> {
		let sdp_session = match sdp_types::Session::parse(request.body()) {
			Ok(sdp_session) => sdp_session,
			Err(e) => {
				tracing::warn!("Failed to parse ANNOUNCE request as SDP session: {e}");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};

		tracing::trace!("Received SDP session from ANNOUNCE request: {sdp_session:#?}");

		let width = match get_sdp_attribute(&sdp_session, "x-nv-video[0].clientViewportWd") {
			Ok(width) => width,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-video[0].clientViewportWd in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let height = match get_sdp_attribute(&sdp_session, "x-nv-video[0].clientViewportHt") {
			Ok(height) => height,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-video[0].clientViewportHt in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let fps = match get_sdp_attribute(&sdp_session, "x-nv-video[0].maxFPS") {
			Ok(fps) => fps,
			Err(()) => {
				tracing::warn!("Failed to parse xx-nv-video[0].maxFPS in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let requested_packet_size: usize = match get_sdp_attribute(&sdp_session, "x-nv-video[0].packetSize") {
			Ok(packet_size) => packet_size,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-video[0].packetSize in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let packet_size = self.video_config.clamp_packet_size(requested_packet_size);
		if packet_size != requested_packet_size {
			tracing::info!("Clamping client video packet size from {requested_packet_size} to {packet_size} bytes.");
		}
		let bitrate_kbps: u64 = match get_sdp_attribute(&sdp_session, "x-ml-video.configuredBitrateKbps") {
			Ok(bitrate) => bitrate,
			Err(()) => {
				tracing::warn!("Failed to parse x-ml-video.configuredBitrateKbps in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let bitrate = match bitrate_bps_from_kbps(bitrate_kbps) {
			Some(bitrate) => bitrate,
			None => {
				tracing::warn!(
					bitrate_kbps,
					"Configured video bitrate is outside the supported numeric range"
				);
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let minimum_fec_packets = match get_sdp_attribute(&sdp_session, "x-nv-vqos[0].fec.minRequiredFecPackets") {
			Ok(minimum_fec_packets) => minimum_fec_packets,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-vqos[0].fec.minRequiredFecPackets in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let video_qos_type: String = match get_sdp_attribute(&sdp_session, "x-nv-vqos[0].qosTrafficType") {
			Ok(video_qos_type) => video_qos_type,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-vqos[0].qosTrafficType in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let video_format: u32 = match get_sdp_attribute(&sdp_session, "x-nv-vqos[0].bitStreamFormat") {
			Ok(video_format) => video_format,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-vqos[0].bitStreamFormat in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let video_format = match VideoFormat::try_from(video_format) {
			Ok(video_format) => video_format,
			Err(()) => {
				tracing::warn!("Invalid video format: {}", video_format);
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let pyrowave_dialect = match negotiated_pyrowave_dialect(video_format, &sdp_session) {
			Ok(dialect) => dialect,
			Err(reason) => {
				tracing::warn!(reason, "PyroWave ANNOUNCE rejected");
				return rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::BadRequest)
					.header(headers::CSEQ, cseq.to_string())
					.header(headers::CONTENT_TYPE, "text/plain")
					.build(format!("PyroWave ANNOUNCE rejected: {reason}").into_bytes());
			},
		};

		let (dynamic_range, chroma_sampling_type, bit_depth) = match negotiated_video_axes(&sdp_session) {
			Ok(axes) => axes,
			Err(()) => {
				tracing::warn!("Client requested malformed or unsupported video profile attributes");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};

		let max_reference_frames: u32 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-video[0].maxNumReferenceFrames").unwrap_or(1);

		// Bit 0 selects full-range luma, the remaining bits an SDR colorspace.
		// Sunshine: `colorspace_from_client_config()`
		const CSC_COLORSPACE_REC601: u32 = 0;
		const CSC_COLORSPACE_REC709: u32 = 1;
		let encoder_csc_mode: Option<u32> = get_optional_sdp_attribute(&sdp_session, "x-nv-video[0].encoderCscMode");
		let full_range = encoder_csc_mode.unwrap_or_default() & 0x1 != 0;

		// Only Rec.709 is encoded. Rec.601 doubles as the protocol default for
		// clients that never chose a colorspace, so it stays quiet; higher
		// values are explicit requests worth a warning.
		match encoder_csc_mode.map(|mode| mode >> 1) {
			Some(CSC_COLORSPACE_REC601) => {
				tracing::debug!("Client sent SDR colorspace Rec.601 (the protocol default), encoding Rec.709.");
			},
			Some(colorspace) if colorspace != CSC_COLORSPACE_REC709 => {
				tracing::warn!(
					"Client requested SDR colorspace {colorspace} via encoderCscMode, encoding Rec.709 instead."
				);
			},
			_ => {},
		}
		tracing::debug!(
			"Client requested {} range video.",
			if full_range { "full" } else { "limited" }
		);

		// Parse the client's encryption flags from the ANNOUNCE SDP.
		let client_encryption_flags: u8 =
			get_optional_sdp_attribute(&sdp_session, "x-ss-general.encryptionEnabled").unwrap_or(0);

		let range = if full_range {
			ColorRange::Full
		} else {
			ColorRange::Limited
		};
		let format = if dynamic_range == VideoDynamicRange::Hdr {
			let mut format = NegotiatedVideoFormat::hdr10(video_format, chroma_sampling_type, range);
			// Preserve an explicit client bit-depth choice so validation rejects a
			// contradictory 8-bit HDR request rather than silently promoting it.
			format.bit_depth = bit_depth;
			format
		} else {
			NegotiatedVideoFormat::sdr(video_format, chroma_sampling_type, bit_depth, range)
		};
		if let Err(reason) = format.validate() {
			tracing::warn!(?format, reason, "Rejecting contradictory video format request");
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
		}
		if !supports_video_format(self.supported_codecs, format) {
			tracing::warn!(
				codec = %format.codec,
				chroma = %format.chroma,
				bit_depth = format.bit_depth.bits(),
				hdr = format.hdr,
				"Requested video combination was not detected on this GPU/encoder; refusing silent fallback"
			);
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::UnsupportedMediaType);
		}

		tracing::info!(
			codec = %format.codec,
			chroma = %format.chroma,
			bit_depth = format.bit_depth.bits(),
			primaries = ?format.primaries,
			transfer = ?format.transfer,
			matrix = ?format.matrix,
			range = ?format.range,
			hdr = format.hdr,
			width,
			height,
			fps,
			pyrowave_dialect = ?pyrowave_dialect,
			pyrowave_bitstream = BITSTREAM_ID,
			packet_size,
			"Selected video mode"
		);

		let video_stream_context = VideoStreamContext {
			pyrowave_dialect,
			width,
			height,
			fps,
			packet_size,
			bitrate,
			minimum_fec_packets,
			qos: video_qos_type != "0",
			format,
			max_reference_frames,
			encrypt_video: self.video_config.encrypt && (client_encryption_flags & EncryptionFlags::Video as u8 != 0),
		};

		let packet_duration: u32 = match get_sdp_attribute(&sdp_session, "x-nv-aqos.packetDuration") {
			Ok(packet_duration) => packet_duration,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-aqos.packetDuration in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		let audio_qos_type: String = match get_sdp_attribute(&sdp_session, "x-nv-aqos.qosTrafficType") {
			Ok(audio_qos_type) => audio_qos_type,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-aqos.qosTrafficType in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};

		// Parse surround audio attributes from SDP.
		let surround_channels: u8 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.numChannels").unwrap_or(2);
		let surround_mask: u32 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.channelMask").unwrap_or(0x3);
		let surround_enable: u8 = get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.enable").unwrap_or(0);
		let audio_quality: u8 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.AudioQuality").unwrap_or(1);

		let (channels, channel_mask) = if surround_enable != 0 {
			(AudioChannels::from(surround_channels), surround_mask)
		} else {
			// Fall back to the values from the HTTP launch request.
			let ctx = self.session_manager.get_session_context().await;
			match ctx {
				Ok(Some(session_ctx)) => (session_ctx.audio_channels, session_ctx.audio_channel_mask),
				_ => (AudioChannels::Stereo, 0x3),
			}
		};
		let high_quality = audio_quality != 0;
		let audio_config = AudioConfig::from_channels(channels, channel_mask, high_quality);

		tracing::debug!(
			"Audio config: {} channels, mask=0x{:x}, high_quality={}, config={:?}",
			audio_config.channels,
			audio_config.channel_mask,
			audio_config.high_quality,
			audio_config.stream_config
		);

		let audio_stream_context = AudioStreamContext {
			packet_duration_ms: packet_duration,
			qos: audio_qos_type != "0",
			audio_config,
			encrypt_audio: client_encryption_flags & EncryptionFlags::Audio as u8 != 0,
		};

		if self
			.session_manager
			.set_stream_context(video_stream_context, audio_stream_context)
			.await
			.is_err()
		{
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError);
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
		if self.session_manager.start_session().await.is_err() {
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError);
		}

		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.build(Vec::new())
	}

	async fn handle_connection(&self, mut connection: TcpStream, address: SocketAddr) -> Result<(), ()> {
		// Gate RTSP access: only process requests when a session has been initialized
		// via the authenticated /launch or /resume endpoint (H-3 mitigation).
		match self.session_manager.get_session_context().await {
			Ok(Some(_)) => {},
			_ => {
				tracing::warn!("Rejected RTSP connection from {}: no active session", address);
				return Ok(());
			},
		}

		let mut message_buffer = String::new();

		let message = loop {
			let mut buffer = [0u8; 2048];
			let bytes_read = connection
				.read(&mut buffer)
				.await
				.map_err(|e| tracing::warn!("Failed to read from connection '{}': {}", address, e))?;
			if bytes_read == 0 {
				tracing::warn!("Received empty RTSP request.");
				return Ok(());
			}
			message_buffer.push_str(
				std::str::from_utf8(&buffer[..bytes_read])
					.map_err(|e| tracing::warn!("Failed to convert message to string: {e}"))?,
			);

			// Hacky workaround to fix rtsp_types parsing SETUP/PLAY requests from Moonlight.
			let message_buffer = message_buffer.replace("streamid", "rtsp://localhost?streamid");
			let message_buffer = message_buffer.replace("PLAY /", "PLAY rtsp://localhost/");

			tracing::trace!("Request: {}", message_buffer);
			let result = rtsp_types::Message::parse(&message_buffer);

			break match result {
				Ok((message, _consumed)) => message,
				Err(rtsp_types::ParseError::Incomplete(_)) => {
					tracing::debug!("Incomplete RTSP message received, waiting for more data.");
					continue;
				},
				Err(e) => {
					tracing::warn!("Failed to parse request as RTSP message: {}", e);
					return Err(());
				},
			};
		};

		// tracing::trace!("Consumed {} bytes into RTSP request: {:#?}", consumed, message);

		let response = match message {
			rtsp_types::Message::Request(ref request) => {
				tracing::debug!("Received RTSP {:?} request", request.method());

				let cseq: i32 = request
					.header(&headers::CSEQ)
					.ok_or_else(|| tracing::warn!("RTSP request has no CSeq header"))?
					.as_str()
					.parse()
					.map_err(|e| tracing::warn!("Failed to parse CSeq header: {}", e))?;

				match request.method() {
					Method::Announce => self.handle_announce_request(request, cseq).await,
					Method::Describe => self.handle_describe_request(request, cseq).await,
					Method::Options => self.handle_options_request(request, cseq),
					Method::Setup => self.handle_setup_request(request, cseq),
					Method::Play => self.handle_play_request(request, cseq).await,
					method => {
						tracing::warn!("Received request with unsupported method {:?}", method);
						rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest)
					},
				}
			},
			_ => {
				tracing::warn!("Unknown RTSP message type received");
				rtsp_response(0, rtsp_types::Version::V2_0, rtsp_types::StatusCode::BadRequest)
			},
		};

		tracing::debug!("Sending RTSP response");
		tracing::trace!("{:#?}", response);

		let mut buffer = Vec::new();
		response
			.write(&mut buffer)
			.map_err(|e| tracing::warn!("Failed to serialize RTSP response: {}", e))?;

		connection
			.write_all(&buffer)
			.await
			.map_err(|e| tracing::warn!("Failed to send RTSP response: {}", e))?;

		// For some reason, Moonlight expects a connection per request, so we close the connection here.
		connection
			.shutdown()
			.await
			.map_err(|e| tracing::warn!("Failed to shutdown the connection: {e}"))?;

		Ok(())
	}
}

fn rtsp_response(
	cseq: i32,
	version: rtsp_types::Version,
	status: rtsp_types::StatusCode,
) -> rtsp_types::Response<Vec<u8>> {
	rtsp_types::Response::builder(version, status)
		.header(headers::CSEQ, cseq.to_string())
		.build(Vec::new())
}

fn pyrowave_profiles(capabilities: u32) -> String {
	let mut profiles = Vec::new();
	for (bit, sdr, hdr) in [
		(CODEC_PYROWAVE, "420-sdr8", "420-hdr10"),
		(CODEC_PYROWAVE_444, "444-sdr8", "444-hdr10"),
	] {
		if capabilities & bit != 0 {
			profiles.push(sdr);
			if capabilities & CODEC_PYROWAVE_HDR != 0 {
				profiles.push(hdr);
			}
		}
	}
	profiles.join(" ")
}

fn unique_sdp_attribute<'a>(session: &'a sdp_types::Session, name: &str) -> Result<Option<&'a str>, &'static str> {
	let mut attributes = session.attributes.iter().filter(|a| a.attribute == name);
	let Some(attribute) = attributes.next() else {
		return Ok(None);
	};
	if attributes.next().is_some() {
		return Err("Duplicate protocol attribute");
	}
	let value = attribute.value.as_deref().map(str::trim).filter(|v| !v.is_empty());
	value.map(Some).ok_or("Empty protocol attribute")
}

fn negotiated_pyrowave_dialect(
	codec: VideoCodec,
	session: &sdp_types::Session,
) -> Result<Option<PyroWaveDialect>, &'static str> {
	if codec != VideoCodec::PyroWave {
		return Ok(None);
	}
	PyroWaveDialect::negotiate(
		unique_sdp_attribute(session, "x-ss-pyrowave.version")?,
		unique_sdp_attribute(session, "x-ss-pyrowave.dialect")?,
		unique_sdp_attribute(session, "x-ss-pyrowave.bitstream")?,
		unique_sdp_attribute(session, "x-ss-video[0].pyrowaveFeatures")?,
		unique_sdp_attribute(session, "x-ss-video[0].pyrowaveAdaptiveFec")?,
	)
	.map(Some)
}

fn get_optional_sdp_attribute<F: FromStr>(sdp_session: &sdp_types::Session, attribute: &str) -> Option<F> {
	sdp_session
		.get_first_attribute_value(attribute)
		.ok()
		.flatten()
		.map(|s| s.trim())
		.and_then(|s| s.parse().ok())
}

// Missing legacy attributes have defaults; malformed or unknown values do not.
fn negotiated_video_axes(
	session: &sdp_types::Session,
) -> Result<(VideoDynamicRange, VideoChromaSampling, BitDepth), ()> {
	fn optional_u32(session: &sdp_types::Session, name: &str) -> Result<Option<u32>, ()> {
		unique_sdp_attribute(session, name)
			.map_err(|_| ())?
			.map(|value| value.parse().map_err(|_| ()))
			.transpose()
	}
	let dynamic = VideoDynamicRange::try_from(optional_u32(session, "x-nv-video[0].dynamicRangeMode")?.unwrap_or(0))?;
	let chroma =
		VideoChromaSampling::try_from(optional_u32(session, "x-ss-video[0].chromaSamplingType")?.unwrap_or(0))?;
	let depth = match optional_u32(session, "x-moonshine-video[0].bitDepth")? {
		Some(value) => BitDepth::try_from(value)?,
		None if dynamic == VideoDynamicRange::Hdr => BitDepth::Ten,
		None => BitDepth::Eight,
	};
	Ok((dynamic, chroma, depth))
}

fn bitrate_bps_from_kbps(bitrate_kbps: u64) -> Option<usize> {
	bitrate_kbps
		.checked_mul(1000)
		.and_then(|bitrate| usize::try_from(bitrate).ok())
}

fn get_sdp_attribute<F: FromStr>(sdp_session: &sdp_types::Session, attribute: &str) -> Result<F, ()> {
	sdp_session
		.get_first_attribute_value(attribute)
		.map_err(|e| tracing::warn!("Failed to attribute {attribute} from request: {e}"))?
		.ok_or_else(|| tracing::warn!("No {attribute} attribute in request"))?
		.trim()
		.parse()
		.map_err(|_| tracing::warn!("Attribute {attribute} can't be parsed."))
}

#[cfg(test)]
mod tests {
	use super::bitrate_bps_from_kbps;

	#[test]
	fn negotiation_fixtures_and_conventional_isolation() {
		use super::*;
		let base = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=fixture\r\nt=0 0\r\n";
		for (attributes, expected) in [
			(
				include_str!("../tests/protocol/native-announce.sdp"),
				PyroWaveDialect::NativeWireV1,
			),
			(
				include_str!("../tests/protocol/nonary-announce.sdp"),
				PyroWaveDialect::RecordFramed,
			),
		] {
			let session = sdp_types::Session::parse(attributes.as_bytes()).unwrap();
			assert_eq!(
				negotiated_pyrowave_dialect(VideoCodec::PyroWave, &session),
				Ok(Some(expected))
			);
		}
		for attributes in [
			"",
			"a=x-ss-pyrowave.dialect:native-wire-v1\r\n",
			"a=x-ss-pyrowave.version:2\r\n",
			"a=x-ss-pyrowave.version:1\r\na=x-ss-pyrowave.version:1\r\n",
			"a=x-ss-pyrowave.version:1\r\na=x-ss-pyrowave.dialect:unknown\r\n",
			"a=x-ss-pyrowave.version:1\r\na=x-ss-pyrowave.bitstream:unknown\r\n",
			"a=x-ss-pyrowave.version\r\n",
		] {
			let session = sdp_types::Session::parse(format!("{base}{attributes}").as_bytes()).unwrap();
			assert!(negotiated_pyrowave_dialect(VideoCodec::PyroWave, &session).is_err());
			for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
				assert_eq!(negotiated_pyrowave_dialect(codec, &session), Ok(None));
			}
		}
		assert_eq!(pyrowave_profiles(CODEC_PYROWAVE), "420-sdr8");
		assert_eq!(
			pyrowave_profiles(CODEC_PYROWAVE_444 | CODEC_PYROWAVE_HDR),
			"444-sdr8 444-hdr10"
		);
	}

	#[test]
	fn profile_axes_preserve_hdr_chroma_and_explicit_sdr_depth() {
		use super::*;
		for hdr in [0, 1] {
			for chroma in [0, 1] {
				for depth in [8, 10] {
					let data = format!(
						"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\na=x-nv-video[0].dynamicRangeMode:{hdr}\r\na=x-ss-video[0].chromaSamplingType:{chroma}\r\na=x-moonshine-video[0].bitDepth:{depth}\r\n"
					);
					let session = sdp_types::Session::parse(data.as_bytes()).unwrap();
					assert_eq!(
						negotiated_video_axes(&session).unwrap(),
						(
							VideoDynamicRange::try_from(hdr).unwrap(),
							VideoChromaSampling::try_from(chroma).unwrap(),
							BitDepth::try_from(depth).unwrap()
						)
					);
				}
			}
		}
	}

	#[test]
	fn invalid_profile_attributes_are_rejected_and_absence_defaults() {
		use super::*;
		let base = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\n";
		let session = sdp_types::Session::parse(base.as_bytes()).unwrap();
		assert_eq!(
			negotiated_video_axes(&session).unwrap(),
			(VideoDynamicRange::Sdr, VideoChromaSampling::Yuv420, BitDepth::Eight)
		);
		for (attribute, value) in [
			("x-nv-video[0].dynamicRangeMode", "2"),
			("x-ss-video[0].chromaSamplingType", "2"),
			("x-moonshine-video[0].bitDepth", "12"),
			("x-nv-video[0].dynamicRangeMode", "hdr"),
			("x-ss-video[0].chromaSamplingType", "444"),
			("x-moonshine-video[0].bitDepth", "ten"),
		] {
			let data = format!("{base}a={attribute}:{value}\r\n");
			assert!(negotiated_video_axes(&sdp_types::Session::parse(data.as_bytes()).unwrap()).is_err());
		}
	}

	#[test]
	fn bitrate_conversion_is_wide_and_checked() {
		for mbps in [400u64, 424, 425, 426, 500, 750, 1_000, 2_000] {
			assert_eq!(bitrate_bps_from_kbps(mbps * 1000), Some((mbps * 1_000_000) as usize));
		}
		if usize::BITS >= 64 {
			assert_eq!(bitrate_bps_from_kbps(4_294_967), Some(4_294_967_000));
			assert_eq!(bitrate_bps_from_kbps(4_294_968), Some(4_294_968_000));
		}
		let max_safe_kbps = (usize::MAX as u64) / 1000;
		assert_eq!(
			bitrate_bps_from_kbps(max_safe_kbps),
			Some((max_safe_kbps * 1000) as usize)
		);
		assert_eq!(bitrate_bps_from_kbps(max_safe_kbps + 1), None);
		assert_eq!(bitrate_bps_from_kbps(u64::MAX), None);
		assert_eq!(bitrate_bps_from_kbps(u64::MAX / 1000 + 1), None);
	}
}
