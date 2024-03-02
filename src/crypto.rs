use openssl::{
	asn1::Asn1Time,
	bn::{BigNum, MsbOption},
	cipher::CipherRef,
	cipher_ctx::CipherCtx,
	error::ErrorStack,
	hash::MessageDigest,
	pkey::{PKey, Private},
	rsa::Rsa,
	x509::{
		extension::{
			BasicConstraints, KeyUsage, SubjectKeyIdentifier
		},
	 	X509
	}
};

pub fn create_certificate() -> Result<(X509, PKey<Private>), ErrorStack> {
	let rsa = Rsa::generate(2048)?;
	let key_pair = PKey::from_rsa(rsa)?;

	let mut cert_builder = X509::builder()?;
	cert_builder.set_version(2)?;
	let serial_number = {
		let mut serial = BigNum::new()?;
		serial.rand(159, MsbOption::MAYBE_ZERO, false)?;
		serial.to_asn1_integer()?
	};
	cert_builder.set_serial_number(&serial_number)?;
	cert_builder.set_pubkey(&key_pair)?;
	let not_before = Asn1Time::days_from_now(0)?;
	cert_builder.set_not_before(&not_before)?;
	let not_after = Asn1Time::days_from_now(3650)?;
	cert_builder.set_not_after(&not_after)?;

	cert_builder.append_extension(BasicConstraints::new().critical().ca().build()?)?;
	cert_builder.append_extension(
		KeyUsage::new()
			.critical()
			.key_cert_sign()
			.crl_sign()
			.build()?,
	)?;

	let subject_key_identifier =
		SubjectKeyIdentifier::new().build(&cert_builder.x509v3_context(None, None))?;
	cert_builder.append_extension(subject_key_identifier)?;

	cert_builder.sign(&key_pair, MessageDigest::sha256())?;
	let cert = cert_builder.build();

	Ok((cert, key_pair))
}

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
