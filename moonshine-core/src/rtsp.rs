use crate::session::stream::video::pyrowave_protocol::{BITSTREAM_ID, PyroWaveDialect};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use rtsp_types::Method;
use rtsp_types::headers;
use rtsp_types::headers::Transport;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use crate::ShutdownReason;
use crate::healthcheck::{
	CODEC_PYROWAVE, CODEC_PYROWAVE_444, CODEC_PYROWAVE_HDR, CODEC_PYROWAVE_MASK, supports_video_format,
};
use crate::ingress::Ingress;
use crate::session::authorization::{ML_FF_SESSION_ID_V1, MediaStream, StreamAuthorization, canonical_ip};
use crate::session::manager::SessionManager;
use crate::session::negotiation;
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

/// Concurrent RTSP connections. Moonlight sends one request per connection,
/// sequentially; the headroom only absorbs abandoned connections.
const MAX_RTSP_CONNECTIONS: usize = 16;
/// Bound for the request line and headers.
const MAX_RTSP_HEADER_BYTES: usize = 16 * 1024;
/// Bound for a request body (Moonlight's ANNOUNCE SDP is a few KiB).
const MAX_RTSP_BODY_BYTES: usize = 64 * 1024;

/// Deadlines for one RTSP connection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RtspLimits {
	/// Time to receive the complete request after the connection is accepted.
	pub request_timeout: Duration,
	/// Time to deliver the response.
	pub response_timeout: Duration,
}

impl Default for RtspLimits {
	fn default() -> Self {
		Self {
			request_timeout: Duration::from_secs(10),
			response_timeout: Duration::from_secs(10),
		}
	}
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
	limits: RtspLimits,
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
			limits: RtspLimits::default(),
		};

		tokio::spawn({
			let server = server.clone();
			async move {
				let ingress = Ingress::new("rtsp", MAX_RTSP_CONNECTIONS, shutdown.clone());
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
							server.serve_listener(listener, ingress).await
						}
					}))
					.await;

				tracing::debug!("RTSP server shutting down.");
			}
		});

		server
	}

	/// Server without a listener, for tests that drive [`Self::serve_listener`].
	#[cfg(test)]
	pub(crate) fn for_test(session_manager: SessionManager, limits: RtspLimits) -> Self {
		Self {
			address: "127.0.0.1".into(),
			rtsp_port: 0,
			video_config: Default::default(),
			audio_config: Default::default(),
			control_config: Default::default(),
			session_manager,
			supported_codecs: 0,
			limits,
		}
	}

	/// Accept connections, each handled in its own bounded, cancellable task.
	pub(crate) async fn serve_listener(&self, listener: TcpListener, ingress: Ingress) -> Result<(), ()> {
		loop {
			let (connection, address) = listener
				.accept()
				.await
				.map_err(|e| tracing::error!("Failed to accept connection: {}", e))?;
			tracing::trace!("Accepted connection from {}", address);

			let Some(permit) = ingress.admit(address) else {
				continue;
			};
			let server = self.clone();
			permit.spawn(async move {
				let _ = server.handle_connection(connection, address).await;
			});
		}
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

	fn handle_setup_request(
		&self,
		request: &rtsp_types::Request<Vec<u8>>,
		cseq: i32,
		grant: &StreamAuthorization,
	) -> rtsp_types::Response<Vec<u8>> {
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

					let mut response = rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
						.header(headers::CSEQ, cseq.to_string())
						.header(headers::SESSION, "MoonshineSession;timeout = 90".to_string())
						.header(headers::TRANSPORT, format!("server_port={port}"));
					// Sunshine session identifiers (`ML_FF_SESSION_ID_V1`): supporting
					// clients echo them in media PINGs and ENet connect data. Moonlight
					// matches option names case-sensitively.
					response = match stream_id {
						"audio" => response.header(PING_PAYLOAD_HEADER.clone(), grant.ping_payload(MediaStream::Audio)),
						"video" => response.header(PING_PAYLOAD_HEADER.clone(), grant.ping_payload(MediaStream::Video)),
						_ => response.header(CONNECT_DATA_HEADER.clone(), grant.control_connect_data().to_string()),
					};
					return response.build(Vec::new());
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
		grant: &StreamAuthorization,
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
		// Parse the client's encryption flags from the ANNOUNCE SDP. The packet
		// size cap depends on whether video carries the encryption prefix.
		let client_encryption_flags: u8 =
			get_optional_sdp_attribute(&sdp_session, "x-ss-general.encryptionEnabled").unwrap_or(0);
		let encrypt_video = self.video_config.encrypt && (client_encryption_flags & EncryptionFlags::Video as u8 != 0);
		let packet_size = self
			.video_config
			.clamp_packet_size(requested_packet_size, encrypt_video);
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
			encrypt_video,
		};
		// Reject before the session manager pauses or reconfigures anything.
		if let Err(reason) = video_stream_context.validate() {
			tracing::warn!(reason, "Rejecting invalid video negotiation");
			return bad_request(cseq, request.version(), &reason);
		}

		let packet_duration: u32 = match get_sdp_attribute(&sdp_session, "x-nv-aqos.packetDuration") {
			Ok(packet_duration) => packet_duration,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-aqos.packetDuration in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};
		// Unsupported durations used to fall back to 5 ms silently.
		if let Err(reason) = negotiation::validate_audio_packet_duration(packet_duration) {
			tracing::warn!(reason, "Rejecting invalid audio negotiation");
			return bad_request(cseq, request.version(), &reason);
		}
		let audio_qos_type: String = match get_sdp_attribute(&sdp_session, "x-nv-aqos.qosTrafficType") {
			Ok(audio_qos_type) => audio_qos_type,
			Err(()) => {
				tracing::warn!("Failed to parse x-nv-aqos.qosTrafficType in SDP session.");
				return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::BadRequest);
			},
		};

		// Parse surround audio attributes from SDP.
		let surround_channels: u32 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.numChannels").unwrap_or(2);
		let surround_mask: u32 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.channelMask").unwrap_or(0x3);
		let surround_enable: u8 = get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.enable").unwrap_or(0);
		let audio_quality: u8 =
			get_optional_sdp_attribute(&sdp_session, "x-nv-audio.surround.AudioQuality").unwrap_or(1);

		let (channels, channel_mask) = if surround_enable != 0 {
			match negotiation::audio_channels(surround_channels) {
				Ok(channels) => (channels, surround_mask),
				Err(reason) => {
					tracing::warn!(reason, "Rejecting invalid audio negotiation");
					return bad_request(cseq, request.version(), &reason);
				},
			}
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

		let client_features: u32 = get_optional_sdp_attribute(&sdp_session, "x-ml-general.featureFlags").unwrap_or(0);
		let session_id_v1 = client_features & ML_FF_SESSION_ID_V1 != 0;

		if self
			.session_manager
			.set_stream_context(grant, video_stream_context, audio_stream_context, session_id_v1)
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
		grant: &StreamAuthorization,
	) -> rtsp_types::Response<Vec<u8>> {
		if self.session_manager.start_session(grant).await.is_err() {
			return rtsp_response(cseq, request.version(), rtsp_types::StatusCode::InternalServerError);
		}

		rtsp_types::Response::builder(request.version(), rtsp_types::StatusCode::Ok)
			.header(headers::CSEQ, cseq.to_string())
			.build(Vec::new())
	}

	async fn handle_connection(&self, mut connection: TcpStream, address: SocketAddr) -> Result<(), ()> {
		// Gate RTSP access: only the client that authenticated the current
		// /launch or /resume generation may negotiate. The grant ties every
		// ANNOUNCE/PLAY to that generation; a later resume invalidates it.
		let Some(grant) = self.session_manager.authorize_stream(canonical_ip(address.ip())).await else {
			tracing::warn!(
				"Rejected RTSP connection from {}: no session authorized for this client",
				address
			);
			return Ok(());
		};

		let request = match tokio::time::timeout(self.limits.request_timeout, read_rtsp_request(&mut connection)).await
		{
			Ok(Ok(request)) => request,
			Ok(Err(error)) => {
				tracing::warn!(%address, ?error, "Rejected RTSP request");
				return Err(());
			},
			Err(_) => {
				tracing::warn!(%address, "RTSP request was not received in time");
				return Err(());
			},
		};
		let request = normalize_request_target(request);
		tracing::trace!("Request: {}", String::from_utf8_lossy(&request));

		let message = match rtsp_types::Message::parse(&request) {
			Ok((message, _consumed)) => message,
			Err(e) => {
				tracing::warn!("Failed to parse request as RTSP message: {}", e);
				return Err(());
			},
		};

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
					Method::Announce => self.handle_announce_request(request, cseq, &grant).await,
					Method::Describe => self.handle_describe_request(request, cseq).await,
					Method::Options => self.handle_options_request(request, cseq),
					Method::Setup => self.handle_setup_request(request, cseq, &grant),
					Method::Play => self.handle_play_request(request, cseq, &grant).await,
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

		let respond = async {
			connection.write_all(&buffer).await?;
			// For some reason, Moonlight expects a connection per request, so we close the connection here.
			connection.shutdown().await
		};
		match tokio::time::timeout(self.limits.response_timeout, respond).await {
			Ok(Ok(())) => Ok(()),
			Ok(Err(e)) => {
				tracing::warn!("Failed to send RTSP response: {e}");
				Err(())
			},
			Err(_) => {
				tracing::warn!(%address, "RTSP response was not delivered in time");
				Err(())
			},
		}
	}
}

static PING_PAYLOAD_HEADER: std::sync::LazyLock<headers::HeaderName> =
	std::sync::LazyLock::new(|| headers::HeaderName::from_static_str("X-SS-Ping-Payload").expect("ASCII header"));
static CONNECT_DATA_HEADER: std::sync::LazyLock<headers::HeaderName> =
	std::sync::LazyLock::new(|| headers::HeaderName::from_static_str("X-SS-Connect-Data").expect("ASCII header"));

/// Why an RTSP request could not be framed.
#[derive(Debug, PartialEq, Eq)]
enum RtspRequestError {
	/// The peer closed the connection before sending a complete request.
	Incomplete,
	HeaderTooLarge,
	BodyTooLarge,
	InvalidContentLength,
	Io(std::io::ErrorKind),
}

/// Incremental, bounded framing of one RTSP request (request line, headers
/// and a `Content-Length` body). Bytes are scanned once; nothing is parsed or
/// copied until the request is complete.
#[derive(Default)]
struct RtspRequestFramer {
	buffer: Vec<u8>,
	scanned: usize,
	total_length: Option<usize>,
}

impl RtspRequestFramer {
	/// Append received bytes; returns the complete request once available.
	fn push(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>, RtspRequestError> {
		self.buffer.extend_from_slice(data);
		if self.total_length.is_none() {
			// Resume the terminator search across reads that split it.
			let from = self.scanned.saturating_sub(3);
			match find_subsequence(&self.buffer[from..], b"\r\n\r\n") {
				Some(position) => {
					let header_length = from + position + 4;
					if header_length > MAX_RTSP_HEADER_BYTES {
						return Err(RtspRequestError::HeaderTooLarge);
					}
					let body_length = content_length(&self.buffer[..header_length])?;
					if body_length > MAX_RTSP_BODY_BYTES {
						return Err(RtspRequestError::BodyTooLarge);
					}
					self.total_length = Some(header_length + body_length);
				},
				None if self.buffer.len() > MAX_RTSP_HEADER_BYTES => return Err(RtspRequestError::HeaderTooLarge),
				None => {
					self.scanned = self.buffer.len();
					return Ok(None);
				},
			}
		}
		match self.total_length {
			Some(total) if self.buffer.len() >= total => {
				// One request per connection; ignore anything after it.
				self.buffer.truncate(total);
				Ok(Some(std::mem::take(&mut self.buffer)))
			},
			_ => Ok(None),
		}
	}
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|window| window == needle)
}

/// Body length declared by the request headers (absent means no body).
fn content_length(head: &[u8]) -> Result<usize, RtspRequestError> {
	let mut length = None;
	for line in head.split(|&byte| byte == b'\n').skip(1) {
		let line = line.strip_suffix(b"\r").unwrap_or(line);
		let Some(colon) = line.iter().position(|&byte| byte == b':') else {
			continue;
		};
		if !line[..colon].trim_ascii().eq_ignore_ascii_case(b"content-length") {
			continue;
		}
		let value = line[colon + 1..].trim_ascii();
		if value.is_empty() || value.len() > 10 || !value.iter().all(u8::is_ascii_digit) {
			return Err(RtspRequestError::InvalidContentLength);
		}
		let value: usize = std::str::from_utf8(value)
			.ok()
			.and_then(|value| value.parse().ok())
			.ok_or(RtspRequestError::InvalidContentLength)?;
		if length.is_some_and(|length| length != value) {
			return Err(RtspRequestError::InvalidContentLength);
		}
		length = Some(value);
	}
	Ok(length.unwrap_or(0))
}

async fn read_rtsp_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, RtspRequestError> {
	let mut framer = RtspRequestFramer::default();
	let mut chunk = [0u8; 4096];
	loop {
		let read = reader
			.read(&mut chunk)
			.await
			.map_err(|error| RtspRequestError::Io(error.kind()))?;
		if read == 0 {
			return Err(RtspRequestError::Incomplete);
		}
		if let Some(request) = framer.push(&chunk[..read])? {
			return Ok(request);
		}
	}
}

/// Rewrite Moonlight's request targets into URIs `rtsp_types` accepts: SETUP and
/// ANNOUNCE use a bare `streamid=...` and PLAY uses `/`. Only the request line
/// is changed; headers and the SDP body are passed through untouched.
fn normalize_request_target(request: Vec<u8>) -> Vec<u8> {
	let Some(line_end) = find_subsequence(&request, b"\r\n") else {
		return request;
	};
	let Ok(line) = std::str::from_utf8(&request[..line_end]) else {
		return request;
	};
	let mut parts = line.splitn(3, ' ');
	let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
		return request;
	};
	let target = if target.starts_with("streamid") {
		format!("rtsp://localhost?{target}")
	} else if method == "PLAY" && target.starts_with('/') {
		format!("rtsp://localhost{target}")
	} else {
		return request;
	};
	let mut normalized = format!("{method} {target} {version}").into_bytes();
	normalized.extend_from_slice(&request[line_end..]);
	normalized
}

/// A 400 response whose body tells the client which value was rejected.
fn bad_request(cseq: i32, version: rtsp_types::Version, reason: &str) -> rtsp_types::Response<Vec<u8>> {
	rtsp_types::Response::builder(version, rtsp_types::StatusCode::BadRequest)
		.header(headers::CSEQ, cseq.to_string())
		.header(headers::CONTENT_TYPE, "text/plain")
		.build(reason.as_bytes().to_vec())
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

	/// CFG-001: ANNOUNCE rejects values that timing, allocation, transport or
	/// audio code cannot honor with a 400 before the session manager sees them,
	/// while admitting high-end 4K/high-refresh/high-bitrate requests.
	mod negotiation_domains {
		use super::super::*;
		use crate::healthcheck::CODEC_HEVC;

		fn sdp(overrides: &[(&str, &str)]) -> String {
			let mut attributes = vec![
				("x-nv-video[0].clientViewportWd", "3840"),
				("x-nv-video[0].clientViewportHt", "2160"),
				("x-nv-video[0].maxFPS", "120"),
				("x-nv-video[0].packetSize", "1392"),
				("x-ml-video.configuredBitrateKbps", "900000"),
				("x-nv-vqos[0].fec.minRequiredFecPackets", "2"),
				("x-nv-vqos[0].qosTrafficType", "5"),
				("x-nv-vqos[0].bitStreamFormat", "1"),
				("x-nv-aqos.packetDuration", "5"),
				("x-nv-aqos.qosTrafficType", "4"),
				("x-ss-general.encryptionEnabled", "7"),
			];
			for (name, value) in overrides {
				match attributes.iter_mut().find(|(existing, _)| existing == name) {
					Some(attribute) => attribute.1 = value,
					None => attributes.push((name, value)),
				}
			}
			let mut body = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\n".to_string();
			for (name, value) in attributes {
				body.push_str(&format!("a={name}:{value}\r\n"));
			}
			body
		}

		async fn announce(server: &RtspServer, grant: &StreamAuthorization, body: String) -> (u16, String) {
			let request = rtsp_types::Request::builder(Method::Announce, rtsp_types::Version::V1_0)
				.header(headers::CSEQ, "6")
				.build(body.into_bytes());
			let response = server.handle_announce_request(&request, 6, grant).await;
			(
				response.status().into(),
				String::from_utf8_lossy(response.body()).into_owned(),
			)
		}

		#[tokio::test]
		async fn announce_validates_numeric_domains_before_the_session() {
			let shutdown = ShutdownManager::new();
			let manager = SessionManager::for_test(shutdown.clone());
			let grant = manager.authorize_client_for_test("127.0.0.1".parse().unwrap()).await;
			let mut server = RtspServer::for_test(manager, RtspLimits::default());
			server.supported_codecs = CODEC_HEVC;
			server.video_config.encrypt = true;

			// Valid requests pass validation and reach the manager, which refuses
			// them only because no session was launched (500, not 400).
			for overrides in [
				&[][..],
				&[("x-nv-video[0].maxFPS", "240")][..],
				&[("x-ml-video.configuredBitrateKbps", "650000")][..],
				&[
					("x-nv-video[0].clientViewportWd", "7680"),
					("x-nv-video[0].clientViewportHt", "4320"),
				][..],
				&[("x-nv-aqos.packetDuration", "10")][..],
				&[
					("x-nv-audio.surround.enable", "1"),
					("x-nv-audio.surround.numChannels", "8"),
				][..],
				// Encrypted jumbo packets that still fit one datagram.
				&[("x-nv-video[0].packetSize", "65459")][..],
			] {
				let (status, body) = announce(&server, &grant, sdp(overrides)).await;
				assert_eq!(status, 500, "{overrides:?}: {body}");
			}

			for (overrides, reason) in [
				(&[("x-nv-video[0].maxFPS", "0")][..], "refresh rate"),
				(&[("x-nv-video[0].clientViewportWd", "0")][..], "width"),
				(&[("x-nv-video[0].clientViewportHt", "100000")][..], "height"),
				(&[("x-nv-video[0].packetSize", "0")][..], "packet size"),
				(&[("x-nv-video[0].packetSize", "16")][..], "packet size"),
				// Fits without, but not with, the 32-byte encryption prefix.
				(&[("x-nv-video[0].packetSize", "65460")][..], "UDP datagram"),
				(
					&[("x-nv-video[0].packetSize", "18446744073709551615")][..],
					"UDP datagram",
				),
				(&[("x-ml-video.configuredBitrateKbps", "0")][..], "bitrate"),
				(&[("x-ml-video.configuredBitrateKbps", "5000000")][..], "32-bit"),
				(&[("x-nv-aqos.packetDuration", "20")][..], "audio packet duration"),
				(&[("x-nv-aqos.packetDuration", "0")][..], "audio packet duration"),
				(
					&[
						("x-nv-audio.surround.enable", "1"),
						("x-nv-audio.surround.numChannels", "4"),
					][..],
					"channel count",
				),
				(
					&[
						("x-nv-audio.surround.enable", "1"),
						("x-nv-audio.surround.numChannels", "258"),
					][..],
					"channel count",
				),
			] {
				let (status, body) = announce(&server, &grant, sdp(overrides)).await;
				assert_eq!(status, 400, "{overrides:?}: {body}");
				assert!(body.contains(reason), "{overrides:?}: {body}");
			}
			shutdown.trigger_shutdown(crate::ShutdownReason::AppQuit).unwrap();
		}
	}

	mod framing {
		use super::super::*;

		const OPTIONS: &[u8] = b"OPTIONS rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 1\r\n\r\n";

		fn announce(body: &str) -> Vec<u8> {
			format!(
				"ANNOUNCE streamid=control/13/0 RTSP/1.0\r\nCSeq: 6\r\nContent-type: application/sdp\r\nContent-length: {}\r\n\r\n{body}",
				body.len()
			)
			.into_bytes()
		}

		#[test]
		fn frames_complete_and_fragmented_requests() {
			let mut framer = RtspRequestFramer::default();
			assert_eq!(framer.push(OPTIONS), Ok(Some(OPTIONS.to_vec())));

			// Byte-by-byte delivery, including the terminator and a multi-byte
			// UTF-8 sequence split across reads, with trailing data ignored.
			let request = announce("s=caf\u{e9}\r\na=x-nv-video[0].maxFPS:60\r\n");
			let mut stream = request.clone();
			stream.extend_from_slice(b"TRAILING");
			let mut framer = RtspRequestFramer::default();
			let mut complete = None;
			for (index, byte) in stream.iter().enumerate() {
				if let Some(framed) = framer.push(std::slice::from_ref(byte)).unwrap() {
					assert_eq!(index + 1, request.len());
					complete = Some(framed);
					break;
				}
			}
			assert_eq!(complete, Some(request));
		}

		#[test]
		fn rejects_oversized_and_invalid_requests() {
			let mut framer = RtspRequestFramer::default();
			let header = vec![b'a'; MAX_RTSP_HEADER_BYTES];
			assert_eq!(framer.push(&header), Ok(None));
			assert_eq!(framer.push(b"a"), Err(RtspRequestError::HeaderTooLarge));

			// Declared bodies are bounded before any body bytes arrive.
			let mut framer = RtspRequestFramer::default();
			let request = format!(
				"ANNOUNCE streamid=x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
				MAX_RTSP_BODY_BYTES + 1
			);
			assert_eq!(framer.push(request.as_bytes()), Err(RtspRequestError::BodyTooLarge));

			for length in ["-1", "1x", "", "99999999999", "1, 2"] {
				let request = format!("ANNOUNCE x RTSP/1.0\r\nContent-Length: {length}\r\n\r\n");
				assert_eq!(
					RtspRequestFramer::default().push(request.as_bytes()),
					Err(RtspRequestError::InvalidContentLength),
					"{length:?}"
				);
			}
			let conflicting = b"ANNOUNCE x RTSP/1.0\r\nContent-Length: 1\r\ncontent-length: 2\r\n\r\nab";
			assert_eq!(
				RtspRequestFramer::default().push(conflicting),
				Err(RtspRequestError::InvalidContentLength)
			);
		}

		#[tokio::test]
		async fn incomplete_request_is_an_error() {
			let mut partial: &[u8] = b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n";
			assert_eq!(read_rtsp_request(&mut partial).await, Err(RtspRequestError::Incomplete));
			let mut short_body: &[u8] = &announce("v=0\r\n")[..60];
			assert_eq!(
				read_rtsp_request(&mut short_body).await,
				Err(RtspRequestError::Incomplete)
			);
		}

		#[test]
		fn normalizes_only_moonlight_request_targets() {
			let setup = b"SETUP streamid=video/0/0 RTSP/1.0\r\nX-Note: streamid\r\n\r\n".to_vec();
			assert_eq!(
				normalize_request_target(setup),
				b"SETUP rtsp://localhost?streamid=video/0/0 RTSP/1.0\r\nX-Note: streamid\r\n\r\n"
			);
			let play = b"PLAY / RTSP/1.0\r\nCSeq: 7\r\n\r\n".to_vec();
			assert_eq!(
				normalize_request_target(play),
				b"PLAY rtsp://localhost/ RTSP/1.0\r\nCSeq: 7\r\n\r\n"
			);
			let body = announce("a=streamid\r\nPLAY /\r\n");
			let normalized = normalize_request_target(body.clone());
			assert!(normalized.starts_with(b"ANNOUNCE rtsp://localhost?streamid=control/13/0 RTSP/1.0\r\n"));
			assert!(normalized.ends_with(b"a=streamid\r\nPLAY /\r\n"), "body untouched");
			assert_eq!(normalize_request_target(OPTIONS.to_vec()), OPTIONS);
			assert_eq!(
				normalize_request_target(b"\xff\xfe /x y\r\n".to_vec()),
				b"\xff\xfe /x y\r\n"
			);
		}
	}

	mod server {
		use std::time::{Duration, Instant};

		use async_shutdown::ShutdownManager;
		use tokio::io::{AsyncReadExt, AsyncWriteExt};
		use tokio::net::{TcpListener, TcpSocket, TcpStream};

		use super::super::*;
		use crate::session::authorization::MediaStream;

		const LIMITS: RtspLimits = RtspLimits {
			request_timeout: Duration::from_millis(300),
			response_timeout: Duration::from_millis(300),
		};

		async fn start() -> (
			std::net::SocketAddr,
			StreamAuthorization,
			ShutdownManager<crate::ShutdownReason>,
		) {
			let shutdown = ShutdownManager::new();
			let manager = SessionManager::for_test(shutdown.clone());
			let grant = manager.authorize_client_for_test("127.0.0.1".parse().unwrap()).await;
			let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
			let address = listener.local_addr().unwrap();
			let server = RtspServer::for_test(manager, LIMITS);
			let ingress = Ingress::new("rtsp-test", MAX_RTSP_CONNECTIONS, shutdown.clone());
			let cancel = shutdown.clone();
			tokio::spawn(async move { cancel.wrap_cancel(server.serve_listener(listener, ingress)).await });
			(address, grant, shutdown)
		}

		async fn connect_from(source: &str, server: std::net::SocketAddr) -> TcpStream {
			let socket = TcpSocket::new_v4().unwrap();
			socket.bind(format!("{source}:0").parse().unwrap()).unwrap();
			socket.connect(server).await.unwrap()
		}

		async fn exchange(source: &str, server: std::net::SocketAddr, request: &[u8]) -> String {
			let mut stream = connect_from(source, server).await;
			stream.write_all(request).await.unwrap();
			let mut response = Vec::new();
			// A refused connection may be reset because its request was never read.
			let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
				.await
				.unwrap();
			String::from_utf8(response).unwrap()
		}

		#[tokio::test]
		async fn only_the_authorized_client_negotiates() {
			let (server, grant, shutdown) = start().await;
			let options = b"OPTIONS rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 1\r\n\r\n";
			assert_eq!(exchange("127.0.0.2", server, options).await, "", "unauthorized host");
			assert!(
				exchange("127.0.0.1", server, options)
					.await
					.starts_with("RTSP/1.0 200 ")
			);

			// SETUP hands out the generation's Sunshine session identifiers.
			for (stream, header) in [
				(
					"audio",
					format!("X-SS-Ping-Payload: {}", grant.ping_payload(MediaStream::Audio)),
				),
				(
					"video",
					format!("X-SS-Ping-Payload: {}", grant.ping_payload(MediaStream::Video)),
				),
				(
					"control",
					format!("X-SS-Connect-Data: {}", grant.control_connect_data()),
				),
			] {
				let request = format!(
					"SETUP streamid={stream}/0/0 RTSP/1.0\r\nCSeq: 3\r\nTransport: unicast;X-GS-ClientPort=50000-50001\r\nIf-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n\r\n"
				);
				let response = exchange("127.0.0.1", server, request.as_bytes()).await;
				assert!(response.starts_with("RTSP/1.0 200 "), "{response}");
				assert!(response.contains(&format!("{header}\r\n")), "{response}");
			}

			// ANNOUNCE and PLAY without an initialized session are refused rather
			// than committing anything.
			let play = b"PLAY / RTSP/1.0\r\nCSeq: 7\r\n\r\n";
			assert!(exchange("127.0.0.1", server, play).await.starts_with("RTSP/1.0 500"));
			shutdown.trigger_shutdown(crate::ShutdownReason::AppQuit).unwrap();
		}

		#[tokio::test]
		async fn stalled_and_oversized_requests_are_bounded() {
			let (server, _grant, shutdown) = start().await;
			// A stalled request does not delay other clients and is closed at its deadline.
			let mut stalled = connect_from("127.0.0.1", server).await;
			stalled
				.write_all(b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n")
				.await
				.unwrap();
			let started = Instant::now();
			let options = b"OPTIONS rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 2\r\n\r\n";
			let response = exchange("127.0.0.1", server, options).await;
			assert!(response.starts_with("RTSP/1.0 200 "), "{response:?}");
			assert!(started.elapsed() < LIMITS.request_timeout);
			let mut rest = Vec::new();
			tokio::time::timeout(Duration::from_secs(2), stalled.read_to_end(&mut rest))
				.await
				.expect("stalled request is closed at its deadline")
				.unwrap();
			assert!(rest.is_empty());

			// An oversized header is rejected as soon as it exceeds the bound.
			let mut oversized = connect_from("127.0.0.1", server).await;
			let started = Instant::now();
			let _ = oversized.write_all(&vec![b'a'; MAX_RTSP_HEADER_BYTES + 4096]).await;
			let mut rest = Vec::new();
			let _ = tokio::time::timeout(Duration::from_secs(2), oversized.read_to_end(&mut rest)).await;
			assert!(started.elapsed() < LIMITS.request_timeout, "closed before the deadline");
			assert!(rest.is_empty());

			// A request delivered byte by byte is still served.
			let mut fragmented = connect_from("127.0.0.1", server).await;
			for byte in options {
				fragmented.write_all(std::slice::from_ref(byte)).await.unwrap();
			}
			let mut response = Vec::new();
			fragmented.read_to_end(&mut response).await.unwrap();
			assert!(response.starts_with(b"RTSP/1.0 200 "));
			shutdown.trigger_shutdown(crate::ShutdownReason::AppQuit).unwrap();
		}
	}

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
