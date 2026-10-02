//! Authorization of ENet control peers.
//!
//! An ENet connection is not an identity. A peer becomes a *candidate* only if
//! its address and connect data match the current launch/resume generation
//! (see `session::authorization`), and becomes the *active* peer only after it
//! sends a control message authenticated with that generation's AES-GCM key,
//! which the client received over the paired HTTPS launch/resume request.
//! Only the active peer's messages are dispatched, only they extend the stream
//! timeout, and only the active peer receives feedback.
//!
//! Every supported client encrypts all control messages: the server advertises
//! the Sunshine `ControlV2` encryption flag and protocol version 7.1.431, for
//! which moonlight-common-c always encrypts. Plaintext is therefore rejected.
//!
//! Authenticated sequence numbers are recorded per generation (not per peer),
//! so a captured message cannot be replayed through the same or another peer.

use std::net::SocketAddr;

use tokio_enet::PeerId;

use super::{ControlMessage, ControlParseError, decrypt_control};
use crate::session::authorization::StreamAuthorization;

/// Sequence numbers tracked behind the highest authenticated one. Moonlight
/// numbers messages from 0 per connection and may reorder messages sent on
/// different channels or unsequenced; this tolerates that reordering.
pub(super) const REPLAY_WINDOW: u32 = 1024;
const REPLAY_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// Sliding anti-replay window over authenticated sequence numbers.
pub(super) struct ReplayWindow {
	highest: Option<u32>,
	seen: [u64; REPLAY_WORDS],
}

impl ReplayWindow {
	pub(super) fn new() -> Self {
		Self {
			highest: None,
			seen: [0; REPLAY_WORDS],
		}
	}

	fn bit(sequence: u32) -> (usize, u64) {
		let index = (sequence % REPLAY_WINDOW) as usize;
		(index / 64, 1 << (index % 64))
	}

	/// Record `sequence` and report whether it was fresh. Call only after the
	/// message authenticated, so forged sequence numbers cannot move the window.
	pub(super) fn accept(&mut self, sequence: u32) -> bool {
		let Some(highest) = self.highest else {
			self.highest = Some(sequence);
			let (word, mask) = Self::bit(sequence);
			self.seen[word] |= mask;
			return true;
		};
		if sequence > highest {
			if sequence - highest >= REPLAY_WINDOW {
				self.seen = [0; REPLAY_WORDS];
			} else {
				for skipped in highest + 1..=sequence {
					let (word, mask) = Self::bit(skipped);
					self.seen[word] &= !mask;
				}
			}
			self.highest = Some(sequence);
		} else if highest - sequence >= REPLAY_WINDOW {
			return false;
		}
		let (word, mask) = Self::bit(sequence);
		let fresh = self.seen[word] & mask == 0;
		self.seen[word] |= mask;
		fresh
	}
}

/// Why a received packet was not dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rejection {
	/// The peer is neither a candidate nor the active peer of this generation.
	Unauthorized,
	/// The outer packet is not a well-formed control message.
	Malformed(ControlParseError),
	/// A well-formed but unencrypted control message.
	Plaintext,
	/// AES-GCM authentication failed (wrong or stale key, forged message).
	Authentication,
	/// The authenticated sequence number was already used or is too old.
	Replay,
	/// Another peer already proved this generation's key.
	Superseded,
}

/// Control peers of the current launch/resume generation.
pub(super) struct ControlPeers {
	generation: u64,
	active: Option<PeerId>,
	candidates: Vec<PeerId>,
	replay: ReplayWindow,
}

impl ControlPeers {
	pub(super) fn new(generation: u64) -> Self {
		Self {
			generation,
			active: None,
			candidates: Vec::new(),
			replay: ReplayWindow::new(),
		}
	}

	/// The peer that authenticated the current generation, if any.
	pub(super) fn active(&self) -> Option<PeerId> {
		self.active
	}

	/// Adopt a new launch/resume generation (new client and key). Returns the
	/// previous generation's peers, which must be disconnected.
	pub(super) fn begin_generation(&mut self, generation: u64) -> Vec<PeerId> {
		if generation == self.generation {
			return Vec::new();
		}
		self.generation = generation;
		self.replay = ReplayWindow::new();
		let mut stale: Vec<_> = self.candidates.drain(..).collect();
		stale.extend(self.active.take());
		stale
	}

	/// Admit a newly connected peer as a candidate. `authorization` must be the
	/// generation this tracker was last advanced to.
	pub(super) fn connect(
		&mut self,
		peer: PeerId,
		from: Option<SocketAddr>,
		data: u32,
		authorization: &StreamAuthorization,
	) -> bool {
		let admitted = authorization.generation() == self.generation
			&& from.is_some_and(|from| authorization.admits_control_connect(from, data));
		if admitted {
			self.candidates.push(peer);
		}
		admitted
	}

	/// Forget a disconnected peer. Returns whether it was the active peer.
	pub(super) fn disconnect(&mut self, peer: PeerId) -> bool {
		self.candidates.retain(|candidate| *candidate != peer);
		if self.active == Some(peer) {
			self.active = None;
			true
		} else {
			false
		}
	}

	/// Authenticate a received packet and return its decrypted control message.
	pub(super) fn authenticate(&mut self, peer: PeerId, packet: &[u8], key: &[u8]) -> Result<Vec<u8>, Rejection> {
		let is_active = self.active == Some(peer);
		if !is_active && !self.candidates.contains(&peer) {
			return Err(Rejection::Unauthorized);
		}
		let (sequence_number, tag, ciphertext) = match ControlMessage::from_bytes(packet) {
			Ok(ControlMessage::Encrypted {
				sequence_number,
				tag,
				ciphertext,
			}) => (sequence_number, tag, ciphertext),
			Ok(_) => return Err(Rejection::Plaintext),
			Err(error) => return Err(Rejection::Malformed(error)),
		};
		if !is_active && self.active.is_some() {
			return Err(Rejection::Superseded);
		}
		let plaintext =
			decrypt_control(key, sequence_number, &tag, ciphertext).map_err(|()| Rejection::Authentication)?;
		if !self.replay.accept(sequence_number) {
			return Err(Rejection::Replay);
		}
		if !is_active {
			self.candidates.retain(|candidate| *candidate != peer);
			self.active = Some(peer);
		}
		Ok(plaintext)
	}
}

#[cfg(test)]
mod tests {
	use std::net::{IpAddr, Ipv4Addr};

	use super::super::{encode_client_control, plaintext_control};
	use super::*;

	const KEY: [u8; 16] = [7; 16];

	fn peer(id: usize) -> PeerId {
		PeerId(id)
	}

	fn authorization(generation: u64) -> StreamAuthorization {
		let mut authorization = StreamAuthorization::new(generation, IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
		authorization.require_session_id();
		authorization
	}

	fn local(port: u16) -> Option<SocketAddr> {
		Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
	}

	#[test]
	fn replay_window_accepts_reordering_and_rejects_duplicates() {
		let mut window = ReplayWindow::new();
		assert!(window.accept(0));
		assert!(!window.accept(0));
		assert!(window.accept(5));
		assert!(window.accept(3), "reordered message within the window");
		assert!(!window.accept(3));
		assert!(window.accept(4));
		assert!(window.accept(5 + REPLAY_WINDOW - 1));
		assert!(!window.accept(5), "oldest tracked sequence was already seen");
		assert!(!window.accept(1), "older than the window");
		assert!(window.accept(6 + REPLAY_WINDOW - 1));
		assert!(!window.accept(6 + REPLAY_WINDOW - 1));
		// A large jump clears the whole window without accepting old numbers.
		assert!(window.accept(u32::MAX - 1));
		assert!(!window.accept(10 * REPLAY_WINDOW));
		assert!(window.accept(u32::MAX));
		assert!(!window.accept(u32::MAX));
	}

	#[test]
	fn replay_window_clears_reused_slots_when_advancing() {
		let mut window = ReplayWindow::new();
		for sequence in 0..4 * REPLAY_WINDOW {
			assert!(window.accept(sequence), "sequence {sequence}");
		}
		for sequence in 3 * REPLAY_WINDOW + 1..4 * REPLAY_WINDOW {
			assert!(!window.accept(sequence), "sequence {sequence}");
		}
	}

	#[test]
	fn connection_requires_address_and_connect_data() {
		let auth = authorization(1);
		let mut peers = ControlPeers::new(1);
		let other = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 1));
		assert!(!peers.connect(peer(0), other, auth.control_connect_data(), &auth));
		assert!(!peers.connect(peer(1), local(1), auth.control_connect_data() ^ 1, &auth));
		assert!(!peers.connect(peer(2), local(1), 0, &auth));
		assert!(!peers.connect(peer(3), None, auth.control_connect_data(), &auth));
		assert!(peers.connect(peer(4), local(1), auth.control_connect_data(), &auth));
		assert!(
			!peers.connect(
				peer(5),
				local(1),
				authorization(2).control_connect_data(),
				&authorization(2)
			),
			"authorization for a generation the tracker has not adopted"
		);
	}

	#[test]
	fn only_an_authenticated_peer_becomes_active() {
		let auth = authorization(1);
		let mut peers = ControlPeers::new(1);
		for id in 0..4 {
			assert!(peers.connect(peer(id), local(id as u16), auth.control_connect_data(), &auth));
		}
		let request_idr = plaintext_control(0x0302, &[]);

		assert_eq!(
			peers.authenticate(peer(9), &encode_client_control(&KEY, 0, &request_idr), &KEY),
			Err(Rejection::Unauthorized)
		);
		assert_eq!(
			peers.authenticate(peer(0), &request_idr, &KEY),
			Err(Rejection::Plaintext)
		);
		assert_eq!(
			peers.authenticate(peer(1), &encode_client_control(&[8; 16], 0, &request_idr), &KEY),
			Err(Rejection::Authentication)
		);
		assert!(matches!(
			peers.authenticate(peer(2), &[0x06, 0x02, 0, 0], &KEY),
			Err(Rejection::Malformed(ControlParseError::Truncated { .. }))
		));
		assert_eq!(peers.active(), None);

		let first = encode_client_control(&KEY, 0, &request_idr);
		assert_eq!(peers.authenticate(peer(3), &first, &KEY), Ok(request_idr.clone()));
		assert_eq!(peers.active(), Some(peer(3)));
		assert_eq!(peers.authenticate(peer(3), &first, &KEY), Err(Rejection::Replay));
		// Replays and fresh messages from another peer cannot take over.
		assert!(peers.connect(peer(4), local(4), auth.control_connect_data(), &auth));
		assert_eq!(
			peers.authenticate(peer(4), &encode_client_control(&KEY, 1, &request_idr), &KEY),
			Err(Rejection::Superseded)
		);
		assert_eq!(peers.active(), Some(peer(3)));
		assert_eq!(
			peers.authenticate(peer(3), &encode_client_control(&KEY, 1, &request_idr), &KEY),
			Ok(request_idr)
		);
	}

	#[test]
	fn new_generation_disconnects_previous_peers_and_resets_replay_state() {
		let old = authorization(1);
		let mut peers = ControlPeers::new(1);
		assert!(peers.connect(peer(0), local(1), old.control_connect_data(), &old));
		assert!(peers.connect(peer(1), local(2), old.control_connect_data(), &old));
		let message = plaintext_control(0x0200, &[]);
		peers
			.authenticate(peer(0), &encode_client_control(&KEY, 0, &message), &KEY)
			.unwrap();

		let mut stale = peers.begin_generation(2);
		stale.sort_by_key(|peer| peer.0);
		assert_eq!(stale, vec![peer(0), peer(1)]);
		assert_eq!(peers.active(), None);
		assert!(peers.begin_generation(2).is_empty());
		assert_eq!(
			peers.authenticate(peer(0), &encode_client_control(&KEY, 1, &message), &KEY),
			Err(Rejection::Unauthorized)
		);
		// The new client numbers its messages from 0 again under its new key.
		let new = authorization(2);
		let new_key = [9; 16];
		assert!(!peers.connect(peer(2), local(3), old.control_connect_data(), &new));
		assert!(peers.connect(peer(3), local(3), new.control_connect_data(), &new));
		assert_eq!(
			peers.authenticate(peer(3), &encode_client_control(&KEY, 0, &message), &new_key),
			Err(Rejection::Authentication),
			"old key"
		);
		assert_eq!(
			peers.authenticate(peer(3), &encode_client_control(&new_key, 0, &message), &new_key),
			Ok(message)
		);
		assert!(peers.disconnect(peer(3)));
		assert!(!peers.disconnect(peer(3)));
	}
}
