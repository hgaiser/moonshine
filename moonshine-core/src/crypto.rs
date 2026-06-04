use aes::cipher::{block_padding, BlockEncryptMut, KeyIvInit};
use aes_gcm::{
	aead::{Aead, KeyInit},
	Aes128Gcm, Key, Nonce,
};

pub(crate) fn encrypt(plaintext: &[u8], key: &[u8], iv: &[u8], tag: &mut [u8]) -> Result<Vec<u8>, aes_gcm::Error> {
	let key = Key::<Aes128Gcm>::from_slice(key);
	let nonce = Nonce::from_slice(iv);
	let cipher = Aes128Gcm::new(key);

	// In OpenSSL, encrypting with GCM returns ciphertext usually without tag appended if you use `tag()` to retrieve it separate.
	// aes-gcm crate append tag to ciphertext.
	let mut ciphertext = cipher.encrypt(nonce, plaintext)?;

	// Split tag from ciphertext
	let tag_len = 16;
	let len = ciphertext.len();
	if len < tag_len {
		return Err(aes_gcm::Error);
	}
	let actual_ciphertext_len = len - tag_len;

	tag.copy_from_slice(&ciphertext[actual_ciphertext_len..]);
	ciphertext.truncate(actual_ciphertext_len);

	Ok(ciphertext)
}

pub(crate) fn decrypt(ciphertext: &[u8], key: &[u8], iv: &[u8], tag: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
	let key = Key::<Aes128Gcm>::from_slice(key);
	let nonce = Nonce::from_slice(iv);
	let cipher = Aes128Gcm::new(key);

	// Append tag to ciphertext for aes-gcm crate
	let mut payload = Vec::with_capacity(ciphertext.len() + tag.len());
	payload.extend_from_slice(ciphertext);
	payload.extend_from_slice(tag);

	cipher.decrypt(nonce, payload.as_ref())
}

pub(crate) fn encrypt_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
	let key = key.into();
	let iv = iv.into();

	let cipher = cbc::Encryptor::<aes::Aes128>::new(key, iv);

	let mut buffer = vec![0u8; data.len() + 16];
	let pos = data.len();
	buffer[..pos].copy_from_slice(data);

	let ct_len = cipher
		.encrypt_padded_mut::<block_padding::Pkcs7>(&mut buffer, pos)
		.map_err(|e| format!("Padding error: {:?}", e))?
		.len();

	buffer.truncate(ct_len);
	Ok(buffer)
}
