use aes::cipher::{BlockEncryptMut, KeyIvInit};
use aes_gcm::{
	aead::{Aead, KeyInit},
	Aes128Gcm, Key, Nonce,
};
use rand::RngCore;
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose, SerialNumber};
use rsa::{
    pkcs8::{EncodePrivateKey, LineEnding},
    RsaPrivateKey,
};
use std::time::{Duration, SystemTime};

pub fn create_certificate() -> Result<(String, String), Box<dyn std::error::Error>> {
    // Generate RSA key (2048 bits)
    let mut rng = rand::rngs::OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 2048)?;
    let key_pem = private_key.to_pkcs8_pem(LineEnding::LF)?.to_string();

	let mut params = CertificateParams::default();
	params.not_before = SystemTime::now().into();
	params.not_after = (SystemTime::now() + Duration::from_secs(3650 * 24 * 60 * 60)).into();
	params.serial_number = Some(SerialNumber::from(rng.next_u64())); 

	let mut distinguished_name = DistinguishedName::new();
	distinguished_name.push(DnType::CommonName, "Moonshine");
	params.distinguished_name = distinguished_name;

	params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
	params.key_usages = vec![
		KeyUsagePurpose::DigitalSignature,
		KeyUsagePurpose::KeyEncipherment,
		KeyUsagePurpose::KeyAgreement,
	];

	let key_pair = KeyPair::from_pem(&key_pem)?;
	let cert = params.self_signed(&key_pair)?;

	Ok((cert.pem(), key_pem))
}

pub fn encrypt(
	plaintext: &[u8],
	key: &[u8],
	iv: &[u8],
	tag: &mut [u8],
) -> Result<Vec<u8>, aes_gcm::Error> {
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

pub fn decrypt(ciphertext: &[u8], key: &[u8], iv: &[u8], tag: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
	let key = Key::<Aes128Gcm>::from_slice(key);
	let nonce = Nonce::from_slice(iv);
	let cipher = Aes128Gcm::new(key);

	// Append tag to ciphertext for aes-gcm crate
	let mut payload = Vec::with_capacity(ciphertext.len() + tag.len());
	payload.extend_from_slice(ciphertext);
	payload.extend_from_slice(tag);

	cipher.decrypt(nonce, payload.as_ref())
}

pub fn encrypt_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
    let key = match key.try_into() {
        Ok(k) => k,
        Err(_) => return Err("Invalid key length".to_string()),
    };
    let iv = match iv.try_into() {
        Ok(iv) => iv,
        Err(_) => return Err("Invalid IV length".to_string()),
    };
    
    let cipher = cbc::Encryptor::<aes::Aes128>::new(key, iv);
    
    let mut buffer = vec![0u8; data.len() + 16];
    let pos = data.len();
    buffer[..pos].copy_from_slice(data);
    
    let ct_len = cipher.encrypt_padded_mut::<block_padding::Pkcs7>(&mut buffer, pos)
        .map_err(|e| format!("Padding error: {:?}", e))?
        .len();
    
    buffer.truncate(ct_len);
    Ok(buffer)
}
