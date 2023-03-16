use std::collections::BTreeMap;
use std::sync::Arc;

use hyper::Body;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use openssl::cipher::Cipher;
use openssl::cipher_ctx::CipherCtx;
use openssl::hash::MessageDigest;
use openssl::md::Md;
use openssl::md_ctx::MdCtx;
use openssl::pkey::PKey;
use openssl::pkey::PKeyRef;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use super::Params;
use super::parse_params;
use super::bad_request;

pub(super) struct PairingClient {
	_id: String,
	pem: openssl::x509::X509,
	salt: [u8; 16],
	notify_pin_received: Arc<Notify>,
	key: Option<[u8; 16]>,
	server_secret: Option<[u8; 16]>,
	server_challenge: Option<[u8; 16]>,
	client_hash: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Client {}

#[derive(Clone, Default)]
pub(super) struct Clients {
	pairing_clients: Arc<Mutex<BTreeMap<String, PairingClient>>>,
	clients: Arc<Mutex<BTreeMap<String, Client>>>,
}

impl Clients {
	pub(super) fn from_state_or_default() -> Self {
		let mut path = match dirs::state_dir() {
			Some(path) => path,
			None => {
				log::warn!("Failed to get user state directory.");
				return Self::default();
			}
		};
		path.push("moonshine");
		path.push("clients.toml");

		if path.exists() {
			let serialized = match std::fs::read_to_string(&path) {
				Ok(serialized) => serialized,
				Err(e) => {
					log::warn!("Failed to read clients state file: {}", e);
					return Self::default();
				}
			};
			let clients: BTreeMap<String, Client> = match toml::from_str(&serialized) {
				Ok(clients) => clients,
				Err(e) => {
					log::warn!("Failed to deserialize clients state: {}", e);
					return Self::default();
				}
			};

			log::debug!("Successfully loaded clients state from {:?}", path);
			log::trace!("Clients: {clients:?}");

			return Self {
				pairing_clients: Default::default(),
				clients: Arc::new(Mutex::new(clients)),
			};
		}

		log::debug!("No clients state found, starting with an empty state.");
		Self::default()
	}

	pub(super) async fn has_pairing_client(&self, key: &str) -> bool {
		self.pairing_clients.lock().await.contains_key(key)
	}

	pub(super) async fn has_client(&self, key: &str) -> bool {
		self.clients.lock().await.contains_key(key)
	}

	async fn add_client(&self, key: &str) {
		self.clients.lock().await.insert(key.to_string(), Client {});
		self.pairing_clients.lock().await.remove(key);
		self.save_state().await;
	}

	async fn remove_client(&self, key: &str) -> bool {
		if self.clients.lock().await.remove(key).is_none() {
			return false;
		}
		self.pairing_clients.lock().await.remove(key);
		self.save_state().await;

		true
	}

	async fn save_state(&self) {
		let mut path = match dirs::state_dir() {
			Some(path) => path,
			None => {
				log::warn!("Failed to get user state directory.");
				return;
			}
		};

		path.push("moonshine");
		if let Err(e) = std::fs::create_dir_all(&path) {
			log::warn!("Failed to save clients state file: {}", e);
			return;
		}

		path.push("clients.toml");

		let clients = self.clients.lock().await;
		let serialized = match toml::to_string_pretty(&*clients) {
			Ok(serialized) => serialized,
			Err(e) => {
				log::warn!("Failed to serialize clients: {}", e);
				return;
			}
		};

		if let Err(e) = std::fs::write(&path, serialized) {
			log::warn!("Failed to save serialized clients: {}", e);
		}

		log::debug!("Saved clients state to {:?}", path);
	}

	pub(super) async fn unpair(&self, req: Request<Body>) -> Response<Body> {
		let params = parse_params(req.uri());

		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in unpair request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		if self.remove_client(unique_id).await {
			log::info!("Successfully unpaired client '{}'", unique_id);
			Response::builder()
				.status(StatusCode::OK)
				.body(Body::from("Successfully unpaired.".to_string()))
				.unwrap()
		} else {
			log::error!("Failed to unpair: unknown unique id '{}'.", unique_id);
			bad_request()
		}
	}

	pub(super) async fn pin(&self, req: Request<Body>) -> Response<Body> {
		let params = parse_params(req.uri());

		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		let pin = match params.get("pin") {
			Some(pin) => pin,
			None => {
				log::error!("Expected 'pin' in pin request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		match self.pairing_clients.lock().await.get_mut(unique_id) {
			Some(mut client) => {
				client.key = Some(create_key(&client.salt, pin));
				client.notify_pin_received.notify_waiters();
				log::info!("Successfully notified of pin entry: {:?}", pin);
			},
			None => {
				log::error!("Unknown unique id '{}'.", unique_id);
				return bad_request();
			}
		};

		Response::builder()
			.status(StatusCode::OK)
			.body(Body::from(format!("Successfully received pin '{}' for unique id '{}'.", pin, unique_id)))
			.unwrap()
	}

	async fn get_server_cert(&self, params: Params) -> Response<Body> {
		let client_cert = match params.get("clientcert") {
			Some(client_cert) => hex::decode(client_cert).unwrap(),
			None => {
				log::error!("Expected 'clientcert' in get server cert request, got {:?}.", params.keys());
				return bad_request();
			}
		};
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in get server cert request, got {:?}.", params.keys());
				return bad_request();
			}
		};
		let salt = match params.get("salt") {
			Some(salt) => hex::decode(salt).unwrap(),
			None => {
				log::error!("Expected 'salt' in get server cert request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		let pem = openssl::x509::X509::from_pem(client_cert.as_slice()).unwrap();
		let server_pem = openssl::x509::X509::from_pem(&std::fs::read("./cert/cert.pem").unwrap()).unwrap();

		let notify_pin = {
			let pairing_client = PairingClient {
				_id: unique_id.to_owned(),
				pem,
				salt: salt.clone().try_into().unwrap(),
				notify_pin_received: Arc::new(Notify::new()),
				key: None,
				server_secret: None,
				server_challenge: None,
				client_hash: None,
			};
			let notify = pairing_client.notify_pin_received.clone();

			let mut pairing_clients = self.pairing_clients.lock().await;
			pairing_clients.insert(unique_id.to_owned(), pairing_client);

			notify
		};

		log::info!("Waiting for pin to be sent at /pin?uniqueid={}&pin=<PIN>", unique_id);
		notify_pin.notified().await;

		let response = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>
	<root status_code=\"200\">
		<paired>1</paired>
		<plaincert>{}</plaincert>
	</root>", hex::encode(server_pem.to_pem().unwrap()));
		Response::builder()
			.header("CONTENT_TYPE", "application/xml")
			.body(Body::from(response))
			.unwrap()
	}

	async fn client_challenge(&self, params: Params) -> Response<Body> {
		let client_challenge = match params.get("clientchallenge") {
			Some(client_challenge) => hex::decode(client_challenge).unwrap(),
			None => {
				log::error!("Expected 'clientchallenge' in client challenge request, got {:?}.", params.keys());
				return bad_request();
			}
		};
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in client challenge request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		let mut pairing_clients = self.pairing_clients.lock().await;
		let client = match pairing_clients.get_mut(unique_id) {
			Some(client) => client,
			None => {
				log::error!("Unknown unique id '{}' provided in client challenge.", unique_id);
				return bad_request();
			}
		};

		let key = match &client.key {
			Some(key) => key,
			None => {
				log::error!("Client has not provided a pin yet.");
				return bad_request();
			}
		};

		let mut server_secret = [0u8; 16];
		openssl::rand::rand_bytes(&mut server_secret).unwrap();
		client.server_secret = Some(server_secret);

		let server_pem = openssl::x509::X509::from_pem(&std::fs::read("./cert/cert.pem").unwrap()).unwrap();
		let mut decrypted = decrypt(&client_challenge, key);
		decrypted.extend_from_slice(server_pem.signature().as_slice());
		decrypted.extend_from_slice(&server_secret);

		let mut server_challenge = [0u8; 16];
		openssl::rand::rand_bytes(&mut server_challenge).unwrap();
		client.server_challenge = Some(server_challenge);

		let mut challenge_response = openssl::hash::hash(MessageDigest::sha256(), decrypted.as_slice()).unwrap().to_vec();
		challenge_response.extend(server_challenge);

		let challenge_response = encrypt(&challenge_response, key);
		let challenge_response = hex::encode(challenge_response);

		let response = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>
	<root status_code=\"200\">
		<paired>1</paired>
		<challengeresponse>{}</challengeresponse>
	</root>", challenge_response);

		Response::builder()
			.header("CONTENT_TYPE", "application/xml")
			.body(Body::from(response))
			.unwrap()
	}

	async fn server_challenge_response(&self, params: Params) -> Response<Body> {
		let server_challenge_response = match params.get("serverchallengeresp") {
			Some(server_challenge_response) => hex::decode(server_challenge_response).unwrap(),
			None => {
				log::error!("Expected 'serverchallengeresp' in server challenge response request, got {:?}.", params.keys());
				return bad_request();
			}
		};
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in server challenge response request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		let mut pairing_clients = self.pairing_clients.lock().await;
		let client = match pairing_clients.get_mut(unique_id) {
			Some(client) => client,
			None => {
				log::error!("Unknown unique id '{}' provided in server challenge response.", unique_id);
				return bad_request();
			}
		};

		let key = match &client.key {
			Some(key) => key,
			None => {
				log::warn!("Client has not provided a pin yet.");
				return bad_request();
			}
		};

		let decrypted = decrypt(&server_challenge_response, key);
		client.client_hash = Some(decrypted);

		let pkey = PKey::private_key_from_pem(&std::fs::read("./cert/key.pem").unwrap()).unwrap();

		let mut pairing_secret = client.server_secret.unwrap().to_vec();
		pairing_secret.extend(sign(&client.server_secret.unwrap(), pkey.as_ref()));
		let pairing_secret = hex::encode(pairing_secret);

		let response = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>
	<root status_code=\"200\">
		<paired>1</paired>
		<pairingsecret>{}</pairingsecret>
	</root>", pairing_secret);

		Response::builder()
			.header("CONTENT_TYPE", "application/xml")
			.body(Body::from(response))
			.unwrap()
	}

	async fn client_pairing_secret(&self, params: Params) -> Response<Body> {
		let client_pairing_secret = match params.get("clientpairingsecret") {
			Some(client_pairing_secret) => hex::decode(client_pairing_secret).unwrap(),
			None => {
				log::error!("Expected 'clientpairingsecret' in client pairing secret request, got {:?}.", params.keys());
				return bad_request();
			}
		};
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in client pairing secret request, got {:?}.", params.keys());
				return bad_request();
			}
		};

		let mut pairing_clients = self.pairing_clients.lock().await;

		let pairing_client = match pairing_clients.get_mut(unique_id) {
			Some(pairing_client) => pairing_client,
			None => {
				log::error!("Unknown unique id '{}' provided in client pairing secret.", unique_id);
				return bad_request();
			}
		};

		if client_pairing_secret.len() != 256 + 16 {
			panic!("Expected client pairing secret to be of size {}, but got {} bytes.", 256 + 16, client_pairing_secret.len());
		}

		let client_secret = &client_pairing_secret[..16];
		// let signed_client_secret = &client_pairing_secret[16..];

		let mut data = pairing_client.server_challenge.unwrap().to_vec();
		data.extend(pairing_client.pem.signature().as_slice());
		data.extend(client_secret);

		if !openssl::hash::hash(MessageDigest::sha256(), &data).unwrap().to_vec().eq(pairing_client.client_hash.as_ref().unwrap()) {
			log::error!("Client hash is not as expected, MITM?");
			return bad_request();
		}

		// TODO: Verify x509 cert.

		let response = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
	<root status_code=\"200\">
		<paired>1</paired>
	</root>";

		Response::builder()
			.header("CONTENT_TYPE", "application/xml")
			.body(Body::from(response))
			.unwrap()
	}

	async fn pair_challenge(&self, params: Params) -> Response<Body> {
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				log::error!("Expected 'uniqueid' in pair challenge, got {:?}.", params.keys());
				return bad_request();
			}
		};

		if !self.has_pairing_client(unique_id).await {
			log::error!("Unknown unique id '{}' provided in pair challenge.", unique_id);
			return bad_request();
		}
		self.add_client(unique_id).await;

		let response = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
	<root status_code=\"200\">
		<paired>1</paired>
	</root>";

		Response::builder()
			.header("CONTENT_TYPE", "application/xml")
			.body(Body::from(response))
			.unwrap()
	}

	pub(super) async fn pair(&self, req: Request<Body>) -> Response<Body> {
		let params = parse_params(req.uri());

		if params.contains_key("phrase") {
			match params.get("phrase").unwrap().as_str() {
				"getservercert" => self.get_server_cert(params).await,
				"pairchallenge" => self.pair_challenge(params).await,
				unknown => {
					log::error!("Unknown pair phrase received: {}", unknown);
					Response::builder()
						.status(400)
						.body(Body::from("INVALID REQUEST"))
						.unwrap()
				}
			}
		} else if params.contains_key("clientchallenge") {
			self.client_challenge(params).await
		} else if params.contains_key("serverchallengeresp") {
			self.server_challenge_response(params).await
		} else if params.contains_key("clientpairingsecret") {
			self.client_pairing_secret(params).await
		} else {
			log::error!("Unknown pair command with params: {:?}", params);
			bad_request()
		}
	}

}

fn create_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
	let mut key = Vec::with_capacity(salt.len() + pin.len());
	key.extend(salt);
	key.extend(pin.as_bytes());
	openssl::hash::hash(MessageDigest::sha256(), &key).unwrap().to_vec()[..16].try_into().unwrap()
}

fn encrypt(plaintext: &[u8], key: &[u8]) -> Vec<u8> {
	let cipher = Cipher::aes_128_ecb();

	let mut context = CipherCtx::new().unwrap();
	context.encrypt_init(Some(cipher), Some(key), None).unwrap();
	context.set_padding(false);

	let mut ciphertext = Vec::with_capacity(plaintext.len());
	context.cipher_update_vec(plaintext, &mut ciphertext).unwrap();
	context.cipher_final_vec(&mut ciphertext).unwrap();

	if plaintext.len() != ciphertext.len() {
		panic!("Cipher and plaintext should be the same length, but are {} vs {}.", plaintext.len(), ciphertext.len());
	}

	ciphertext
}

fn decrypt(ciphertext: &[u8], key: &[u8]) -> Vec<u8> {
	let cipher = Cipher::aes_128_ecb();

	let mut context = CipherCtx::new().unwrap();
	context.decrypt_init(Some(cipher), Some(key), None).unwrap();
	context.set_padding(false);

	let mut plaintext = Vec::with_capacity(ciphertext.len());
	context.cipher_update_vec(ciphertext, &mut plaintext).unwrap();
	context.cipher_final_vec(&mut plaintext).unwrap();

	if plaintext.len() != ciphertext.len() {
		panic!("Cipher and plaintext should be the same length, but are {} vs {}.", plaintext.len(), ciphertext.len());
	}

	plaintext
}

fn sign<T>(data: &[u8], key: &PKeyRef<T>) -> Vec<u8>
	where T: openssl::pkey::HasPrivate
{
	// Create the signature.
	let mut context = MdCtx::new().unwrap();
	context.digest_sign_init(Some(Md::sha256()), key).unwrap();
	context.digest_sign_update(data).unwrap();

	// let mut signature = [0u8; 256];
	let mut signature = Vec::new();
	context.digest_sign_final_to_vec(&mut signature).unwrap();

	signature
}
