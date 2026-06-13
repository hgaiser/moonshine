use aes::cipher::Array;
use aes::cipher::BlockModeEncrypt;
use aes::cipher::KeyIvInit;
use aes_gcm::{
	aead::{generic_array::GenericArray, AeadInPlace, KeyInit},
	Aes128Gcm, Key, Nonce,
};
use inout::block_padding::Pkcs7;

/// An AES-128-GCM cipher cached across calls, keyed by the control stream's
/// input key.
///
/// The control stream encrypts feedback and decrypts input using
/// `remote_input_key`, which only changes on key rotation (tracked by
/// `remote_input_key_id`). Rebuilding the cipher — a full AES key schedule plus
/// GHASH table — on every packet showed up on the per-input-event path, so we
/// cache it and only rebuild when the key id changes.
pub(crate) struct GcmCipher {
	cipher: Option<Aes128Gcm>,
	key_id: i64,
}

impl GcmCipher {
	pub fn new() -> Self {
		Self {
			cipher: None,
			key_id: i64::MIN,
		}
	}

	/// Return the cached cipher, rebuilding it if the key has rotated.
	fn get(&mut self, key: &[u8], key_id: i64) -> Result<&Aes128Gcm, ()> {
		if self.cipher.is_none() || self.key_id != key_id {
			if key.len() != 16 {
				tracing::warn!("Control key must be 16 bytes, got {}.", key.len());
				self.cipher = None;
				return Err(());
			}
			self.cipher = Some(Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(key)));
			self.key_id = key_id;
		}
		self.cipher.as_ref().ok_or(())
	}

	/// Encrypt `buffer` in place, returning the detached 16-byte tag.
	pub fn encrypt(&mut self, key: &[u8], key_id: i64, iv: &[u8], buffer: &mut [u8]) -> Result<[u8; 16], ()> {
		let cipher = self.get(key, key_id)?;
		let tag = cipher
			.encrypt_in_place_detached(Nonce::from_slice(iv), b"", buffer)
			.map_err(|e| tracing::warn!("Failed to encrypt control data: {e}"))?;
		let mut out = [0u8; 16];
		out.copy_from_slice(&tag);
		Ok(out)
	}

	/// Decrypt `buffer` in place using the detached `tag`.
	pub fn decrypt(&mut self, key: &[u8], key_id: i64, iv: &[u8], tag: &[u8], buffer: &mut [u8]) -> Result<(), ()> {
		let cipher = self.get(key, key_id)?;
		cipher
			.decrypt_in_place_detached(Nonce::from_slice(iv), b"", buffer, GenericArray::from_slice(tag))
			.map_err(|e| tracing::warn!("Failed to decrypt control message: {e}"))
	}
}

pub(crate) fn encrypt_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
	let key: &Array<u8, _> = key.try_into().map_err(|_| "Invalid CBC key length")?;
	let iv: &Array<u8, _> = iv.try_into().map_err(|_| "Invalid CBC IV length")?;

	let cipher = cbc::Encryptor::<aes::Aes128>::new(key, iv);

	let pad_len = 16 - (data.len() % 16);
	let mut buffer = vec![0u8; data.len() + pad_len];
	buffer[..data.len()].copy_from_slice(data);

	let ct = cipher
		.encrypt_padded::<Pkcs7>(&mut buffer, data.len())
		.map_err(|_| "Buffer too small for CBC encryption")?;
	Ok(ct.to_vec())
}
