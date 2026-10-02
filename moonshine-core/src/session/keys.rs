//! Session encryption keys and key-scoped AES-GCM nonce ownership.
//!
//! An authenticated `/launch` or `/resume` carries `rikey` (the AES-128 key the
//! client generated for this stream) and `rikeyid` (a 32-bit value the client
//! also uses as the audio IV prefix). Both are validated here, at the
//! authenticated boundary, before any session state changes.
//!
//! Every AES-GCM nonce the host emits is allocated from a [`KeyNonces`] owned by
//! the *key bytes*, never by a packetizer, encoder epoch or control task. A
//! `KeyLedger` in the session manager hands out the same sequences whenever
//! the same key reappears (unchanged resume, key-ID-only change, reconfigure,
//! or a later launch that reuses a key), so recreating any stream object
//! cannot restart a counter for a key that already used it. The client-supplied
//! key ID never participates in change detection: each publication receives a
//! server-owned [`KeyMaterial::generation`].

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

/// AES-128 key length. Moonlight's `remoteInputAesKey` is a fixed 16-byte array
/// sent as 32 hex digits.
pub const REMOTE_INPUT_KEY_LEN: usize = 16;

/// Why a launch/resume key parameter was rejected. Messages never echo key bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
	/// `rikey` is not hexadecimal.
	InvalidHex,
	/// `rikey` does not decode to exactly [`REMOTE_INPUT_KEY_LEN`] bytes.
	InvalidLength(usize),
	/// `rikeyid` is not a 32-bit decimal value.
	InvalidKeyId,
}

impl fmt::Display for KeyError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidHex => write!(f, "'rikey' is not valid hexadecimal"),
			Self::InvalidLength(len) => {
				write!(f, "'rikey' must be {REMOTE_INPUT_KEY_LEN} bytes, got {len}")
			},
			Self::InvalidKeyId => write!(f, "'rikeyid' must be a 32-bit decimal integer"),
		}
	}
}

/// A validated AES-128 session key.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteInputKey([u8; REMOTE_INPUT_KEY_LEN]);

impl RemoteInputKey {
	pub const fn from_bytes(bytes: [u8; REMOTE_INPUT_KEY_LEN]) -> Self {
		Self(bytes)
	}

	/// Parse `rikey`: exactly 32 hexadecimal digits.
	pub fn from_hex(value: &str) -> Result<Self, KeyError> {
		let bytes = hex::decode(value).map_err(|error| match error {
			hex::FromHexError::OddLength => KeyError::InvalidLength(value.len() / 2),
			_ => KeyError::InvalidHex,
		})?;
		let len = bytes.len();
		bytes.try_into().map(Self).map_err(|_| KeyError::InvalidLength(len))
	}

	pub fn as_bytes(&self) -> &[u8; REMOTE_INPUT_KEY_LEN] {
		&self.0
	}

	fn fingerprint(&self) -> [u8; 32] {
		let mut hasher = Sha256::new();
		hasher.update(b"moonshine key-scoped nonce ledger v1");
		hasher.update(self.0);
		hasher.finalize().into()
	}
}

impl fmt::Debug for RemoteInputKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RemoteInputKey(<redacted>)")
	}
}

/// The client's 32-bit `rikeyid`.
///
/// Moonlight formats it as a signed `int`; other clients may format the same
/// 32 bits unsigned. Both spellings are accepted and normalized, matching the
/// client's `BE32(avRiKeyId + sequence)` audio IV arithmetic. Values outside
/// either 32-bit range are rejected rather than truncated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteInputKeyId(u32);

impl RemoteInputKeyId {
	pub const fn new(value: u32) -> Self {
		Self(value)
	}

	pub fn parse(value: &str) -> Result<Self, KeyError> {
		let value: i64 = value.parse().map_err(|_| KeyError::InvalidKeyId)?;
		if (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&value) {
			Ok(Self(value as u32))
		} else {
			Err(KeyError::InvalidKeyId)
		}
	}

	pub const fn get(self) -> u32 {
		self.0
	}
}

/// Validated key parameters from an authenticated launch/resume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionKeyData {
	pub key: RemoteInputKey,
	pub key_id: RemoteInputKeyId,
}

impl SessionKeyData {
	pub fn new(key: RemoteInputKey, key_id: RemoteInputKeyId) -> Self {
		Self { key, key_id }
	}

	/// Validate the `rikey`/`rikeyid` pair of a launch/resume request.
	pub fn from_params(rikey: &str, rikeyid: &str) -> Result<Self, KeyError> {
		Ok(Self::new(
			RemoteInputKey::from_hex(rikey)?,
			RemoteInputKeyId::parse(rikeyid)?,
		))
	}
}

/// A nonce counter was exhausted; the key must not encrypt anything further.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceExhausted;

/// Monotonic, never-wrapping allocator for one nonce counter field of one key.
///
/// Allocation is a single lock-free `fetch_update` per reservation, so the
/// video hot path reserves a whole FEC block of nonces at once and contends
/// only with other users of the same key.
#[derive(Debug)]
pub struct NonceSequence {
	next: AtomicU64,
	/// Exclusive upper bound: the size of the counter field in the wire nonce.
	end: u64,
}

impl NonceSequence {
	fn new(start: u64, end: u64) -> Self {
		Self {
			next: AtomicU64::new(start.min(end)),
			end,
		}
	}

	/// Reserve `count` consecutive counters and return the first. A reservation
	/// that would reach `end` fails without handing anything out; counters
	/// never wrap and are never handed out twice.
	pub fn reserve(&self, count: u64) -> Result<u64, NonceExhausted> {
		self.next
			.fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
				next.checked_add(count).filter(|&after| after <= self.end)
			})
			.map_err(|_| NonceExhausted)
	}

	/// First counter not yet handed out.
	pub fn high_water(&self) -> u64 {
		self.next.load(Ordering::Acquire)
	}
}

/// Video GCM IVs carry a 64-bit little-endian counter in bytes 0..8 (byte 11 = 'V').
pub const VIDEO_NONCE_END: u64 = u64::MAX;
/// Host control GCM IVs carry the 32-bit envelope sequence number (bytes 10..12 = "HC").
pub const CONTROL_NONCE_END: u64 = 1 << 32;

/// All host-originated AES-GCM nonce spaces of one key. Direction bytes in
/// the wire IVs keep these spaces (and the client's) disjoint.
#[derive(Debug)]
pub struct KeyNonces {
	pub video: NonceSequence,
	pub control: NonceSequence,
}

impl KeyNonces {
	fn starting_at(floor: NonceFloor) -> Self {
		Self {
			video: NonceSequence::new(floor.video, VIDEO_NONCE_END),
			control: NonceSequence::new(floor.control, CONTROL_NONCE_END),
		}
	}

	#[cfg(test)]
	pub(crate) fn for_test(video_start: u64, control_start: u64) -> Arc<Self> {
		Arc::new(Self::starting_at(NonceFloor {
			video: video_start,
			control: control_start,
		}))
	}
}

/// A published key: validated bytes, a server-owned generation and the
/// key-scoped nonce owner. Consumers compare `generation`, never the key ID.
pub struct KeyMaterial {
	key: RemoteInputKey,
	generation: u64,
	nonces: Arc<KeyNonces>,
}

impl KeyMaterial {
	pub fn key(&self) -> &RemoteInputKey {
		&self.key
	}

	pub fn generation(&self) -> u64 {
		self.generation
	}

	pub fn nonces(&self) -> &Arc<KeyNonces> {
		&self.nonces
	}
}

impl fmt::Debug for KeyMaterial {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("KeyMaterial")
			.field("generation", &self.generation)
			.finish_non_exhaustive()
	}
}

/// The key state watched by the audio, video and control workers.
#[derive(Clone, Debug)]
pub struct ActiveKeys {
	material: Arc<KeyMaterial>,
	key_id: RemoteInputKeyId,
}

impl ActiveKeys {
	pub fn material(&self) -> &Arc<KeyMaterial> {
		&self.material
	}

	pub fn key(&self) -> &RemoteInputKey {
		&self.material.key
	}

	pub fn key_id(&self) -> RemoteInputKeyId {
		self.key_id
	}

	/// Keys for tests and tools that run without a session manager. Each call
	/// gets independent nonce sequences, so callers must use distinct keys.
	#[cfg(test)]
	pub(crate) fn for_test(key: [u8; REMOTE_INPUT_KEY_LEN], key_id: u32) -> Self {
		KeyLedger::default().publish(SessionKeyData::new(
			RemoteInputKey::from_bytes(key),
			RemoteInputKeyId::new(key_id),
		))
	}

	/// Keys sharing injected nonce sequences (e.g. near exhaustion), published
	/// under a fresh generation.
	#[cfg(test)]
	pub(crate) fn with_nonces(key: [u8; REMOTE_INPUT_KEY_LEN], key_id: u32, nonces: Arc<KeyNonces>) -> Self {
		static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1 << 40);
		Self {
			material: Arc::new(KeyMaterial {
				key: RemoteInputKey::from_bytes(key),
				generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
				nonces,
			}),
			key_id: RemoteInputKeyId::new(key_id),
		}
	}
}

#[derive(Clone, Copy, Debug, Default)]
struct NonceFloor {
	video: u64,
	control: u64,
}

/// Retained key fingerprints. Each distinct key adds one small entry; past this
/// bound, entries no longer referenced by any published key are folded into
/// the floor below.
const MAX_LEDGER_ENTRIES: usize = 4096;

/// Process-lifetime owner of nonce sequences, keyed by a one-way fingerprint
/// of the key (key bytes are not retained after a session ends).
#[derive(Default)]
pub(crate) struct KeyLedger {
	sequences: HashMap<[u8; 32], Arc<KeyNonces>>,
	/// Start for keys not in the ledger: at least the high-water mark of every
	/// evicted entry, so a key returning after eviction still cannot reuse a
	/// counter (any start value is valid on the wire).
	floor: NonceFloor,
	next_generation: u64,
}

impl KeyLedger {
	/// Publish validated keys, sharing nonce sequences with every earlier use
	/// of the same key bytes.
	pub(crate) fn publish(&mut self, keys: SessionKeyData) -> ActiveKeys {
		let fingerprint = keys.key.fingerprint();
		if !self.sequences.contains_key(&fingerprint) && self.sequences.len() >= MAX_LEDGER_ENTRIES {
			self.evict_unused();
		}
		let floor = self.floor;
		let nonces = self
			.sequences
			.entry(fingerprint)
			.or_insert_with(|| Arc::new(KeyNonces::starting_at(floor)))
			.clone();
		self.next_generation += 1;
		ActiveKeys {
			material: Arc::new(KeyMaterial {
				key: keys.key,
				generation: self.next_generation,
				nonces,
			}),
			key_id: keys.key_id,
		}
	}

	fn evict_unused(&mut self) {
		let floor = &mut self.floor;
		self.sequences.retain(|_, nonces| {
			if Arc::strong_count(nonces) > 1 {
				return true;
			}
			floor.video = floor.video.max(nonces.video.high_water());
			floor.control = floor.control.max(nonces.control.high_water());
			false
		});
	}

	#[cfg(test)]
	fn len(&self) -> usize {
		self.sequences.len()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn key_shape_is_exactly_sixteen_bytes_of_hex() {
		assert!(RemoteInputKey::from_hex(&"ab".repeat(16)).is_ok());
		assert!(RemoteInputKey::from_hex(&"AB".repeat(16)).is_ok());
		for (value, error) in [
			(String::new(), KeyError::InvalidLength(0)),
			("ab".repeat(15), KeyError::InvalidLength(15)),
			("ab".repeat(17), KeyError::InvalidLength(17)),
			("abc".to_string(), KeyError::InvalidLength(1)),
			("zz".repeat(16), KeyError::InvalidHex),
			(format!("{} ", "ab".repeat(16)), KeyError::InvalidLength(16)),
		] {
			assert_eq!(RemoteInputKey::from_hex(&value), Err(error), "{value:?}");
		}
		assert_eq!(
			RemoteInputKey::from_hex(&format!("{}g", "a".repeat(31))),
			Err(KeyError::InvalidHex)
		);
	}

	#[test]
	fn key_ids_accept_signed_and_unsigned_32_bit_forms() {
		for (value, expected) in [
			("0", 0),
			("1", 1),
			("-1", u32::MAX),
			("2147483647", i32::MAX as u32),
			("-2147483648", i32::MIN as u32),
			("4294967295", u32::MAX),
			("2147483648", 1 << 31),
		] {
			assert_eq!(RemoteInputKeyId::parse(value).unwrap().get(), expected, "{value}");
		}
		for value in [
			"",
			"x",
			"1.5",
			"4294967296",
			"-2147483649",
			"9223372036854775807",
			"-9223372036854775808",
			"99999999999999999999",
		] {
			assert_eq!(RemoteInputKeyId::parse(value), Err(KeyError::InvalidKeyId), "{value}");
		}
	}

	#[test]
	fn debug_output_redacts_keys() {
		let keys = SessionKeyData::from_params(&"7f".repeat(16), "5").unwrap();
		let rendered = format!("{keys:?} {:?}", KeyLedger::default().publish(keys.clone()));
		assert!(!rendered.contains("127"), "{rendered}");
		assert!(!rendered.to_lowercase().contains("7f7f"), "{rendered}");
	}

	#[test]
	fn same_key_shares_nonces_across_publications_and_key_ids() {
		let mut ledger = KeyLedger::default();
		let key = RemoteInputKey::from_bytes([1; 16]);
		let first = ledger.publish(SessionKeyData::new(key.clone(), RemoteInputKeyId::new(1)));
		let same_id = ledger.publish(SessionKeyData::new(key.clone(), RemoteInputKeyId::new(1)));
		let new_id = ledger.publish(SessionKeyData::new(key, RemoteInputKeyId::new(i32::MIN as u32)));
		let other = ledger.publish(SessionKeyData::new(
			RemoteInputKey::from_bytes([2; 16]),
			RemoteInputKeyId::new(1),
		));

		// Generations are server-owned and distinct for every publication.
		let generations = [&first, &same_id, &new_id, &other].map(|keys| keys.material().generation());
		assert!(generations.windows(2).all(|pair| pair[0] < pair[1]));

		assert!(Arc::ptr_eq(first.material().nonces(), same_id.material().nonces()));
		assert!(Arc::ptr_eq(first.material().nonces(), new_id.material().nonces()));
		assert!(!Arc::ptr_eq(first.material().nonces(), other.material().nonces()));
		assert_eq!(first.material().nonces().video.reserve(10), Ok(0));
		assert_eq!(new_id.material().nonces().video.reserve(1), Ok(10));
		assert_eq!(other.material().nonces().video.reserve(1), Ok(0));
	}

	#[test]
	fn exhaustion_fails_before_reuse_and_is_sticky() {
		let sequence = NonceSequence::new(VIDEO_NONCE_END - 3, VIDEO_NONCE_END);
		assert_eq!(sequence.reserve(2), Ok(VIDEO_NONCE_END - 3));
		assert_eq!(sequence.reserve(2), Err(NonceExhausted), "would hand out END");
		assert_eq!(sequence.reserve(1), Ok(VIDEO_NONCE_END - 1));
		assert_eq!(sequence.reserve(1), Err(NonceExhausted));
		assert_eq!(sequence.reserve(0), Ok(VIDEO_NONCE_END));
		assert_eq!(sequence.reserve(u64::MAX), Err(NonceExhausted), "no wrapping add");

		let control = NonceSequence::new(CONTROL_NONCE_END - 1, CONTROL_NONCE_END);
		assert_eq!(control.reserve(1), Ok(u64::from(u32::MAX)));
		assert_eq!(control.reserve(1), Err(NonceExhausted));
	}

	#[test]
	fn concurrent_reservations_are_disjoint() {
		let nonces = KeyNonces::for_test(0, 0);
		let threads: Vec<_> = (0..4)
			.map(|_| {
				let nonces = nonces.clone();
				std::thread::spawn(move || (0..1000).map(|_| nonces.video.reserve(3).unwrap()).collect::<Vec<_>>())
			})
			.collect();
		let mut starts: Vec<u64> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
		starts.sort_unstable();
		assert!(starts.windows(2).all(|pair| pair[1] - pair[0] == 3));
		assert_eq!(nonces.video.high_water(), 12_000);
	}

	#[test]
	fn evicted_keys_resume_above_their_previous_high_water() {
		let mut ledger = KeyLedger::default();
		let reused = RemoteInputKey::from_bytes([0xaa; 16]);
		let first = ledger.publish(SessionKeyData::new(reused.clone(), RemoteInputKeyId::new(0)));
		first.material().nonces().video.reserve(500).unwrap();
		first.material().nonces().control.reserve(7).unwrap();
		drop(first);

		// A still-published key is never evicted.
		let live = ledger.publish(SessionKeyData::new(
			RemoteInputKey::from_bytes([0xbb; 16]),
			RemoteInputKeyId::new(0),
		));
		for index in 0..MAX_LEDGER_ENTRIES as u32 {
			let mut bytes = [0u8; 16];
			bytes[..4].copy_from_slice(&index.to_le_bytes());
			bytes[15] = 0x55;
			ledger.publish(SessionKeyData::new(
				RemoteInputKey::from_bytes(bytes),
				RemoteInputKeyId::new(0),
			));
		}
		assert!(ledger.len() <= MAX_LEDGER_ENTRIES);
		let again = ledger.publish(SessionKeyData::new(reused, RemoteInputKeyId::new(0)));
		assert!(again.material().nonces().video.reserve(1).unwrap() >= 500);
		assert!(again.material().nonces().control.reserve(1).unwrap() >= 7);
		let live_again = ledger.publish(SessionKeyData::new(
			RemoteInputKey::from_bytes([0xbb; 16]),
			RemoteInputKeyId::new(0),
		));
		assert!(Arc::ptr_eq(live.material().nonces(), live_again.material().nonces()));
	}
}
