use std::net::SocketAddr;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_enet::{Event, Host, HostConfig, Packet, PacketMode, PeerState};

use self::{feedback::FeedbackCommand, input::InputHandler};
use crate::config::Config;
use crate::crypto::{decrypt, encrypt};
use crate::session::compositor::{
	frame::{HdrMetadata, HdrModeState},
	input::CompositorInputEvent,
};
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::audio::AudioStartHandle;
use crate::session::stream::video::VideoStreamHandle;
use crate::session::SessionContext;
use crate::session::SessionKeysReceiver;

mod feedback;
mod input;

const ENCRYPTION_TAG_LENGTH: usize = 16;
// Sequence number + tag + control message id.
const MINIMUM_ENCRYPTED_LENGTH: usize = 4 + ENCRYPTION_TAG_LENGTH + 4;

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
	SetRgbLed = 0x5502,
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
			x if x == Self::SetRgbLed as u16 => Ok(Self::SetRgbLed),
			x if x == Self::SetTriggerEffect as u16 => Ok(Self::SetTriggerEffect),
			_ => Err(()),
		}
	}
}

#[derive(Debug)]
enum ControlMessage<'a> {
	Encrypted(EncryptedControlMessage),
	TerminationExtended,
	RumbleData,
	HdrMode,
	Ping,
	LossStats,
	FrameStats,
	InputData(&'a [u8]),
	RequestIdrFrame,
	InvalidateReferenceFrames,
	StartB,
	RumbleTriggers,
	SetMotionEvent,
	SetRgbLed,
	SetTriggerEffect,
}

impl<'a> ControlMessage<'a> {
	fn from_bytes(buffer: &'a [u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			tracing::warn!(
				"Expected control message to have at least 4 bytes, got {}",
				buffer.len()
			);
			return Err(());
		}

		let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
		if length as usize != buffer.len() - 4 {
			tracing::warn!(
				"Received incorrect packet length: expecting {length} bytes, but buffer says it should be {} bytes.",
				buffer.len() - 4
			);
			return Err(());
		}

		match u16::from_le_bytes(buffer[..2].try_into().unwrap()).try_into()? {
			ControlMessageType::Encrypted => {
				if buffer.len() < MINIMUM_ENCRYPTED_LENGTH {
					tracing::warn!("Expected encrypted control message of at least {MINIMUM_ENCRYPTED_LENGTH} bytes, got buffer of {} bytes.", buffer.len());
					return Err(());
				}

				let length = u16::from_le_bytes(buffer[2..4].try_into().unwrap());
				if (length as usize) < MINIMUM_ENCRYPTED_LENGTH {
					tracing::warn!("Expected encrypted control message of at least {MINIMUM_ENCRYPTED_LENGTH} bytes, got reported length of {length} bytes.");
					return Err(());
				}

				let sequence_number = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
				Ok(Self::Encrypted(EncryptedControlMessage {
					length,
					sequence_number,
					tag: buffer[8..8 + ENCRYPTION_TAG_LENGTH]
						.try_into()
						.map_err(|e| tracing::warn!("Failed to get tag from encrypted control message: {e}"))?,
					payload: buffer[8 + ENCRYPTION_TAG_LENGTH..].to_vec(),
				}))
			},
			ControlMessageType::Ping => Ok(Self::Ping),
			ControlMessageType::TerminationExtended => Ok(Self::TerminationExtended),
			ControlMessageType::RumbleData => Ok(Self::RumbleData),
			ControlMessageType::LossStats => Ok(Self::LossStats),
			ControlMessageType::FrameStats => Ok(Self::FrameStats),
			ControlMessageType::InputData => {
				// Length of the input event, excluding the length itself.
				let length = u32::from_be_bytes(buffer[4..8].try_into().unwrap());
				if length as usize != buffer.len() - 8 {
					tracing::warn!("Failed to interpret input event message: expected {length} bytes, but buffer has {} bytes left.", buffer.len() - 8);
					return Err(());
				}

				Ok(Self::InputData(&buffer[8..]))
			},
			ControlMessageType::InvalidateReferenceFrames => Ok(Self::InvalidateReferenceFrames),
			ControlMessageType::RequestIdrFrame => Ok(Self::RequestIdrFrame),
			ControlMessageType::StartB => Ok(Self::StartB),
			ControlMessageType::HdrMode => Ok(Self::HdrMode),
			ControlMessageType::RumbleTriggers => Ok(Self::RumbleTriggers),
			ControlMessageType::SetMotionEvent => Ok(Self::SetMotionEvent),
			ControlMessageType::SetRgbLed => Ok(Self::SetRgbLed),
			ControlMessageType::SetTriggerEffect => Ok(Self::SetTriggerEffect),
		}
	}
}

#[derive(Debug)]
struct EncryptedControlMessage {
	length: u16,
	sequence_number: u32,
	tag: [u8; 16],
	payload: Vec<u8>,
}

impl EncryptedControlMessage {
	fn as_bytes(&self) -> Vec<u8> {
		let mut buffer = Vec::with_capacity(self.length as usize);

		buffer.extend((ControlMessageType::Encrypted as u16).to_le_bytes());
		buffer.extend(self.length.to_le_bytes());
		buffer.extend(self.sequence_number.to_le_bytes());
		buffer.extend(self.tag);
		buffer.extend(&self.payload);

		buffer
	}
}

fn encode_control(key: &[u8], sequence_number: u32, payload: &[u8]) -> Result<Vec<u8>, ()> {
	let mut initialization_vector = [0u8; 12];
	initialization_vector[0..4].copy_from_slice(&sequence_number.to_le_bytes());
	initialization_vector[10] = b'H';
	initialization_vector[11] = b'C';

	if key.len() != 16 {
		tracing::warn!("Key length has {} bytes, but expected {} bytes.", key.len(), 16);
		return Err(());
	}

	let mut tag = [0u8; 16];
	let payload = encrypt(payload, key, &initialization_vector, &mut tag)
		.map_err(|e| tracing::warn!("Failed to encrypt control data: {e}"))?;

	if payload.is_empty() {
		tracing::warn!("Failed to encrypt control data.");
		return Err(());
	}

	let message = EncryptedControlMessage {
		length: std::mem::size_of::<u32>() as u16 // Sequence number.
			 + ENCRYPTION_TAG_LENGTH as u16   // Tag.
			 + payload.len() as u16, // Payload.
		sequence_number,
		tag,
		payload,
	};

	Ok(message.as_bytes())
}

/// Context passed to the control stream from Session.
#[derive(Clone)]
pub(crate) struct ControlStreamContext {
	pub keys_rx: SessionKeysReceiver,
	pub hdr: bool,
}

impl ControlStreamContext {
	/// Create a `ControlStreamContext` from a session context and the *effective*
	/// HDR flag reported by the compositor after launch.
	///
	/// `hdr_effective` may differ from `ctx.hdr` when the client requested HDR but
	/// the GPU does not support an HDR-capable DMA-BUF format (compositor falls back
	/// to SDR and reports `hdr = false` in `CompositorReady`).
	pub fn new(ctx: &SessionContext, hdr_effective: bool) -> Self {
		Self {
			keys_rx: ctx.keys.clone_rx().expect("session keys not initialized"),
			hdr: hdr_effective,
		}
	}
}

pub(crate) struct ControlStream {
	config: Config,
	stop: ShutdownManager<SessionShutdownReason>,
	input_handler: InputHandler,
	host: Host,
}

impl ControlStream {
	pub fn new(
		config: Config,
		input_tx: calloop::channel::Sender<CompositorInputEvent>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		let input_handler = InputHandler::new(input_tx, stop_session_manager.clone(), config.gamepad.clone())?;

		let socket_address = SocketAddr::new(
			config
				.address
				.parse()
				.map_err(|e| tracing::warn!("Failed to parse address ({}): {e}", config.address))?,
			config.stream.control.port,
		);

		let host_config = HostConfig {
			address: Some(socket_address),
			peer_count: 1,
			channel_limit: 1,
			..Default::default()
		};

		let host = Host::new(host_config).map_err(|e| tracing::error!("Failed to create enet host: {e}"))?;

		tracing::debug!("Listening for control messages on {:?}", socket_address);

		Ok(Self {
			config,
			stop: stop_session_manager,
			input_handler,
			host,
		})
	}

	pub fn start(
		self,
		context: ControlStreamContext,
		video_handle: VideoStreamHandle,
		audio_trigger: AudioStartHandle,
		hdr_metadata_rx: watch::Receiver<HdrModeState>,
	) {
		let Self {
			config,
			stop: stop_session_manager,
			input_handler,
			host,
		} = self;

		tokio::spawn(async move {
			run_control_loop(
				config,
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

/// Send an encrypted control packet to the first peer of `host` if it is connected.
fn send_to_peer(host: &mut Host, key: &[u8], sequence_number: u32, payload: &[u8], label: &str) {
	if let Ok(packet) = encode_control(key, sequence_number, payload) {
		if let Some(peer) = host.peer_mut(tokio_enet::PeerId(0)) {
			if peer.state() == PeerState::Connected {
				let _ = peer
					.send(0, Packet::new(packet.as_slice(), PacketMode::ReliableSequenced))
					.map_err(|e| tracing::warn!("Failed to send {label} to peer: {e}"));
			}
		}
	}
}

/// Build and send an HDR mode control message, then advance `sequence_number`.
fn send_hdr_state(host: &mut Host, state: &HdrModeState, key: &[u8], sequence_number: &mut u32, label: &str) {
	let metadata = if state.enabled {
		Some(state.metadata.unwrap_or_else(HdrMetadata::fallback))
	} else {
		None
	};
	let payload = build_hdr_mode_payload(state.enabled, metadata.as_ref());
	send_to_peer(host, key, *sequence_number, &payload, label);
	*sequence_number += 1;
	tracing::info!("Sent HDR mode ({label}): enabled={}", state.enabled);
}

#[allow(clippy::too_many_arguments)]
async fn run_control_loop(
	config: Config,
	mut host: Host,
	video_handle: VideoStreamHandle,
	audio_trigger: AudioStartHandle,
	context: ControlStreamContext,
	input_handler: InputHandler,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	mut hdr_metadata_rx: watch::Receiver<HdrModeState>,
) {
	// Trigger session shutdown when the control stream stops.
	let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::ControlStreamStopped);
	let _delay_stop = stop_session_manager.delay_shutdown_token();

	let mut stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(config.stream_timeout);

	// Create a channel over which we can receive feedback messages to send to the connected client.
	let (feedback_tx, mut feedback_rx) = mpsc::channel::<FeedbackCommand>(10);

	// Sequence number of feedback messages.
	let mut sequence_number = 0u32;
	let mut send_hdr_mode = false;
	let mut audio_triggered = false;

	while !stop_session_manager.is_shutdown_triggered() {
		// Check if the timeout has passed.
		if std::time::Instant::now() > stop_deadline {
			tracing::info!(
				"Stopping because we haven't received a ping for {} seconds.",
				config.stream_timeout
			);
			break;
		}

		// Check for feedback messages.
		if let Ok(command) = feedback_rx.try_recv() {
			tracing::debug!("Sending control feedback command: {command:?}");
			let payload = command.as_packet();
			let key = context.keys_rx.borrow().remote_input_key.clone();
			send_to_peer(&mut host, &key, sequence_number, &payload, "feedback");
			sequence_number += 1;
		}

		match host
			.service(Duration::from_millis(10))
			.await
			.map_err(|e| tracing::error!("Failure in enet host: {e}"))
		{
			Ok(Some(Event::Connect { .. })) => {},
			Ok(Some(Event::Disconnect { .. })) => {},
			Ok(Some(Event::Receive { ref packet, .. })) => {
				let mut control_message = match ControlMessage::from_bytes(packet.data()) {
					Ok(control_message) => control_message,
					Err(()) => break,
				};
				tracing::trace!("Received control message: {control_message:?}");

				// First check for encrypted control messages and decrypt them.
				let decrypted;
				if let ControlMessage::Encrypted(message) = control_message {
					let mut initialization_vector = [0u8; 12];
					initialization_vector[0..4].copy_from_slice(&message.sequence_number.to_le_bytes());
					initialization_vector[10] = b'C';
					initialization_vector[11] = b'C';

					let keys = &*context.keys_rx.borrow();
					let decrypted_result = decrypt(
						&message.payload,
						&keys.remote_input_key,
						&initialization_vector,
						&message.tag,
					);

					decrypted = match decrypted_result {
						Ok(decrypted) => decrypted,
						Err(e) => {
							tracing::warn!("Failed to decrypt control message: {:?}", e);
							continue;
						},
					};

					control_message = match ControlMessage::from_bytes(&decrypted) {
						Ok(decrypted_message) => decrypted_message,
						Err(()) => continue,
					};

					tracing::trace!("Decrypted control message: {control_message:?}");
				}

				match control_message {
					ControlMessage::Encrypted(_) => {
						unreachable!("Encrypted control messages should be decrypted already.")
					},
					ControlMessage::RequestIdrFrame | ControlMessage::InvalidateReferenceFrames => {
						video_handle.request_idr_frame();
					},
					ControlMessage::StartB => {
						video_handle.trigger();
						if !audio_triggered {
							audio_trigger.trigger();
							audio_triggered = true;
						}
						send_hdr_mode = context.hdr;
					},
					ControlMessage::Ping => {
						stop_deadline =
							std::time::Instant::now() + std::time::Duration::from_secs(config.stream_timeout);
					},
					ControlMessage::InputData(event) => {
						let _ = input_handler.handle_raw_input(event, feedback_tx.clone()).await;
					},
					ControlMessage::HdrMode => {
						tracing::info!("Received HdrMode toggle from client");
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
			let state = hdr_metadata_rx.borrow_and_update().clone();
			let key = context.keys_rx.borrow().remote_input_key.clone();
			send_hdr_state(&mut host, &state, &key, &mut sequence_number, "initial");
		}

		// Check for HDR metadata updates from the video pipeline.
		if context.hdr && hdr_metadata_rx.has_changed().unwrap_or(false) {
			let state = hdr_metadata_rx.borrow_and_update().clone();
			let key = context.keys_rx.borrow().remote_input_key.clone();
			send_hdr_state(&mut host, &state, &key, &mut sequence_number, "metadata update");
		}
	}

	tracing::debug!("Control stream stopped.");

	// Notify the client of graceful termination before closing the connection.
	// NVST_DISCONN_SERVER_TERMINATED_CLOSED (0x80030023) is recognized by the
	// client as a graceful shutdown so it does not display an error.
	let termination_payload = build_termination_payload(0x80030023);
	let key = context.keys_rx.borrow().remote_input_key.clone();
	send_to_peer(&mut host, &key, sequence_number, &termination_payload, "termination");
	let _ = host.flush().await;

	// Explicitly drop the ENet host before the delay shutdown token
	// to ensure the socket is released before wait_shutdown_complete
	// returns. Without this, the next session's control stream may
	// fail to bind to the same port.
	drop(host);
}
