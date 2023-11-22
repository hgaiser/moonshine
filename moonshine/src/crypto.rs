use openssl::{cipher::{Cipher, CipherRef}, cipher_ctx::CipherCtx};

pub fn encrypt(cipher: &CipherRef, plaintext: &[u8], key: Option<&[u8]>, iv: Option<&[u8]>, padding: bool) -> Result<Vec<u8>, openssl::error::ErrorStack> {
	let mut context = CipherCtx::new()?;
	context.encrypt_init(Some(cipher), key, iv)?;
	context.set_padding(padding);

	let mut ciphertext = Vec::with_capacity(plaintext.len());
	context.cipher_update_vec(plaintext, &mut ciphertext)?;
	context.cipher_final_vec(&mut ciphertext)?;

	Ok(ciphertext)
}

pub fn decrypt(cipher: &CipherRef, ciphertext: &[u8], key: &[u8]) -> Result<Vec<u8>, openssl::error::ErrorStack> {
	let mut context = CipherCtx::new()?;
	context.decrypt_init(Some(cipher), Some(key), None)?;
	context.set_padding(false);

	let mut plaintext = Vec::with_capacity(ciphertext.len());
	context.cipher_update_vec(ciphertext, &mut plaintext)?;
	context.cipher_final_vec(&mut plaintext)?;

	if plaintext.len() != ciphertext.len() {
		panic!("Cipher and plaintext should be the same length, but are {} vs {}.", plaintext.len(), ciphertext.len());
	}

	Ok(plaintext)
}