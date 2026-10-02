use std::net::SocketAddr;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_enet::{Event, Host, HostConfig, Packet, PacketMode, PeerState};

use self::input::gamepad::GamepadConfig;
use self::{feedback::FeedbackCommand, input::InputHandler};
use crate::crypto::{decrypt, encrypt};
use crate::session::SessionContext;
use crate::session::compositor::{
	frame::{HdrMetadata, HdrModeState},
	input::CompositorInputEvent,
};
use crate::session::keys::ActiveKeys;
use crate::session::lifecycle::WorkerGuard;
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::audio::AudioStartHandle;
use crate::session::stream::video::VideoStreamHandle;
use crate::session::stream::video::fec::FrameFecStatus;
use crate::session::{AuthorizationReceiver, SessionKeysReceiver};

mod feedback;
pub(crate) mod input;
mod peers;

use self::peers::ControlPeers;

/// Configuration for the control stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlStreamConfig {
	/// Port to use for streaming control data.
	pub port: u16,

	/// Configuration for gamepad input remapping (e.g. hold-to-Home).
	pub gamepad: GamepadConfig,
}

impl Default for ControlStreamConfig {
	fn default() -> Self {
		Self {
			port: 47999,
			gamepad: GamepadConfig::default(),
		}
	}
}

const ENCRYPTION_TAG_LENGTH: usize = 16;
/// Control message header: little-endian type and body length.
const CONTROL_HEADER_LENGTH: usize = 4;
/// Encrypted envelope body: sequence number, tag and an encrypted message header.
const MINIMUM_ENCRYPTED_BODY_LENGTH: usize = 4 + ENCRYPTION_TAG_LENGTH + CONTROL_HEADER_LENGTH;

#[repr(u16)]
enum ControlMessageType {
	Encrypted = 0x0001,
	TerminationExtended = 0x0109,
	RumbleData = 0x010b,
	HdrMode = 0x010e,
	Ping = 0x0200,
	LossStats = 0x0201,
	FrameStats = 0x0204,
	InputData = 0x0206,
	RequestIdrFrame = 0x0302,
	InvalidateReferenceFrames = 0x0301,
	StartB = 0x0307,
	RumbleTriggers = 0x5500,
	SetMotionEvent = 0x5501,
	FrameFecStatus = 0x5502,
	SetTriggerEffect = 0x5503,
}

impl TryFrom<u16> for ControlMessageType {
	type Error = ();

	fn try_from(v: u16) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::Encrypted as u16 => Ok(Self::Encrypted),
			x if x == Self::TerminationExtended as u16 => Ok(Self::TerminationExtended),
			x if x == Self::RumbleData as u16 => Ok(Self::RumbleData),
			x if x == Self::HdrMode as u16 => Ok(Self::HdrMode),
			x if x == Self::Ping as u16 => Ok(Self::Ping),
			x if x == Self::LossStats as u16 => Ok(Self::LossStats),
			x if x == Self::FrameStats as u16 => Ok(Self::FrameStats),
			x if x == Self::InputData as u16 => Ok(Self::InputData),
			x if x == Self::RequestIdrFrame as u16 => Ok(Self::RequestIdrFrame),
			x if x == Self::InvalidateReferenceFrames as u16 => Ok(Self::InvalidateReferenceFrames),
			x if x == Self::StartB as u16 => Ok(Self::StartB),
			x if x == Self::RumbleTriggers as u16 => Ok(Self::RumbleTriggers),
			x if x == Self::SetMotionEvent as u16 => Ok(Self::SetMotionEvent),
			x if x == Self::FrameFecStatus as u16 => Ok(Self::FrameFecStatus),
			x if x == Self::SetTriggerEffect as u16 => Ok(Self::SetTriggerEffect),
			_ => Err(()),
		}
	}
}

/// Why bytes are not a well-formed control message. Parsing is total: every
/// byte sequence yields a message or one of these errors, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlParseError {
	/// Fewer bytes than the message type's fixed layout requires.
	Truncated { needed: usize, actual: usize },
	/// A declared length disagrees with the bytes present.
	LengthMismatch { declared: usize, actual: usize },
	/// An encrypted envelope inside a decrypted envelope. Envelopes are client
	/// data, not an internal invariant, so this is a protocol error.
	NestedEncryption,
}

#[derive(Debug)]
enum ControlMessage<'a> {
	Encrypted {
		sequence_number: u32,
		tag: [u8; ENCRYPTION_TAG_LENGTH],
		ciphertext: &'a [u8],
	},
	TerminationExtended,
	RumbleData,
	HdrMode,
	Ping,
	LossStats,
	FrameStats,
	InputData(&'a [u8]),
	RequestIdrFrame,
	InvalidateReferenceFrames {
		first: u32,
		last: u32,
	},
	StartB,
	RumbleTriggers,
	SetMotionEvent,
	FrameFecStatus(FrameFecStatus),
	SetTriggerEffect,
	/// A type this server does not implement. Newer clients may send extension
	/// messages; they are ignored rather than treated as errors.
	Unknown(u16),
}

/// Read `N` bytes at `offset`, or report the length the layout needs.
fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], ControlParseError> {
	bytes
		.get(offset..offset + N)
		.and_then(|field| field.try_into().ok())
		.ok_or(ControlParseError::Truncated {
			needed: offset + N,
			actual: bytes.len(),
		})
}

fn require_body(body: &[u8], needed: usize) -> Result<(), ControlParseError> {
	if body.len() < needed {
		return Err(ControlParseError::Truncated {
			needed: CONTROL_HEADER_LENGTH + needed,
			actual: CONTROL_HEADER_LENGTH + body.len(),
		});
	}
	Ok(())
}

impl<'a> ControlMessage<'a> {
	/// Parse one control message (V2 framing: type, length, body).
	fn from_bytes(buffer: &'a [u8]) -> Result<Self, ControlParseError> {
		let message_type = u16::from_le_bytes(field(buffer, 0)?);
		let length = usize::from(u16::from_le_bytes(field(buffer, 2)?));
		let body = &buffer[CONTROL_HEADER_LENGTH..];
		if length != body.len() {
			return Err(ControlParseError::LengthMismatch {
				declared: length,
				actual: body.len(),
			});
		}

		let Ok(message_type) = ControlMessageType::try_from(message_type) else {
			return Ok(Self::Unknown(message_type));
		};
		match message_type {
			ControlMessageType::Encrypted => {
				require_body(body, MINIMUM_ENCRYPTED_BODY_LENGTH)?;
				Ok(Self::Encrypted {
					sequence_number: u32::from_le_bytes(field(buffer, CONTROL_HEADER_LENGTH)?),
					tag: field(buffer, CONTROL_HEADER_LENGTH + 4)?,
					ciphertext: &body[4 + ENCRYPTION_TAG_LENGTH..],
				})
			},
			ControlMessageType::Ping => Ok(Self::Ping),
			ControlMessageType::TerminationExtended => Ok(Self::TerminationExtended),
			ControlMessageType::RumbleData => Ok(Self::RumbleData),
			ControlMessageType::LossStats => Ok(Self::LossStats),
			ControlMessageType::FrameStats => Ok(Self::FrameStats),
			ControlMessageType::InputData => {
				// Big-endian length of the input event, excluding the length itself.
				let length = u32::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH)?) as usize;
				let event = &body[4..];
				if length != event.len() {
					return Err(ControlParseError::LengthMismatch {
						declared: length,
						actual: event.len(),
					});
				}
				Ok(Self::InputData(event))
			},
			ControlMessageType::InvalidateReferenceFrames => {
				// Body, little-endian: firstFrameIndex(4), reserved1(4),
				// lastFrameIndex(4), reserved2[3](12). The client asks the host
				// to stop referencing frames in [first, last]. If the body is
				// malformed, invalidate from frame 0 so the encoder safely falls
				// back to a full IDR.
				let (first, last) = match (
					field::<4>(buffer, CONTROL_HEADER_LENGTH),
					field::<4>(buffer, CONTROL_HEADER_LENGTH + 8),
				) {
					(Ok(first), Ok(last)) => (u32::from_le_bytes(first), u32::from_le_bytes(last)),
					_ => (0, u32::MAX),
				};
				Ok(Self::InvalidateReferenceFrames { first, last })
			},
			ControlMessageType::RequestIdrFrame => Ok(Self::RequestIdrFrame),
			ControlMessageType::StartB => Ok(Self::StartB),
			ControlMessageType::HdrMode => Ok(Self::HdrMode),
			ControlMessageType::RumbleTriggers => Ok(Self::RumbleTriggers),
			ControlMessageType::SetMotionEvent => Ok(Self::SetMotionEvent),
			ControlMessageType::FrameFecStatus => {
				// Moonlight sends the packed structure in network byte order. The
				// C structure may include trailing padding, which is intentionally
				// ignored here.
				require_body(body, 21)?;
				Ok(Self::FrameFecStatus(FrameFecStatus {
					frame_index: u32::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH)?),
					highest_received_sequence_number: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 4)?),
					next_contiguous_sequence_number: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 6)?),
					missing_packets_before_highest: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 8)?),
					total_data_packets: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 10)?),
					total_parity_packets: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 12)?),
					received_data_packets: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 14)?),
					received_parity_packets: u16::from_be_bytes(field(buffer, CONTROL_HEADER_LENGTH + 16)?),
					fec_percentage: body[18],
					block_index: body[19],
					block_count: body[20],
					..Default::default()
				}))
			},
			ControlMessageType::SetTriggerEffect => Ok(Self::SetTriggerEffect),
		}
	}

	/// Parse the plaintext of an authenticated envelope.
	fn from_decrypted(buffer: &'a [u8]) -> Result<Self, ControlParseError> {
		match Self::from_bytes(buffer)? {
			Self::Encrypted { .. } => Err(ControlParseError::NestedEncryption),
			message => Ok(message),
		}
	}
}

/// AES-GCM nonce of a control message: little-endian sequence number, then a
/// direction marker (`'C'` client-originated, `'H'` host-originated) and `'C'`
/// for the control stream, so neither direction can reuse the other's nonce.
fn control_nonce(sequence_number: u32, origin: u8) -> [u8; 12] {
	let mut nonce = [0u8; 12];
	nonce[0..4].copy_from_slice(&sequence_number.to_le_bytes());
	nonce[10] = origin;
	nonce[11] = b'C';
	nonce
}

/// Authenticate and decrypt a client-originated control envelope.
fn decrypt_control(key: &[u8], sequence_number: u32, tag: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, ()> {
	// Check before the AES-GCM key conversion, which panics on other lengths.
	if key.len() != 16 {
		return Err(());
	}
	decrypt(ciphertext, key, &control_nonce(sequence_number, b'C'), tag).map_err(|_| ())
}

fn encode_control_envelope(key: &[u8], sequence_number: u32, origin: u8, payload: &[u8]) -> Result<Vec<u8>, ()> {
	if key.len() != 16 {
		tracing::warn!("Key length has {} bytes, but expected {} bytes.", key.len(), 16);
		return Err(());
	}

	let mut tag = [0u8; ENCRYPTION_TAG_LENGTH];
	let ciphertext = encrypt(payload, key, &control_nonce(sequence_number, origin), &mut tag)
		.map_err(|e| tracing::warn!("Failed to encrypt control data: {e}"))?;
	if ciphertext.is_empty() {
		tracing::warn!("Failed to encrypt control data.");
		return Err(());
	}
	let length = u16::try_from(4 + ENCRYPTION_TAG_LENGTH + ciphertext.len())
		.map_err(|_| tracing::warn!("Control message is too large to encrypt"))?;

	let mut buffer = Vec::with_capacity(CONTROL_HEADER_LENGTH + usize::from(length));
	buffer.extend((ControlMessageType::Encrypted as u16).to_le_bytes());
	buffer.extend(length.to_le_bytes());
	buffer.extend(sequence_number.to_le_bytes());
	buffer.extend(tag);
	buffer.extend(ciphertext);
	Ok(buffer)
}

/// Encrypt a host-originated control message.
fn encode_control(key: &[u8], sequence_number: u32, payload: &[u8]) -> Result<Vec<u8>, ()> {
	encode_control_envelope(key, sequence_number, b'H', payload)
}

/// Build a plaintext V2 control message (test fixtures).
#[cfg(test)]
fn plaintext_control(message_type: u16, body: &[u8]) -> Vec<u8> {
	let mut message = message_type.to_le_bytes().to_vec();
	message.extend((body.len() as u16).to_le_bytes());
	message.extend(body);
	message
}

/// Encrypt a control message as a Moonlight client does (test fixtures).
#[cfg(test)]
fn encode_client_control(key: &[u8], sequence_number: u32, payload: &[u8]) -> Vec<u8> {
	encode_control_envelope(key, sequence_number, b'C', payload).unwrap()
}

/// Context passed to the control stream from Session.
#[derive(Clone)]
pub(crate) struct ControlStreamContext {
	pub keys_rx: SessionKeysReceiver,
	/// Authorized client and generation; peers are bound to it (see `peers`).
	pub authorization_rx: AuthorizationReceiver,
}

impl ControlStreamContext {
	/// Create the control stream's key context. HDR state is delivered through
	/// the live metadata watch so it can change between reconnect epochs.
	pub fn new(ctx: &SessionContext, authorization_rx: AuthorizationReceiver) -> Self {
		Self {
			keys_rx: ctx.keys.clone_rx().expect("session keys not initialized"),
			authorization_rx,
		}
	}
}

pub(crate) struct ControlStream {
	stop: ShutdownManager<SessionShutdownReason>,
	input_handler: InputHandler,
	host: Host,
}

impl ControlStream {
	pub fn new(
		config: ControlStreamConfig,
		address: String,
		input_tx: calloop::channel::Sender<CompositorInputEvent>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		let input_handler = InputHandler::new(input_tx, stop_session_manager.clone(), config.gamepad.clone())?;

		let socket_address = SocketAddr::new(
			address
				.parse()
				.map_err(|e| tracing::warn!("Failed to parse address ({}): {e}", address))?,
			config.port,
		);

		let host_config = HostConfig {
			address: Some(socket_address),
			peer_count: 128,
			channel_limit: 0x30,
			..Default::default()
		};

		let host = Host::new(host_config).map_err(|e| tracing::error!("Failed to create enet host: {e}"))?;

		tracing::debug!("Listening for control messages on {:?}", socket_address);

		Ok(Self {
			stop: stop_session_manager,
			input_handler,
			host,
		})
	}

	pub fn start(
		self,
		stream_timeout: u64,
		context: ControlStreamContext,
		video_handle: VideoStreamHandle,
		audio_trigger: AudioStartHandle,
		hdr_metadata_rx: watch::Receiver<HdrModeState>,
	) {
		let Self {
			stop: stop_session_manager,
			input_handler,
			host,
		} = self;

		// Registered before spawning so a stop that races the first poll still
		// waits for the ENet host to be released.
		let Ok(worker) = WorkerGuard::register(&stop_session_manager, SessionShutdownReason::ControlStreamStopped)
		else {
			return;
		};
		tokio::spawn(async move {
			let _worker = worker;
			run_control_loop(
				stream_timeout,
				host,
				video_handle,
				audio_trigger,
				context,
				input_handler,
				stop_session_manager,
				hdr_metadata_rx,
			)
			.await;
		});
	}
}

/// Build the payload for an HDR mode control message (type 0x010e).
///
/// Format matches Sunshine's `control_hdr_mode_t`:
/// - u16 LE: message type (0x010e)
/// - u16 LE: payload length (1 + 30 = 31 bytes for metadata)
/// - u8: enabled (1 = HDR on, 0 = HDR off)
/// - SS_HDR_METADATA: 30 bytes (15 x u16 LE fields: display primaries,
///   white point, luminance, content light levels, padding). Populated
///   from the provided metadata, or zeroed when `metadata` is `None`.
fn build_hdr_mode_payload(enabled: bool, metadata: Option<&HdrMetadata>) -> Vec<u8> {
	let metadata_size = 30u16; // SS_HDR_METADATA: 15 x u16 fields
	let payload_len = 1 + metadata_size; // enabled byte + metadata
	let mut buf = Vec::with_capacity(4 + payload_len as usize);
	buf.extend((ControlMessageType::HdrMode as u16).to_le_bytes());
	buf.extend(payload_len.to_le_bytes());
	buf.push(if enabled { 1 } else { 0 });

	if let Some(m) = metadata {
		// Display primaries (RGB order).
		for &(x, y) in &m.display_primaries {
			buf.extend(x.to_le_bytes());
			buf.extend(y.to_le_bytes());
		}
		// White point.
		buf.extend(m.white_point.0.to_le_bytes());
		buf.extend(m.white_point.1.to_le_bytes());
		// Max display luminance (convert from 0.0001 cd/m² to nits).
		buf.extend(((m.max_luminance / 10000).min(u16::MAX as u32) as u16).to_le_bytes());
		// Min display luminance (0.0001 cd/m² maps to 1/10000th nit).
		buf.extend((m.min_luminance.min(u16::MAX as u32) as u16).to_le_bytes());
		// Content light levels (direct copy, already in nits).
		buf.extend(m.max_cll.to_le_bytes());
		buf.extend(m.max_fall.to_le_bytes());
		// maxFullFrameLuminance (not available from Wayland protocol).
		buf.extend(0u16.to_le_bytes());
		// Padding.
		buf.extend([0u8; 4]);
	} else {
		buf.extend(std::iter::repeat_n(0u8, metadata_size as usize));
	}

	buf
}

/// Build the payload for a termination extended control message.
///
/// The encrypted protocol uses V2 framing: `type(u16 LE) + length(u16 LE) + data`.
/// The client strips the length field after decryption (V2 → V1 conversion) and
/// then reads a 4-byte big-endian error code. Known NVST_DISCONN values are mapped
/// to client-side error constants (e.g. `0x80030023` → graceful termination).
fn build_termination_payload(error_code: u32) -> Vec<u8> {
	let payload_len = 4u16; // 4 bytes for error code.
	let mut buf = Vec::with_capacity(4 + payload_len as usize);
	buf.extend((ControlMessageType::TerminationExtended as u16).to_le_bytes());
	buf.extend(payload_len.to_le_bytes());
	buf.extend(error_code.to_be_bytes());
	buf
}

/// Send an encrypted control packet to the connected peer if it exists and is connected.
/// Encrypt and send a host-originated message. Its envelope sequence number
/// is the GCM nonce counter, so it is allocated from the key's control nonce
/// sequence: it never restarts for a key that was used before (resume with the
/// same key, a new control task, or a later session reusing the key).
fn send_to_peer(host: &mut Host, peer_id: tokio_enet::PeerId, keys: &ActiveKeys, payload: &[u8], label: &str) {
	let Ok(sequence_number) = keys.material().nonces().control.reserve(1) else {
		tracing::error!("Control encryption nonce space exhausted; not sending {label}");
		return;
	};
	// `CONTROL_NONCE_END` bounds the sequence to the 32-bit envelope field.
	let sequence_number = sequence_number as u32;
	if let Ok(packet) = encode_control(keys.key().as_bytes(), sequence_number, payload)
		&& let Some(peer) = host.peer_mut(peer_id)
		&& peer.state() == PeerState::Connected
	{
		let _ = peer
			.send(0, Packet::new(packet.as_slice(), PacketMode::ReliableSequenced))
			.map_err(|e| tracing::warn!("Failed to send {label} to peer: {e}"));
	}
}

/// Build and send an HDR mode control message, then advance `sequence_number`.
fn send_hdr_state(host: &mut Host, peer_id: tokio_enet::PeerId, state: &HdrModeState, keys: &ActiveKeys, label: &str) {
	let metadata = if state.enabled {
		Some(state.metadata.unwrap_or_else(HdrMetadata::fallback))
	} else {
		None
	};
	let payload = build_hdr_mode_payload(state.enabled, metadata.as_ref());
	send_to_peer(host, peer_id, keys, &payload, label);
	tracing::debug!("Sent HDR mode ({label}) to client: enabled={}", state.enabled);
}

/// Disconnect peers of a replaced launch/resume generation.
///
/// The previous client is disconnected gracefully so a still-running Moonlight
/// instance learns that another resume took over. Unauthorized peers are
/// instead reset immediately, which frees their slot without waiting for an
/// acknowledgement they control.
fn adopt_current_generation(host: &mut Host, peers: &mut ControlPeers, authorization_rx: &mut AuthorizationReceiver) {
	if !authorization_rx.has_changed().unwrap_or(false) {
		return;
	}
	let generation = authorization_rx.borrow_and_update().generation();
	for peer_id in peers.begin_generation(generation) {
		tracing::info!(%peer_id, "Disconnecting control peer of a replaced launch/resume generation");
		if let Some(peer) = host.peer_mut(peer_id) {
			peer.disconnect(0);
		}
	}
}

#[allow(clippy::too_many_arguments)]
async fn run_control_loop(
	stream_timeout: u64,
	mut host: Host,
	video_handle: VideoStreamHandle,
	audio_trigger: AudioStartHandle,
	context: ControlStreamContext,
	input_handler: InputHandler,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	mut hdr_metadata_rx: watch::Receiver<HdrModeState>,
) {
	// The caller's `WorkerGuard` stops the session when this loop exits and
	// holds completion until the ENet host has been dropped.
	let mut stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(stream_timeout);

	// Create a channel over which we can receive feedback messages to send to the connected client.
	let (feedback_tx, mut feedback_rx) = mpsc::channel::<FeedbackCommand>(10);

	let mut send_hdr_mode = false;
	let mut audio_triggered = false;
	// Only the peer that authenticated the current generation is dispatched
	// to and receives feedback.
	let mut authorization_rx = context.authorization_rx.clone();
	let mut peers = ControlPeers::new(authorization_rx.borrow_and_update().generation());

	while !stop_session_manager.is_shutdown_triggered() {
		// Check if the timeout has passed.
		if std::time::Instant::now() > stop_deadline {
			tracing::info!(
				"Stopping because we haven't received a ping for {} seconds.",
				stream_timeout
			);
			break;
		}

		adopt_current_generation(&mut host, &mut peers, &mut authorization_rx);

		// Check for feedback messages.
		if let Ok(command) = feedback_rx.try_recv()
			&& let Some(peer_id) = peers.active()
		{
			tracing::debug!("Sending control feedback command: {command:?}");
			let payload = command.as_packet();
			let keys = context.keys_rx.borrow().clone();
			send_to_peer(&mut host, peer_id, &keys, &payload, "feedback");
		}

		let event = host
			.service(Duration::from_millis(10))
			.await
			.map_err(|e| tracing::error!("Failure in enet host: {e}"));
		// A launch/resume may have completed while servicing; never attribute an
		// event from a replaced generation's peer to the new one.
		adopt_current_generation(&mut host, &mut peers, &mut authorization_rx);

		match event {
			Ok(Some(Event::Connect { peer_id, data })) => {
				let from = host.peer(peer_id).map(|peer| peer.address());
				if !peers.connect(peer_id, from, data, &authorization_rx.borrow()) {
					tracing::warn!(
						?from,
						"Rejected control connection that is not authorized for the current session"
					);
					host.disconnect_now(peer_id, 0);
				}
			},
			Ok(Some(Event::Disconnect { peer_id, .. })) => {
				// Retain the application, but stop high-bitrate traffic to the old
				// UDP endpoint. ANNOUNCE/PLAY activates the next video epoch.
				// Peers of a replaced generation were already forgotten, so an old
				// client disconnecting after resume cannot pause the new epoch.
				if peers.disconnect(peer_id) {
					if video_handle.pause_for_reconfigure().await.is_err() {
						break;
					}
					tracing::info!("Control peer disconnected; paused video delivery for resume");
				}
			},
			Ok(Some(Event::Receive {
				peer_id, ref packet, ..
			})) => {
				let authenticated = {
					let keys = context.keys_rx.borrow();
					peers.authenticate(peer_id, packet.data(), keys.key().as_bytes())
				};
				let decrypted = match authenticated {
					Ok(decrypted) => decrypted,
					// The authenticated client keeps its session; a bad packet is dropped.
					Err(rejection) if peers.active() == Some(peer_id) => {
						tracing::debug!(?rejection, "Dropping control packet from the active peer");
						continue;
					},
					Err(rejection) => {
						tracing::warn!(%peer_id, ?rejection, "Disconnecting unauthenticated control peer");
						peers.disconnect(peer_id);
						host.disconnect_now(peer_id, 0);
						continue;
					},
				};
				let control_message = match ControlMessage::from_decrypted(&decrypted) {
					Ok(control_message) => control_message,
					Err(error) => {
						tracing::debug!(?error, "Dropping malformed decrypted control message");
						continue;
					},
				};
				tracing::trace!("Decrypted control message: {control_message:?}");

				match control_message {
					ControlMessage::RequestIdrFrame => {
						video_handle.request_idr_frame();
					},
					ControlMessage::InvalidateReferenceFrames { first, last } => {
						video_handle.invalidate_reference_frames(first, last);
					},
					ControlMessage::StartB => {
						video_handle.trigger();
						if !audio_triggered {
							audio_trigger.trigger();
							audio_triggered = true;
						}
						send_hdr_mode = true;
					},
					ControlMessage::Ping => {
						stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(stream_timeout);
					},
					ControlMessage::InputData(event) => {
						let _ = input_handler.handle_raw_input(event, feedback_tx.clone()).await;
					},
					ControlMessage::HdrMode => {
						tracing::info!("Received HdrMode toggle from client");
					},
					ControlMessage::FrameFecStatus(status) => {
						video_handle.report_fec_status(status);
					},
					ControlMessage::Unknown(message_type) => {
						tracing::trace!("Ignoring unsupported control message type {message_type:#06x}");
					},
					skipped_message => {
						tracing::trace!("Skipped control message: {skipped_message:?}");
					},
				};
			},
			Ok(None) => (),
			Err(_) => break,
		}

		// Send HDR mode notification after the host.service() match to avoid double mutable borrow.
		if send_hdr_mode {
			send_hdr_mode = false;
			if let Some(peer_id) = peers.active() {
				let state = hdr_metadata_rx.borrow_and_update().clone();
				let keys = context.keys_rx.borrow().clone();
				tracing::info!("Informing client: HDR session");
				send_hdr_state(&mut host, peer_id, &state, &keys, "initial");
			}
		}

		// Check for HDR metadata updates from the video pipeline.
		if hdr_metadata_rx.has_changed().unwrap_or(false)
			&& let Some(peer_id) = peers.active()
		{
			let state = hdr_metadata_rx.borrow_and_update().clone();
			let keys = context.keys_rx.borrow().clone();
			send_hdr_state(&mut host, peer_id, &state, &keys, "metadata update");
		}
	}

	tracing::debug!("Control stream stopped.");

	// Notify the client of graceful termination before closing the connection.
	// NVST_DISCONN_SERVER_TERMINATED_CLOSED (0x80030023) is recognized by the
	// client as a graceful shutdown so it does not display an error.
	let termination_payload = build_termination_payload(0x80030023);
	if let Some(peer_id) = peers.active() {
		let keys = context.keys_rx.borrow().clone();
		send_to_peer(&mut host, peer_id, &keys, &termination_payload, "termination");
	}
	let _ = host.flush().await;

	// Explicitly drop the ENet host before the delay shutdown token
	// to ensure the socket is released before wait_shutdown_complete
	// returns. Without this, the next session's control stream may
	// fail to bind to the same port.
	drop(host);
}

#[cfg(test)]
mod tests {
	use super::*;

	const KEY: [u8; 16] = [3; 16];

	/// Deterministic xorshift generator for bounded fuzz corpora.
	struct Rng(u64);

	impl Rng {
		fn next(&mut self) -> u64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			self.0
		}

		fn bytes(&mut self, len: usize) -> Vec<u8> {
			(0..len).map(|_| self.next() as u8).collect()
		}
	}

	const KNOWN_TYPES: [u16; 15] = [
		0x0001, 0x0109, 0x010b, 0x010e, 0x0200, 0x0201, 0x0204, 0x0206, 0x0302, 0x0301, 0x0307, 0x5500, 0x5501, 0x5502,
		0x5503,
	];

	fn input_data(event: &[u8]) -> Vec<u8> {
		let mut body = (event.len() as u32).to_be_bytes().to_vec();
		body.extend(event);
		plaintext_control(0x0206, &body)
	}

	/// Valid messages of every type with a fixed body layout.
	fn fixtures() -> Vec<Vec<u8>> {
		let key_down = [0x03, 0, 0, 0, 0, 0x41, 0, 0, 0, 0];
		vec![
			plaintext_control(0x0200, &[]),
			plaintext_control(0x0302, &[]),
			plaintext_control(0x0307, &[]),
			plaintext_control(0x0301, &[0; 24]),
			plaintext_control(0x5502, &[0; 24]),
			input_data(&key_down),
			encode_client_control(&KEY, 9, &input_data(&key_down)),
		]
	}

	/// Astra's reproduction (`06 02 00 00`) previously panicked while slicing.
	#[test]
	fn short_input_data_is_a_structured_error() {
		assert_eq!(
			ControlMessage::from_bytes(&[0x06, 0x02, 0, 0]).unwrap_err(),
			ControlParseError::Truncated { needed: 8, actual: 4 }
		);
	}

	#[test]
	fn every_truncation_is_bounded() {
		for fixture in fixtures() {
			assert!(ControlMessage::from_bytes(&fixture).is_ok(), "fixture {fixture:02x?}");
			for len in 0..fixture.len() {
				// Raw prefix: the declared length no longer matches.
				assert!(ControlMessage::from_bytes(&fixture[..len]).is_err(), "prefix {len}");
				// Consistent header: exercise each type's own minimum layout.
				if len >= CONTROL_HEADER_LENGTH {
					let mut consistent = fixture[..len].to_vec();
					consistent[2..4].copy_from_slice(&((len - CONTROL_HEADER_LENGTH) as u16).to_le_bytes());
					let _ = ControlMessage::from_bytes(&consistent);
				}
			}
		}
		// Types whose layout requires a minimum body report truncation.
		for (message_type, minimum) in [(0x0001, 24), (0x0206, 4), (0x5502, 21)] {
			for len in 0..minimum {
				assert!(matches!(
					ControlMessage::from_bytes(&plaintext_control(message_type, &vec![0; len])),
					Err(ControlParseError::Truncated { .. })
				));
			}
		}
		// Malformed invalidation ranges fall back to a full invalidation.
		assert!(matches!(
			ControlMessage::from_bytes(&plaintext_control(0x0301, &[1; 4])),
			Ok(ControlMessage::InvalidateReferenceFrames {
				first: 0,
				last: u32::MAX
			})
		));
	}

	#[test]
	fn mismatched_lengths_are_rejected() {
		let mut message = plaintext_control(0x0200, &[0; 4]);
		message[2] = 5;
		assert_eq!(
			ControlMessage::from_bytes(&message).unwrap_err(),
			ControlParseError::LengthMismatch { declared: 5, actual: 4 }
		);
		message[2] = 3;
		assert!(ControlMessage::from_bytes(&message).is_err());
		// The inner input length must cover the event exactly.
		let mut input = input_data(&[0x03, 0, 0, 0, 0, 0x41, 0, 0, 0, 0]);
		input[7] += 1;
		assert!(matches!(
			ControlMessage::from_bytes(&input),
			Err(ControlParseError::LengthMismatch { .. })
		));
	}

	#[test]
	fn unknown_types_are_ignorable() {
		assert!(matches!(
			ControlMessage::from_bytes(&plaintext_control(0x7777, &[1, 2, 3])),
			Ok(ControlMessage::Unknown(0x7777))
		));
		assert!(matches!(
			ControlMessage::from_decrypted(&plaintext_control(0x5504, &[])),
			Ok(ControlMessage::Unknown(0x5504))
		));
	}

	#[test]
	fn nested_encrypted_envelope_is_rejected() {
		let inner = encode_client_control(&KEY, 1, &plaintext_control(0x0302, &[]));
		let outer = encode_client_control(&KEY, 2, &inner);
		let Ok(ControlMessage::Encrypted {
			sequence_number,
			tag,
			ciphertext,
		}) = ControlMessage::from_bytes(&outer)
		else {
			panic!("expected an envelope");
		};
		let decrypted = decrypt_control(&KEY, sequence_number, &tag, ciphertext).unwrap();
		assert_eq!(
			ControlMessage::from_decrypted(&decrypted).unwrap_err(),
			ControlParseError::NestedEncryption
		);
		// Host- and client-originated nonces differ, and so do keys.
		assert!(decrypt_control(&[4; 16], sequence_number, &tag, ciphertext).is_err());
		assert!(decrypt_control(&KEY[..15], sequence_number, &tag, ciphertext).is_err());
		let host = encode_control(&KEY, 2, &inner).unwrap();
		let Ok(ControlMessage::Encrypted { tag, ciphertext, .. }) = ControlMessage::from_bytes(&host) else {
			panic!("expected an envelope");
		};
		assert!(decrypt_control(&KEY, 2, &tag, ciphertext).is_err());
	}

	#[test]
	fn arbitrary_bytes_never_panic() {
		let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
		let fixtures = fixtures();
		for _ in 0..50_000 {
			let len = (rng.next() % 64) as usize;
			let mut buffer = rng.bytes(len);
			match rng.next() % 4 {
				// Bias towards known types with a consistent length field.
				0 | 1 if len >= 4 => {
					let message_type = KNOWN_TYPES[(rng.next() as usize) % KNOWN_TYPES.len()];
					buffer[..2].copy_from_slice(&message_type.to_le_bytes());
					buffer[2..4].copy_from_slice(&((len - 4) as u16).to_le_bytes());
				},
				// Mutate a valid fixture.
				2 => {
					buffer = fixtures[(rng.next() as usize) % fixtures.len()].clone();
					let index = (rng.next() as usize) % buffer.len();
					buffer[index] ^= rng.next() as u8 | 1;
				},
				_ => {},
			}
			let _ = ControlMessage::from_bytes(&buffer);
			let _ = ControlMessage::from_decrypted(&buffer);
			if let Ok(ControlMessage::InputData(event)) = ControlMessage::from_bytes(&buffer) {
				let _ = input::parse_input_event(event);
			}
		}
	}

	mod enet {
		use std::net::{IpAddr, Ipv4Addr, SocketAddr};
		use std::sync::Arc;
		use std::sync::atomic::{AtomicUsize, Ordering};
		use std::time::{Duration, Instant};

		use tokio_enet::{Event, Host, HostConfig, Packet, PacketMode, PeerId};

		use super::*;
		use crate::session::authorization::StreamAuthorization;
		use crate::session::stream::video::VideoPacketMessage;
		use crate::session::{RemoteInputKey, RemoteInputKeyId, SessionKeyData};

		const NEW_KEY: [u8; 16] = [5; 16];

		struct Client {
			host: Host,
			peer: PeerId,
			sequence: u32,
			connected: bool,
			disconnected: bool,
			received: Vec<Vec<u8>>,
		}

		impl Client {
			async fn connect(ip: Ipv4Addr, server: SocketAddr, data: u32) -> Self {
				let mut host = Host::new(HostConfig {
					address: Some(SocketAddr::new(IpAddr::V4(ip), 0)),
					peer_count: 1,
					channel_limit: 0x20,
					..Default::default()
				})
				.unwrap();
				let peer = host.connect(server, 0x20, data).unwrap();
				let mut client = Self {
					host,
					peer,
					sequence: 0,
					connected: false,
					disconnected: false,
					received: Vec::new(),
				};
				client.pump(Duration::from_millis(150)).await;
				assert!(client.connected, "ENet transport connects before authorization");
				client
			}

			async fn pump(&mut self, duration: Duration) {
				let deadline = Instant::now() + duration;
				while Instant::now() < deadline {
					match self.host.service(Duration::from_millis(5)).await {
						Ok(Some(Event::Connect { .. })) => self.connected = true,
						Ok(Some(Event::Disconnect { .. })) => self.disconnected = true,
						Ok(Some(Event::Receive { packet, .. })) => self.received.push(packet.data().to_vec()),
						_ => {},
					}
				}
			}

			async fn send_raw(&mut self, bytes: &[u8]) {
				if let Some(peer) = self.host.peer_mut(self.peer) {
					let _ = peer.send(0, Packet::new(bytes, PacketMode::ReliableSequenced));
				}
				self.pump(Duration::from_millis(60)).await;
			}

			/// Send an encrypted message, returning the envelope for replay tests.
			async fn send(&mut self, key: &[u8], message: &[u8]) -> Vec<u8> {
				let envelope = encode_client_control(key, self.sequence, message);
				self.sequence += 1;
				self.send_raw(&envelope).await;
				envelope
			}

			/// Decrypt host-originated control messages received so far.
			fn feedback(&self, key: &[u8]) -> Vec<Vec<u8>> {
				self.received
					.iter()
					.filter_map(|packet| match ControlMessage::from_bytes(packet) {
						Ok(ControlMessage::Encrypted {
							sequence_number,
							tag,
							ciphertext,
						}) => decrypt(ciphertext, key, &control_nonce(sequence_number, b'H'), &tag).ok(),
						_ => None,
					})
					.collect()
			}
		}

		fn key_down_a() -> Vec<u8> {
			input_data(&[0x03, 0, 0, 0, 0, 0x41, 0, 0, 0, 0])
		}

		fn count_key_presses(input: &calloop::channel::Channel<CompositorInputEvent>) -> usize {
			std::iter::from_fn(|| input.try_recv().ok())
				.filter(|event| matches!(event, CompositorInputEvent::KeyDown { keycode: 30 }))
				.count()
		}

		fn drain(idr: &mut tokio::sync::broadcast::Receiver<()>) -> usize {
			std::iter::from_fn(|| idr.try_recv().ok()).count()
		}

		/// Two-peer regression for SEC-002/BUG-001 over real loopback ENet:
		/// unauthorized, plaintext, wrong-key, replayed, malformed and stale peers
		/// cannot inject input, trigger recovery, take over feedback or end the
		/// session; the authenticated client keeps working throughout.
		#[tokio::test]
		async fn control_is_bound_to_the_authenticated_peer_and_generation() {
			let client_ip = Ipv4Addr::LOCALHOST;
			let mut authorization = StreamAuthorization::new(1, client_ip.into()).unwrap();
			authorization.require_session_id();
			let connect_data = authorization.control_connect_data();
			let (authorization_tx, authorization_rx) = watch::channel(authorization);
			let mut ledger = crate::session::keys::KeyLedger::default();
			let (keys_tx, keys_rx) = watch::channel(ledger.publish(SessionKeyData::new(
				RemoteInputKey::from_bytes(KEY),
				RemoteInputKeyId::new(1),
			)));

			let host = Host::new(HostConfig {
				address: Some(SocketAddr::new(client_ip.into(), 0)),
				peer_count: 16,
				channel_limit: 0x30,
				..Default::default()
			})
			.unwrap();
			let server = host.local_addr().unwrap();
			let stop = ShutdownManager::new();
			let (input_tx, input_rx) = calloop::channel::channel();
			let input_handler = InputHandler::new(input_tx, stop.clone(), Default::default()).unwrap();
			let (video_handle, probe) = VideoStreamHandle::for_test();
			let mut idr = probe.idr_rx;
			let pauses = Arc::new(AtomicUsize::new(0));
			tokio::spawn({
				let pauses = pauses.clone();
				let mut packet_rx = probe.packet_rx;
				async move {
					while let Some(message) = packet_rx.recv().await {
						if let VideoPacketMessage::Pause(ready) = message {
							pauses.fetch_add(1, Ordering::SeqCst);
							let _ = ready.send(());
						}
					}
				}
			});
			let (_hdr_tx, hdr_rx) = watch::channel(HdrModeState::new(false));
			let control = tokio::spawn(run_control_loop(
				60,
				host,
				video_handle,
				AudioStartHandle::for_test(),
				ControlStreamContext {
					keys_rx,
					authorization_rx,
				},
				input_handler,
				stop.clone(),
				hdr_rx,
			));
			let request_idr = plaintext_control(0x0302, &[]);

			// Another host, or the client's address with wrong connect data.
			let mut other_host = Client::connect(Ipv4Addr::new(127, 0, 0, 2), server, connect_data).await;
			let mut wrong_data = Client::connect(client_ip, server, connect_data ^ 1).await;
			// Rejected peers are reset server-side: their slot is freed and later
			// packets are ignored (tokio-enet does not notify the remote).
			other_host.send(&KEY, &request_idr).await;
			wrong_data.send(&KEY, &request_idr).await;
			assert_eq!(drain(&mut idr), 0);

			// Same-address peers that know the connect data but not the key.
			let mut legit = Client::connect(client_ip, server, connect_data).await;
			let mut plaintext = Client::connect(client_ip, server, connect_data).await;
			let mut wrong_key = Client::connect(client_ip, server, connect_data).await;
			let mut malformed = Client::connect(client_ip, server, connect_data).await;
			plaintext.send_raw(&key_down_a()).await;
			plaintext.send_raw(&request_idr).await;
			wrong_key.send(&[9; 16], &key_down_a()).await;
			malformed.send_raw(&[0x06, 0x02, 0, 0]).await;
			legit.pump(Duration::from_millis(50)).await;
			// A rejected peer stays rejected even once it sends valid messages.
			plaintext.send(&KEY, &request_idr).await;
			assert_eq!(count_key_presses(&input_rx), 0, "no injected input");
			assert_eq!(drain(&mut idr), 0, "no injected recovery");

			// The authenticated client is dispatched.
			legit.send(&KEY, &key_down_a()).await;
			let captured = legit.send(&KEY, &request_idr).await;
			assert_eq!(count_key_presses(&input_rx), 1);
			assert_eq!(drain(&mut idr), 1);

			// Replays (through the client or another peer) and malformed packets
			// from the active client are dropped without ending the session.
			let mut replayer = Client::connect(client_ip, server, connect_data).await;
			replayer.send_raw(&captured).await;
			legit.send_raw(&captured).await;
			legit.send_raw(&[0x06, 0x02, 0, 0]).await;
			legit.send_raw(&[0xff; 3]).await;
			assert_eq!(drain(&mut idr), 0, "replayed recovery request");
			replayer.send(&KEY, &request_idr).await;
			assert_eq!(drain(&mut idr), 0, "another peer cannot take over the active client");
			legit.send(&KEY, &request_idr).await;
			assert_eq!(drain(&mut idr), 1, "client still served after its malformed packets");
			assert!(!control.is_finished());

			// Only the active client receives feedback.
			legit.send(&KEY, &plaintext_control(0x0307, &[])).await;
			legit.pump(Duration::from_millis(50)).await;
			assert!(
				legit
					.feedback(&KEY)
					.iter()
					.any(|message| message.starts_with(&0x010e_u16.to_le_bytes())),
				"HDR mode feedback after StartB"
			);
			for peer in [&other_host, &wrong_data, &plaintext, &wrong_key, &malformed, &replayer] {
				assert!(peer.received.is_empty());
			}

			// A resume replaces the generation and key: the previous client is
			// disconnected and cannot act, without pausing the new epoch.
			let mut next = StreamAuthorization::new(2, client_ip.into()).unwrap();
			next.require_session_id();
			let next_data = next.control_connect_data();
			authorization_tx.send_replace(next);
			keys_tx.send_replace(ledger.publish(SessionKeyData::new(
				RemoteInputKey::from_bytes(NEW_KEY),
				RemoteInputKeyId::new(2),
			)));
			legit.pump(Duration::from_millis(100)).await;
			assert!(legit.disconnected, "stale generation peer is disconnected");
			legit.send(&KEY, &request_idr).await;
			let mut stale = Client::connect(client_ip, server, connect_data).await;
			stale.send(&KEY, &request_idr).await;
			stale.send(&NEW_KEY, &request_idr).await;
			assert_eq!(drain(&mut idr), 0);
			assert_eq!(pauses.load(Ordering::SeqCst), 0);

			let mut resumed = Client::connect(client_ip, server, next_data).await;
			resumed.send(&NEW_KEY, &request_idr).await;
			assert_eq!(drain(&mut idr), 1);

			// The active client's disconnect pauses delivery for its resume.
			if let Some(peer) = resumed.host.peer_mut(resumed.peer) {
				peer.disconnect(0);
			}
			resumed.pump(Duration::from_millis(150)).await;
			assert_eq!(pauses.load(Ordering::SeqCst), 1);

			stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
			tokio::time::timeout(Duration::from_secs(2), control)
				.await
				.unwrap()
				.unwrap();
		}
	}

	#[test]
	fn parses_sunshine_frame_fec_status_in_network_byte_order() {
		let mut packet = vec![0u8; 28];
		packet[0..2].copy_from_slice(&(ControlMessageType::FrameFecStatus as u16).to_le_bytes());
		packet[2..4].copy_from_slice(&24u16.to_le_bytes());
		packet[4..8].copy_from_slice(&42u32.to_be_bytes());
		packet[8..10].copy_from_slice(&500u16.to_be_bytes());
		packet[10..12].copy_from_slice(&497u16.to_be_bytes());
		packet[12..14].copy_from_slice(&3u16.to_be_bytes());
		packet[14..16].copy_from_slice(&100u16.to_be_bytes());
		packet[16..18].copy_from_slice(&20u16.to_be_bytes());
		packet[18..20].copy_from_slice(&98u16.to_be_bytes());
		packet[20..22].copy_from_slice(&18u16.to_be_bytes());
		packet[22] = 20;
		packet[23] = 1;
		packet[24] = 2;

		let ControlMessage::FrameFecStatus(status) = ControlMessage::from_bytes(&packet).unwrap() else {
			panic!("wrong control message type");
		};
		assert_eq!(status.frame_index, 42);
		assert_eq!(status.highest_received_sequence_number, 500);
		assert_eq!(status.next_contiguous_sequence_number, 497);
		assert_eq!(status.missing_packets_before_highest, 3);
		assert_eq!(status.total_data_packets, 100);
		assert_eq!(status.total_parity_packets, 20);
		assert_eq!(status.received_data_packets, 98);
		assert_eq!(status.received_parity_packets, 18);
		assert_eq!(status.fec_percentage, 20);
		assert_eq!(status.block_index, 1);
		assert_eq!(status.block_count, 2);
	}
}
