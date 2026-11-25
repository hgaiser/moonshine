//! Streaming to the browser over WebTransport.
//!
//! The page asks for a token (`POST /api/stream/token`), opens a WebTransport session at
//! `https://<host>:<port>/stream?token=<token>` on the control panel's port (UDP) and opens one
//! bidirectional stream for control messages. Its first message is `start`, with what it wants to
//! stream; only then does the [`Backend`](crate::Backend) start or join a session.
//!
//! # Wire protocol
//!
//! All numbers are little-endian.
//!
//! Datagrams from the server:
//!
//! | Type | Layout |
//! |------|--------|
//! | 1 video | type u8, flags u8 (bit 0 key frame, bit 1 recovery frame), frame u32, block u8, blocks u8, shard index u8, data shards u8, parity shards u8, reserved u8, first data shard u16, frame length u32, timestamp u64 (µs), shard |
//! | 2 audio | type u8, sequence u32, timestamp u64 (µs), Opus packet |
//!
//! A video frame is cut into shards of one size (the last one padded with zeros), in blocks of
//! at most 255 shards. Each block has its data shards (index `0..data`), then its Reed-Solomon
//! parity shards (index `data..data + parity`), as `fec-rs` (Moonlight's code) makes them. A block
//! can be rebuilt from any `data` of its shards. The shard size is the datagram's length minus the
//! header; data shard `i` of a block is at byte `(first + i) * shard size` of the frame.
//!
//! Control messages, on the bidirectional stream: length u32 (of what follows), type u8, payload.
//!
//! | From | Type | Payload |
//! |------|------|---------|
//! | browser | 1 start | first: JSON [`StreamRequest`]; then, empty, when it is ready for the media |
//! | browser | 2 input | a Moonlight input packet (event type u32 and payload) |
//! | browser | 3 key frame | none: a frame was lost |
//! | browser | 4 stop | none: stop the session |
//! | browser | 5 ping | browser time f64 (ms) |
//! | server | 1 ready | JSON [`StreamParams`] |
//! | server | 2 feedback | a Moonlight feedback packet (rumble and so on) |
//! | server | 3 ended | the reason, UTF-8 |
//! | server | 4 pong | the browser time f64 from the ping, server time u64 (µs) |
//! | server | 5 error | what went wrong, UTF-8 |

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use wtransport::endpoint::IncomingSession;
use wtransport::error::SendDatagramError;
use wtransport::quinn::congestion;
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig};

use crate::Backend;
use crate::access::{self, Network};

/// A video codec, as the browser and the backend name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
	H264,
	Hevc,
	Av1,
}

/// What the browser wants to stream.
#[derive(Clone, Debug, Deserialize)]
pub struct StreamRequest {
	/// The application's ID, as Moonlight knows it. Ignored when the browser joins a running session.
	pub application_id: i32,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub bitrate_kbps: u32,
	/// The codecs that the browser can decode.
	pub codecs: Vec<VideoCodec>,
	/// The codecs that the browser can decode in 10 bits (for HDR).
	#[serde(default)]
	pub codecs_10bit: Vec<VideoCodec>,
	/// Whether a new session streams HDR (HDR10: 10 bits, BT.2020, PQ).
	#[serde(default)]
	pub hdr: bool,
	/// Audio channels for a new session: 2, 6 (5.1) or 8 (7.1).
	#[serde(default = "stereo")]
	pub audio_channels: u8,
}

fn stereo() -> u8 {
	2
}

/// The stream as the backend started it.
#[derive(Clone, Debug, Serialize)]
pub struct StreamParams {
	pub codec: VideoCodec,
	/// Whether the session is HDR: 10-bit video, whose frames are BT.2020 PQ while the content is
	/// HDR and BT.709 otherwise (the bitstream says which).
	pub hdr: bool,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub bitrate_kbps: u32,
	/// Parity shards per 100 data shards of video (at least 2 per block); 0 for none.
	pub fec_percent: u8,
	pub audio_channels: u8,
	/// Opus multistream streams and coupled streams.
	pub audio_streams: u8,
	pub audio_coupled_streams: u8,
	/// Opus multistream mapping, in WAVE channel order (FL FR FC LFE RL RR SL SR). A decoder that
	/// takes an `OpusHead` wants it in Vorbis order.
	pub audio_mapping: Vec<u8>,
	pub audio_sample_rate: u32,
}

/// An encoded video frame.
#[derive(Debug)]
pub struct VideoFrame {
	pub data: Vec<u8>,
	pub key_frame: bool,
	pub recovery: bool,
	pub number: u32,
	pub captured_at: Instant,
}

/// What the browser asks of the backend.
#[derive(Debug)]
pub enum ClientEvent {
	/// The browser is ready for the stream.
	Start,
	/// A Moonlight input packet.
	Input(Vec<u8>),
	/// A frame was lost.
	RequestKeyFrame,
	/// Stop the session.
	Stop,
}

/// What the backend tells the browser, besides video and audio.
#[derive(Debug)]
pub enum ServerEvent {
	/// A Moonlight feedback packet.
	Feedback(Vec<u8>),
	/// The stream ended, with the reason.
	Ended(String),
}

/// A stream of items from the backend.
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// A running stream for one browser. Dropping it detaches the browser.
pub struct StreamLink {
	pub params: StreamParams,
	pub video: BoxStream<VideoFrame>,
	/// Opus packets.
	pub audio: BoxStream<Vec<u8>>,
	pub events: BoxStream<ServerEvent>,
	pub commands: mpsc::Sender<ClientEvent>,
}

/// How long a token is valid.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30);

/// How long a self-signed certificate is used. Browsers accept a certificate by its hash only when
/// it is valid for at most 14 days, so a new one replaces it before that.
const CERTIFICATE_ROTATION: Duration = Duration::from_secs(10 * 24 * 60 * 60);

/// The size of the datagram send buffer. Larger than a key frame at a high bitrate; the sender
/// waits long before it fills (see [`MAX_QUEUED_FRAMES`]), because a full buffer drops the oldest
/// datagrams.
const DATAGRAM_SEND_BUFFER: usize = 8 * 1024 * 1024;

/// The sender waits while more than this many average frames are queued. Then the backend's
/// pipeline drops captures before it encodes them, which keeps the stream decodable.
const MAX_QUEUED_FRAMES: usize = 2;

/// The longest the sender waits for room for one frame before it drops it.
const MAX_FRAME_WAIT: Duration = Duration::from_millis(500);

/// The largest control message from the browser.
const MAX_CONTROL_MESSAGE: usize = 64 * 1024;

/// The header of a video datagram.
const VIDEO_HEADER: usize = 1 + 1 + 4 + 1 + 1 + 1 + 1 + 1 + 1 + 2 + 4 + 8;

/// The fewest parity shards in a block, when FEC is on.
const MIN_PARITY_SHARDS: usize = 2;

/// The most shards (data and parity) in one block: GF(2^8) has 256 elements.
const MAX_BLOCK_SHARDS: usize = 255;

const DATAGRAM_VIDEO: u8 = 1;
const DATAGRAM_AUDIO: u8 = 2;

const CLIENT_START: u8 = 1;
const CLIENT_INPUT: u8 = 2;
const CLIENT_KEY_FRAME: u8 = 3;
const CLIENT_STOP: u8 = 4;
const CLIENT_PING: u8 = 5;

const SERVER_READY: u8 = 1;
const SERVER_FEEDBACK: u8 = 2;
const SERVER_ENDED: u8 = 3;
const SERVER_PONG: u8 = 4;
const SERVER_ERROR: u8 = 5;

/// What the page needs to open WebTransport.
#[derive(Debug, Serialize)]
pub(crate) struct TokenResponse {
	pub token: String,
	/// The address or name to connect to, or `None` for the one in the page's own URL.
	pub host: Option<String>,
	pub port: u16,
	/// SHA-256 of the self-signed certificate, for `serverCertificateHashes`. `None` with a
	/// certificate that the browser trusts.
	pub certificate_hash: Option<Vec<u8>>,
}

struct Token {
	/// The browser's address, or `None` when it is not known (behind a reverse proxy).
	peer: Option<IpAddr>,
	expires: Instant,
}

/// The WebTransport side of the control panel.
pub(crate) struct WebTransport {
	port: u16,
	/// The hash of the current self-signed certificate.
	certificate_hash: Mutex<Option<Vec<u8>>>,
	tokens: Mutex<HashMap<String, Token>>,
	/// The time base of the timestamps on the wire.
	epoch: Instant,
}

impl WebTransport {
	/// The UDP port of the stream.
	pub fn port(&self) -> u16 {
		self.port
	}

	/// A new single-use token for a page at `peer` that streams from `host`.
	pub fn issue_token(&self, peer: Option<IpAddr>, host: Option<String>) -> Result<TokenResponse, String> {
		let mut bytes = [0u8; 24];
		getrandom::fill(&mut bytes).map_err(|e| format!("Failed to make a token: {e}"))?;
		let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

		let now = Instant::now();
		let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
		tokens.retain(|_, token| token.expires > now);
		tokens.insert(
			token.clone(),
			Token {
				peer: peer.map(canonical),
				expires: now + TOKEN_LIFETIME,
			},
		);

		Ok(TokenResponse {
			token,
			host,
			port: self.port,
			certificate_hash: self
				.certificate_hash
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.clone(),
		})
	}

	/// Take a token, if it is valid and was issued to `peer`.
	fn redeem(&self, token: &str, peer: IpAddr) -> bool {
		let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
		match tokens.remove(token) {
			Some(entry) => {
				entry.expires > Instant::now() && entry.peer.is_none_or(|expected| same_peer(expected, canonical(peer)))
			},
			None => false,
		}
	}

	fn micros(&self, at: Instant) -> u64 {
		at.saturating_duration_since(self.epoch).as_micros() as u64
	}
}

/// Whether two addresses can be the same browser. The page and the stream can use different
/// address families (TCP over IPv4, QUIC over IPv6), so only addresses of one family are compared,
/// and all loopback addresses are one.
fn same_peer(a: IpAddr, b: IpAddr) -> bool {
	if a.is_loopback() || b.is_loopback() {
		return a.is_loopback() && b.is_loopback();
	}
	a.is_ipv4() != b.is_ipv4() || a == b
}

/// An IPv4 address in IPv6 notation is the IPv4 address.
fn canonical(ip: IpAddr) -> IpAddr {
	match ip {
		IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
		ip => ip,
	}
}

/// Where the WebTransport certificate comes from.
pub enum WebTransportCertificate {
	/// A self-signed certificate that is replaced every 10 days; pages trust it by its hash.
	SelfSigned { names: Vec<String> },
	/// PEM files of a certificate that browsers trust.
	Files {
		certificate: std::path::PathBuf,
		private_key: std::path::PathBuf,
	},
}

/// The settings that the WebTransport server shares with the rest of the control panel.
pub(crate) struct Access {
	pub allowed_networks: Vec<Network>,
	pub host_names: Vec<String>,
}

/// Start the WebTransport server on `addresses` (UDP).
pub(crate) async fn serve<B: Backend>(
	backend: Arc<B>,
	addresses: &[SocketAddr],
	port: u16,
	certificate: WebTransportCertificate,
	access: Arc<Access>,
) -> Result<(Arc<WebTransport>, Vec<JoinHandle<()>>), String> {
	let identity = match &certificate {
		WebTransportCertificate::SelfSigned { names } => self_signed(names)?,
		WebTransportCertificate::Files {
			certificate,
			private_key,
		} => Identity::load_pemfiles(certificate, private_key)
			.await
			.map_err(|e| format!("Failed to load the control panel certificate for streaming: {e}"))?,
	};
	let is_self_signed = matches!(certificate, WebTransportCertificate::SelfSigned { .. });

	let webtransport = Arc::new(WebTransport {
		port,
		certificate_hash: Mutex::new(is_self_signed.then(|| hash(&identity))),
		tokens: Mutex::new(HashMap::new()),
		epoch: Instant::now(),
	});

	let mut endpoints = Vec::new();
	let mut errors = Vec::new();
	for address in addresses {
		match Endpoint::server(server_config(*address, identity.clone_identity())) {
			Ok(endpoint) => endpoints.push((*address, Arc::new(endpoint))),
			Err(e) => errors.push(format!("{address}: {e}")),
		}
	}
	if endpoints.is_empty() {
		return Err(format!("Failed to listen for streams ({}).", errors.join(", ")));
	}

	let mut tasks = Vec::new();
	for (address, endpoint) in &endpoints {
		tracing::info!("Control panel listening for browser streams on udp://{address}");
		tasks.push(tokio::spawn(accept(
			endpoint.clone(),
			backend.clone(),
			webtransport.clone(),
			access.clone(),
		)));
	}

	if let WebTransportCertificate::SelfSigned { names } = certificate {
		let webtransport = webtransport.clone();
		tasks.push(tokio::spawn(async move {
			loop {
				tokio::time::sleep(CERTIFICATE_ROTATION).await;
				let identity = match self_signed(&names) {
					Ok(identity) => identity,
					Err(e) => {
						tracing::warn!("{e}");
						continue;
					},
				};
				for (address, endpoint) in &endpoints {
					if let Err(e) = endpoint.reload_config(server_config(*address, identity.clone_identity()), false) {
						tracing::warn!("Failed to replace the streaming certificate on {address}: {e}");
					}
				}
				*webtransport
					.certificate_hash
					.lock()
					.unwrap_or_else(PoisonError::into_inner) = Some(hash(&identity));
				tracing::debug!("Replaced the streaming certificate.");
			}
		}));
	}

	Ok((webtransport, tasks))
}

fn self_signed(names: &[String]) -> Result<Identity, String> {
	Identity::self_signed(names).map_err(|e| format!("Failed to make a certificate for streaming: {e}"))
}

fn hash(identity: &Identity) -> Vec<u8> {
	identity.certificate_chain().as_slice()[0].hash().as_ref().to_vec()
}

fn server_config(address: SocketAddr, identity: Identity) -> ServerConfig {
	let mut transport = wtransport::config::QuicTransportConfig::default();
	transport.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
	transport.datagram_receive_buffer_size(Some(256 * 1024));
	transport.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("a valid idle timeout")));
	// Keep the connection open while the application starts, which can take long.
	transport.keep_alive_interval(Some(Duration::from_secs(2)));
	transport.congestion_controller_factory(Arc::new(congestion::CubicConfig::default()));
	// An unspecified IPv4 address binds both IPv4 and IPv6: browsers often reach `localhost` over
	// IPv6, and QUIC does not fall back to IPv4 as TCP does.
	let builder = ServerConfig::builder();
	let builder = if address.ip().is_unspecified() {
		builder.with_bind_default(address.port())
	} else {
		builder.with_bind_address(address)
	};
	builder.with_custom_transport(identity, transport).build()
}

async fn accept<B: Backend>(
	endpoint: Arc<Endpoint<wtransport::endpoint::endpoint_side::Server>>,
	backend: Arc<B>,
	webtransport: Arc<WebTransport>,
	access: Arc<Access>,
) {
	loop {
		let incoming = endpoint.accept().await;
		let peer = incoming.remote_address();
		if !access
			.allowed_networks
			.iter()
			.any(|network| network.contains(peer.ip()))
		{
			tracing::debug!("Refused a browser stream from {peer}: not in [ui] allowed_networks.");
			incoming.refuse();
			continue;
		}
		tokio::spawn(session(incoming, backend.clone(), webtransport.clone(), access.clone()));
	}
}

async fn session<B: Backend>(
	incoming: IncomingSession,
	backend: Arc<B>,
	webtransport: Arc<WebTransport>,
	access: Arc<Access>,
) {
	let request = match incoming.await {
		Ok(request) => request,
		Err(e) => {
			tracing::debug!("A browser stream failed to connect: {e}");
			return;
		},
	};
	let peer = request.remote_address();

	let authority = request.authority().to_string();
	let origin_ok = request
		.origin()
		.is_some_and(|origin| access::origin_allowed(origin, &authority, &access.host_names));
	if !access::host_allowed(&authority, &access.host_names) || !origin_ok {
		tracing::debug!("Refused a browser stream from {peer}: it does not come from the control panel.");
		request.forbidden().await;
		return;
	}

	let path = request.path().to_string();
	let token = path
		.strip_prefix("/stream?")
		.and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("token=")))
		.unwrap_or_default();
	if !webtransport.redeem(token, peer.ip()) {
		tracing::debug!("Refused a browser stream from {peer}: no valid token.");
		request.forbidden().await;
		return;
	}

	let connection = match request.accept().await {
		Ok(connection) => connection,
		Err(e) => {
			tracing::debug!("A browser stream from {peer} failed to open: {e}");
			return;
		},
	};

	let (mut send, mut recv) = match tokio::time::timeout(Duration::from_secs(10), connection.accept_bi()).await {
		Ok(Ok(streams)) => streams,
		_ => {
			tracing::debug!("The browser at {peer} opened no control stream.");
			return;
		},
	};

	// The first message says what to stream.
	let request: StreamRequest = match read_message(&mut recv).await {
		Some((CLIENT_START, payload)) => match serde_json::from_slice(&payload) {
			Ok(request) => request,
			Err(e) => {
				let _ = write_message(
					&mut send,
					SERVER_ERROR,
					format!("The start message is not valid: {e}").as_bytes(),
				)
				.await;
				return;
			},
		},
		_ => return,
	};

	tracing::info!("A browser at {peer} starts a stream: {request:?}");
	let link = match backend.start_stream(request).await {
		Ok(link) => link,
		Err(e) => {
			tracing::warn!("The browser stream from {peer} failed to start: {e}");
			let _ = write_message(&mut send, SERVER_ERROR, e.as_bytes()).await;
			let _ = send.finish().await;
			// Give the message time to arrive before the connection closes.
			tokio::time::sleep(Duration::from_millis(500)).await;
			return;
		},
	};

	let params = serde_json::to_vec(&link.params).unwrap_or_default();
	if write_message(&mut send, SERVER_READY, &params).await.is_err() {
		return;
	}

	run(connection, send, recv, link, webtransport).await;
	tracing::info!("The browser stream from {peer} ended.");
}

async fn run(
	connection: Connection,
	mut send: SendStream,
	recv: RecvStream,
	mut link: StreamLink,
	webtransport: Arc<WebTransport>,
) {
	// The control stream from the browser: commands go to the backend, pings come back here.
	let (pong_tx, mut pong_rx) = mpsc::channel::<f64>(16);
	let mut reader = tokio::spawn(read_commands(recv, link.commands.clone(), pong_tx));

	let average_frame = (link.params.bitrate_kbps as usize * 1000 / 8) / link.params.fps.max(1) as usize
		* (100 + usize::from(link.params.fec_percent))
		/ 100;
	let mut audio_sequence = 0u32;
	let mut audio_too_large = false;
	let mut skip_until_key_frame = false;
	let mut fec = Fec::new(link.params.fec_percent);

	loop {
		tokio::select! {
			frame = link.video.next() => {
				let Some(frame) = frame else { break };
				tracing::trace!(
					number = frame.number,
					bytes = frame.data.len(),
					key = frame.key_frame,
					max_datagram = ?connection.max_datagram_size(),
					"Video frame for the browser."
				);
				if skip_until_key_frame && !frame.key_frame {
					continue;
				}
				match send_frame(&connection, &frame, average_frame, &webtransport, &mut fec).await {
					Ok(true) => skip_until_key_frame = false,
					Ok(false) => {
						// The frame did not fit: everything after it is useless until a key frame.
						skip_until_key_frame = true;
						let _ = link.commands.try_send(ClientEvent::RequestKeyFrame);
					},
					Err(()) => break,
				}
			},
			packet = link.audio.next() => {
				let Some(packet) = packet else { break };
				let mut datagram = Vec::with_capacity(13 + packet.len());
				datagram.push(DATAGRAM_AUDIO);
				datagram.extend_from_slice(&audio_sequence.to_le_bytes());
				datagram.extend_from_slice(&webtransport.micros(Instant::now()).to_le_bytes());
				datagram.extend_from_slice(&packet);
				audio_sequence = audio_sequence.wrapping_add(1);
				match connection.send_datagram(datagram) {
					Ok(()) => {},
					// A packet larger than the path allows: the browser hears a short gap.
					Err(SendDatagramError::TooLarge) => {
						if !audio_too_large {
							audio_too_large = true;
							tracing::warn!(
								"An audio packet of {} bytes is too large for the network path to the browser ({:?} bytes); such packets are dropped.",
								packet.len(),
								connection.max_datagram_size()
							);
						}
					},
					Err(_) => break,
				}
			},
			event = link.events.next() => match event {
				Some(ServerEvent::Feedback(packet)) => {
					if write_message(&mut send, SERVER_FEEDBACK, &packet).await.is_err() {
						break;
					}
				},
				Some(ServerEvent::Ended(reason)) => {
					let _ = write_message(&mut send, SERVER_ENDED, reason.as_bytes()).await;
					let _ = send.finish().await;
					tokio::time::sleep(Duration::from_millis(200)).await;
					break;
				},
				None => break,
			},
			Some(sent_at) = pong_rx.recv() => {
				let mut payload = Vec::with_capacity(16);
				payload.extend_from_slice(&sent_at.to_le_bytes());
				payload.extend_from_slice(&webtransport.micros(Instant::now()).to_le_bytes());
				if write_message(&mut send, SERVER_PONG, &payload).await.is_err() {
					break;
				}
			},
			_ = &mut reader => break,
			_ = connection.closed() => break,
		}
	}

	reader.abort();
	connection.close(0u32.into(), b"");
}

/// Send one frame as datagrams. Waits while the send queue is long, so that the backend drops
/// captures instead. Returns false when the frame was dropped, and an error when the connection is
/// gone.
async fn send_frame(
	connection: &Connection,
	frame: &VideoFrame,
	average_frame: usize,
	webtransport: &WebTransport,
	fec: &mut Fec,
) -> Result<bool, ()> {
	let max_datagram = connection.max_datagram_size().ok_or(())?;
	if max_datagram <= VIDEO_HEADER {
		return Err(());
	}
	let flags = u8::from(frame.key_frame) | (u8::from(frame.recovery) << 1);
	let timestamp = webtransport.micros(frame.captured_at);
	let Some(datagrams) = fec.datagrams(frame, flags, timestamp, max_datagram - VIDEO_HEADER) else {
		tracing::warn!("A frame of {} bytes is too large to send.", frame.data.len());
		return Ok(false);
	};
	let bytes: usize = datagrams.iter().map(Vec::len).sum();

	// Wait while the queue holds more than a few frames, or the frame does not fit.
	let quic = connection.quic_connection();
	let limit = average_frame * MAX_QUEUED_FRAMES;
	let started = Instant::now();
	loop {
		let space = quic.datagram_send_buffer_space();
		let queued = DATAGRAM_SEND_BUFFER.saturating_sub(space);
		if space >= bytes && queued <= limit {
			break;
		}
		if started.elapsed() > MAX_FRAME_WAIT {
			tracing::debug!(
				"Dropped a frame of {} bytes: the network is too slow.",
				frame.data.len()
			);
			return Ok(false);
		}
		tokio::time::sleep(Duration::from_millis(1)).await;
	}

	for datagram in datagrams {
		match connection.send_datagram(datagram) {
			Ok(()) => {},
			// The path's datagram size shrank since the shards were cut: the next frame fits.
			Err(SendDatagramError::TooLarge) => return Ok(false),
			Err(_) => return Err(()),
		}
	}
	Ok(true)
}

/// Cuts frames into shards and adds Reed-Solomon parity shards.
struct Fec {
	percent: usize,
	/// Codecs by (data shards, parity shards).
	codecs: HashMap<(usize, usize), fec_rs::ReedSolomon>,
}

impl Fec {
	fn new(percent: u8) -> Self {
		Self {
			percent: usize::from(percent),
			codecs: HashMap::new(),
		}
	}

	/// The parity shards for a block of `data` data shards.
	fn parity(&self, data: usize) -> usize {
		if self.percent == 0 {
			return 0;
		}
		(data * self.percent)
			.div_ceil(100)
			.max(MIN_PARITY_SHARDS)
			.min(MAX_BLOCK_SHARDS - data)
	}

	/// The most data shards in one block.
	fn max_block_data(&self) -> usize {
		let mut data = MAX_BLOCK_SHARDS * 100 / (100 + self.percent);
		while data > 1 && data + self.parity(data) > MAX_BLOCK_SHARDS {
			data -= 1;
		}
		data
	}

	/// The datagrams of `frame`, with at most `max_shard` bytes of shard each. `None` when the
	/// frame is too large.
	fn datagrams(&mut self, frame: &VideoFrame, flags: u8, timestamp: u64, max_shard: usize) -> Option<Vec<Vec<u8>>> {
		let length = frame.data.len();
		let shards = length.div_ceil(max_shard).max(1);
		// Even shards: the padding is less than one byte per shard.
		let shard_size = length.div_ceil(shards).max(1);
		let blocks = shards.div_ceil(self.max_block_data());
		if blocks > usize::from(u8::MAX) || shards > usize::from(u16::MAX) || u32::try_from(length).is_err() {
			return None;
		}

		let mut datagrams = Vec::new();
		let mut first = 0;
		for block in 0..blocks {
			// Spread the shards evenly over the blocks.
			let data = shards / blocks + usize::from(block < shards % blocks);
			let parity = self.parity(data);
			let mut block_shards: Vec<Vec<u8>> = (0..data + parity)
				.map(|index| {
					let mut shard = vec![0u8; shard_size];
					if index < data {
						let start = ((first + index) * shard_size).min(length);
						let end = (start + shard_size).min(length);
						shard[..end - start].copy_from_slice(&frame.data[start..end]);
					}
					shard
				})
				.collect();
			if parity > 0 {
				let codec = match self.codecs.entry((data, parity)) {
					std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
					std::collections::hash_map::Entry::Vacant(entry) => {
						entry.insert(fec_rs::ReedSolomon::new(data, parity).ok()?)
					},
				};
				codec.encode(&mut block_shards).ok()?;
			}

			for (index, shard) in block_shards.into_iter().enumerate() {
				let mut datagram = Vec::with_capacity(VIDEO_HEADER + shard.len());
				datagram.push(DATAGRAM_VIDEO);
				datagram.push(flags);
				datagram.extend_from_slice(&frame.number.to_le_bytes());
				datagram.push(block as u8);
				datagram.push(blocks as u8);
				datagram.push(index as u8);
				datagram.push(data as u8);
				datagram.push(parity as u8);
				datagram.push(0);
				datagram.extend_from_slice(&(first as u16).to_le_bytes());
				datagram.extend_from_slice(&(length as u32).to_le_bytes());
				datagram.extend_from_slice(&timestamp.to_le_bytes());
				datagram.extend_from_slice(&shard);
				datagrams.push(datagram);
			}
			first += data;
		}
		Some(datagrams)
	}
}

/// Read the browser's control messages until the stream closes.
async fn read_commands(mut recv: RecvStream, commands: mpsc::Sender<ClientEvent>, pongs: mpsc::Sender<f64>) {
	while let Some((kind, payload)) = read_message(&mut recv).await {
		let event = match kind {
			CLIENT_START => ClientEvent::Start,
			CLIENT_INPUT => ClientEvent::Input(payload),
			CLIENT_KEY_FRAME => ClientEvent::RequestKeyFrame,
			CLIENT_STOP => ClientEvent::Stop,
			CLIENT_PING => {
				if let Ok(bytes) = <[u8; 8]>::try_from(payload.as_slice()) {
					let _ = pongs.try_send(f64::from_le_bytes(bytes));
				}
				continue;
			},
			kind => {
				tracing::debug!("Ignored an unknown control message ({kind}) from the browser.");
				continue;
			},
		};
		if commands.send(event).await.is_err() {
			break;
		}
	}
}

async fn read_message(recv: &mut RecvStream) -> Option<(u8, Vec<u8>)> {
	let length = recv.read_u32_le().await.ok()? as usize;
	if length == 0 || length > MAX_CONTROL_MESSAGE {
		return None;
	}
	let mut message = vec![0u8; length];
	recv.read_exact(&mut message).await.ok()?;
	let kind = message.remove(0);
	Some((kind, message))
}

async fn write_message(send: &mut SendStream, kind: u8, payload: &[u8]) -> Result<(), ()> {
	let mut message = Vec::with_capacity(5 + payload.len());
	message.extend_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
	message.push(kind);
	message.extend_from_slice(payload);
	send.write_all(&message).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tokens_are_single_use_and_bound_to_the_peer() {
		let webtransport = WebTransport {
			port: 1,
			certificate_hash: Mutex::new(None),
			tokens: Mutex::new(HashMap::new()),
			epoch: Instant::now(),
		};
		let peer: IpAddr = "192.168.1.2".parse().unwrap();
		let token = webtransport.issue_token(Some(peer), None).unwrap().token;
		assert!(!webtransport.redeem(&token, "192.168.1.3".parse().unwrap()));

		let token = webtransport.issue_token(Some(peer), None).unwrap().token;
		assert!(webtransport.redeem(&token, "::ffff:192.168.1.2".parse().unwrap()));
		assert!(!webtransport.redeem(&token, peer));
		assert!(!webtransport.redeem("", peer));

		let loopback = webtransport
			.issue_token(Some("127.0.0.1".parse().unwrap()), None)
			.unwrap()
			.token;
		assert!(webtransport.redeem(&loopback, "::1".parse().unwrap()));
		let remote = webtransport.issue_token(Some(peer), None).unwrap().token;
		assert!(!webtransport.redeem(&remote, "127.0.0.1".parse().unwrap()));

		// Behind a reverse proxy that does not say who the browser is, any address can redeem.
		let proxied = webtransport.issue_token(None, None).unwrap().token;
		assert!(webtransport.redeem(&proxied, "100.64.0.9".parse().unwrap()));
		assert!(!webtransport.redeem(&proxied, "100.64.0.9".parse().unwrap()));
	}

	/// Writes test vectors for `ui/tests/fec.mjs`, which checks the browser's decoder against
	/// `fec-rs`: `FEC_VECTORS=<file> cargo test -p moonshine-ui fec_vectors`, then
	/// `node ui/tests/fec.mjs <file>`.
	#[test]
	fn fec_vectors() {
		let Ok(path) = std::env::var("FEC_VECTORS") else {
			return;
		};
		let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
		let mut seed = 0x2545_f491_4f6c_dd1du64;
		let mut random = move || {
			seed ^= seed << 13;
			seed ^= seed >> 7;
			seed ^= seed << 17;
			seed
		};
		let mut frames = Vec::new();
		for (percent, length) in [
			(20, 1),
			(20, 900),
			(20, 1000),
			(20, 12_345),
			(20, 300_000),
			(50, 54_321),
			(0, 5000),
		] {
			let mut fec = Fec::new(percent);
			let frame = VideoFrame {
				data: (0..length).map(|_| random() as u8).collect(),
				key_frame: true,
				recovery: false,
				number: 7,
				captured_at: Instant::now(),
			};
			let datagrams = fec.datagrams(&frame, 1, 42, 1000).unwrap();
			frames.push(serde_json::json!({
				"percent": percent,
				"data": hex(&frame.data),
				"datagrams": datagrams.iter().map(|d| hex(d)).collect::<Vec<_>>(),
			}));
		}
		std::fs::write(path, serde_json::to_vec(&frames).unwrap()).unwrap();
	}

	#[test]
	fn blocks_hold_at_most_255_shards() {
		for percent in [0u8, 1, 20, 50, 100, 200] {
			let fec = Fec::new(percent);
			let data = fec.max_block_data();
			assert!(
				data >= 1 && data + fec.parity(data) <= MAX_BLOCK_SHARDS,
				"{percent}%: {data}"
			);
			if percent > 0 {
				assert!(fec.parity(1) >= MIN_PARITY_SHARDS);
			}
		}
	}
}
