use std::{sync::Arc, collections::BTreeMap};

use async_shutdown::TriggerShutdownToken;
use openssl::{hash::MessageDigest, pkey::{PKey, PKeyRef, Private}, md::Md, md_ctx::MdCtx, x509::X509, cipher::Cipher};
use tokio::sync::{oneshot, mpsc, Notify};

use crate::{crypto::{encrypt, decrypt}, state::State};

/// A client that is not yet paired, but in the pairing process.
pub struct PendingClient {
	/// Unique id of the client.
	pub id: String,

	/// Client certificate used for secure communication.
	pub pem: X509,

	/// Salt provided by the client to use for encryption.
	pub salt: [u8; 16],

	/// A channel that sends a notification when a PIN has been received for this client.
	///
	/// The client shows a PIN code on the clients screen.
	/// The user is expected to provide this PIN to the server.
	/// This channel notifies listeners that the user has provided a PIN code.
	pub pin_notify: Arc<Notify>,

	/// Cryptographic key.
	pub key: Option<[u8; 16]>,

	/// Server secret as generated by the server.
	pub server_secret: Option<[u8; 16]>,

	/// Server challenge provided by the client.
	pub server_challenge: Option<[u8; 16]>,

	/// Cryptographic hash.
	pub client_hash: Option<Vec<u8>>,
}

pub enum ClientManagerCommand {
	/// Check if a client is already paired.
	IsPaired(IsPairedCommand),

	/// Initiate the pairing procedure.
	StartPairing(StartPairingCommand),

	/// Register a pin for a client.
	RegisterPin(RegisterPinCommand),

	/// Run a challenge for the client.
	ClientChallenge(ClientChallengeCommand),

	/// Process the response from the challenge.
	ServerChallengeResponse(ServerChallengeResponseCommand),

	/// Check to make sure the client pairing secret is as expected.
	CheckClientPairingSecret(CheckClientPairingSecretCommand),

	/// Add a client to the list of paired clients.
	AddClient(AddClientCommand),

	// /// Remove client from the list of paired clients.
	// RemoveClient(RemoveClientCommand),
}

/// Query the manager to check if this unique id is paired or not.
pub struct IsPairedCommand {
	/// Unique id of the client.
	pub id: String,

	/// Channel used to provide a response.
	pub response: oneshot::Sender<Result<bool, String>>,
}

/// Initiate a pairing process for a client.
pub struct StartPairingCommand {
	/// Client to start the pairing process for.
	pub pending_client: PendingClient,
}

/// Register a pin for a client.
pub struct RegisterPinCommand {
	/// Id of the client.
	pub id: String,

	/// The pin for the client.
	pub pin: String,

	/// Channel used to provide a response.
	pub response: oneshot::Sender<Result<(), String>>,
}

/// Run a challenge for the client.
pub struct ClientChallengeCommand {
	/// Id of the client.
	pub id: String,

	/// Challenge from the client.
	pub challenge: Vec<u8>,

	/// Channel used to provide a response.
	pub response: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Process the response from the client challenge.
pub struct ServerChallengeResponseCommand {
	/// Id of the client.
	pub id: String,

	/// Challenge response from the client.
	pub challenge_response: Vec<u8>,

	/// Channel used to provide a response.
	pub response: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Check the secret of the client.
pub struct CheckClientPairingSecretCommand {
	/// Id of the client.
	pub id: String,

	/// Challenge response from the client.
	pub client_secret: Vec<u8>,

	/// Channel used to provide a response.
	pub response: oneshot::Sender<Result<(), String>>,
}

/// Add a client to the list of paired clients.
pub struct AddClientCommand {
	/// Id of the client.
	pub id: String,

	/// Channel used to provide a response.
	pub response: oneshot::Sender<Result<(), String>>,
}

// /// Remove client from the list of paired clients.
// pub struct RemoveClientCommand {
// 	/// Id of the client.
// 	pub id: String,

// 	/// Channel used to provide a response.
// 	pub response: oneshot::Sender<Result<(), String>>,
// }

#[derive(Clone)]
pub struct ClientManager {
	command_tx: mpsc::Sender<ClientManagerCommand>,
}

impl ClientManager {
	pub fn new(
		state: State,
		server_certs: X509,
		server_pkey: PKey<Private>,
		shutdown_token: TriggerShutdownToken<i32>,
	) -> Self {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = ClientManagerInner { server_certs, server_pkey };
		tokio::spawn(async move { inner.run(command_rx, state).await; drop(shutdown_token); });

		Self { command_tx }
	}

	pub async fn is_paired(&self, id: String) -> Result<bool, ()> {
		let (response_tx, response_rx) = oneshot::channel();
		self.command_tx.send(ClientManagerCommand::IsPaired(IsPairedCommand { id, response: response_tx }))
			.await
			.map_err(|e| tracing::error!("Failed to check paired status: {e}"))?;

		response_rx.await
			.map_err(|e| tracing::error!("Failed to receive IsPaired response: {e}"))?
			.map_err(|e| tracing::error!("Failed to check paired status: {e}"))
	}

	pub async fn start_pairing(&self, pending_client: PendingClient) -> Result<(), ()> {
		self.command_tx.send(ClientManagerCommand::StartPairing(StartPairingCommand { pending_client }))
			.await
			.map_err(|e| tracing::error!("Failed to start pairing: {e}"))
	}

	pub async fn register_pin(&self, id: &str, pin: &str) -> Result<(), ()> {
		let (response_tx, response_rx) = oneshot::channel();
		self.command_tx.send(ClientManagerCommand::RegisterPin(RegisterPinCommand {
			id: id.to_string(),
			pin: pin.to_string(),
			response: response_tx,
		}))
			.await
			.map_err(|e| tracing::error!("Failed to send pin to client manager: {e}"))?;

		response_rx
			.await
			.map_err(|e| tracing::error!("Failed to wait for response to RegisterPin command from client manager: {e}"))?
			.map_err(|e| tracing::warn!("{e}"))
	}

	pub async fn add_client(&self, id: &str) -> Result<(), ()> {
		let (response_tx, response_rx) = oneshot::channel();
		self.command_tx.send(ClientManagerCommand::AddClient(AddClientCommand {
			id: id.to_string(),
			response: response_tx,
		}))
			.await
			.map_err(|e| tracing::error!("Failed to send AddClient command to client manager: {e}"))?;

		response_rx
			.await
			.map_err(|e| tracing::error!("Failed to wait for response to AddClient command from client manager: {e}"))?
			.map_err(|e| tracing::warn!("{e}"))
	}

	pub async fn client_challenge(&self, id: &str, challenge: Vec<u8>) -> Result<Vec<u8>, ()> {
		let (response_tx, response_rx) = oneshot::channel();
		self.command_tx.send(ClientManagerCommand::ClientChallenge(ClientChallengeCommand {
			id: id.to_string(),
			challenge,
			response: response_tx,
		}))
			.await
			.map_err(|e| tracing::error!("Failed to send client challenge to client manager: {e}"))?;

		response_rx
			.await
			.map_err(|e| tracing::error!("Failed to wait for response to client challenge command from client manager: {e}"))?
			.map_err(|e| tracing::warn!("{e}"))
	}

	pub async fn server_challenge_response(&self, id: &str, challenge_response: Vec<u8>) -> Result<Vec<u8>, ()> {
		let (response_tx, response_rx) = oneshot::channel();
		self.command_tx.send(ClientManagerCommand::ServerChallengeResponse(ServerChallengeResponseCommand {
			id: id.to_string(),
			challenge_response,
			response: response_tx,
		}))
			.await
			.map_err(|e| tracing::error!("Failed to send server challenge response to client manager: {e}"))?;

		response_rx
			.await
			.map_err(|e| tracing::error!("Failed to wait for response to server challenge response command from client manager: {e}"))?
			.map_err(|e| tracing::warn!("{e}"))
	}

	pub async fn check_client_pairing_secret(&self, id: &str, client_secret: Vec<u8>) -> Result<(), ()> {
		let (response_tx, response_rx) = oneshot::channel();
		self.command_tx.send(ClientManagerCommand::CheckClientPairingSecret(CheckClientPairingSecretCommand {
			id: id.to_string(),
			client_secret,
			response: response_tx,
		}))
			.await
			.map_err(|e| tracing::error!("Failed to send check client pairing secret response to client manager: {e}"))?;

		response_rx
			.await
			.map_err(|e| tracing::error!("Failed to wait for response to check client pairing secret command from client manager: {e}"))?
			.map_err(|e| tracing::warn!("{e}"))
	}

	// pub async fn remove_client(&self, id: &str) -> Result<(), ()> {
	// 	let (response_tx, response_rx) = oneshot::channel();
	// 	self.command_tx.send(ClientManagerCommand::RemoveClient(RemoveClientCommand {
	// 		id: id.to_string(),
	// 		response: response_tx,
	// 	}))
	// 		.await
	// 		.map_err(|e| tracing::error!("Failed to send remove client command to client manager: {e}"))?;

	// 	response_rx
	// 		.await
	// 		.map_err(|e| tracing::error!("Failed to wait for response to remove client command from client manager: {e}"))?
	// 		.map_err(|e| tracing::warn!("{e}"))
	// }
}

struct ClientManagerInner {
	server_certs: X509,
	server_pkey: PKey<Private>,
}

impl ClientManagerInner {
	async fn run(self, mut command_rx: mpsc::Receiver<ClientManagerCommand>, state: State) {
		tracing::debug!("Waiting for commands.");

		let mut pending_clients = BTreeMap::new();
		while let Some(command) = command_rx.recv().await {
			match command {
				ClientManagerCommand::IsPaired(command) => {
					match state.has_client(command.id).await {
						Ok(result) => {
							command.response.send(Ok(result))
								.map_err(|_| tracing::error!("Failed to send IsPaired response.")).ok();
						},
						Err(()) => {
							command.response.send(Err("Failed to check client paired status.".to_string()))
								.map_err(|_| tracing::error!("Failed to send IsPaired response.")).ok();
						},
					}
				},

				ClientManagerCommand::StartPairing(command) => {
					pending_clients.insert(command.pending_client.id.clone(), command.pending_client);
				},

				ClientManagerCommand::RegisterPin(command) => {
					match pending_clients.get_mut(&command.id) {
						Some(client) => {
							let key = match create_key(&client.salt, &command.pin) {
								Ok(key) => key,
								Err(e) => {
									tracing::error!("Failed to create client key: {e}");
									command.response.send(Err(e))
										.map_err(|_| tracing::error!("Failed to send RegisterPin response.")).ok();
									continue;
								}
							};
							client.key = Some(key);
							client.pin_notify.notify_waiters();
							command.response.send(Ok(()))
								.map_err(|_| tracing::error!("Failed to send RegisterPin error.")).ok();
						},
						None => {
							command.response.send(Err(format!("No known client with id {}", command.id)))
								.map_err(|_| tracing::error!("Failed to send pin notify error.")).ok();
						},
					};
				},

				ClientManagerCommand::ClientChallenge(command) => {
					match pending_clients.get_mut(&command.id) {
						Some(client) => {
							match self.client_challenge(client, command.challenge).await {
								Ok(response) => {
									command.response.send(Ok(response))
										.map_err(|_| tracing::error!("Failed to send ClientChallenge response.")).ok();
								},
								Err(e) => {
									tracing::error!("Failed to respond to client challenge: {e}");
									command.response.send(Err(e))
										.map_err(|_| tracing::error!("Failed to send ClientChallenge error.")).ok();
									continue;
								},
							};
						},
						None => {
							command.response.send(Err(format!("No known client with id {}", command.id)))
								.map_err(|_| tracing::error!("Failed to send ClientChallenge response.")).ok();
						},
					};
				},

				ClientManagerCommand::ServerChallengeResponse(command) => {
					match pending_clients.get_mut(&command.id) {
						Some(client) => {
							match self.server_challenge_response(client, command.challenge_response).await {
								Ok(response) => {
									command.response.send(Ok(response))
										.map_err(|_| tracing::error!("Failed to send ServerChallengeResponse response.")).ok();
								},
								Err(e) => {
									tracing::error!("Failed to respond to server challenge: {e}");
									command.response.send(Err(e))
										.map_err(|_| tracing::error!("Failed to send ServerChallengeResponse error.")).ok();
									continue;
								},
							};
						},
						None => {
							command.response.send(Err(format!("No known client with id {}", command.id)))
								.map_err(|_| tracing::error!("Failed to send ServerChallengeResponse response.")).ok();
						},
					};
				},

				ClientManagerCommand::CheckClientPairingSecret(command) => {
					match pending_clients.get_mut(&command.id) {
						Some(client) => {
							match check_client_pairing_secret(client, command.client_secret).await {
								Ok(()) => {
									command.response.send(Ok(()))
										.map_err(|_| tracing::error!("Failed to send CheckClientPairingSecret response.")).ok();
								},
								Err(e) => {
									tracing::error!("Failed to check client pairing secret: {e}");
									command.response.send(Err(e))
										.map_err(|_| tracing::error!("Failed to send CheckClientPairingSecret error.")).ok();
									continue;
								},
							};
						},
						None => {
							command.response.send(Err(format!("No known client with id {}", command.id)))
								.map_err(|_| tracing::error!("Failed to send CheckClientPairingSecret response.")).ok();
						},
					};
				},

				ClientManagerCommand::AddClient(command) => {
					let Ok(has_client) = state.has_client(command.id.clone()).await else {
						command.response.send(Err("Failed to check client paired status.".to_string()))
							.map_err(|_| tracing::error!("Failed to send AddClient command response.")).ok();
						continue;
					};

					if has_client {
						command.response.send(Err("Client is already paired, can't add it again.".to_string()))
							.map_err(|_| tracing::error!("Failed to send AddClient command response.")).ok();
						continue;
					}

					if let Err(()) = state.add_client(command.id).await {
						command.response.send(Err("Failed to add client.".to_string()))
							.map_err(|_| tracing::error!("Failed to send AddClient command response.")).ok();
					} else {
						command.response.send(Ok(()))
							.map_err(|_| tracing::error!("Failed to send AddClient command response.")).ok();
					}
				},

				// ClientManagerCommand::RemoveClient(command) => {
				// 	pending_clients.remove(&command.id);
				// 	let Ok(result) = state.remove_client(command.id).await else {
				// 		command.response.send(Err("Failed to remove client.".to_string()))
				// 			.map_err(|_| tracing::error!("Failed to send RemoveClient command response.")).ok();
				// 		continue;
				// 	};

				// 	if !result {
				// 		command.response.send(Err("Client is not known, can't remove it.".to_string()))
				// 			.map_err(|_| tracing::error!("Failed to send remove client command response.")).ok();
				// 		continue;
				// 	}

				// 	command.response.send(Ok(()))
				// 		.map_err(|_| tracing::error!("Failed to send remove client command response.")).ok();
				// },
			}
		}

		tracing::debug!("Command channel closed.");
	}

	async fn client_challenge(&self, client: &mut PendingClient, challenge: Vec<u8>) -> Result<Vec<u8>, String> {
		let key = match &client.key {
			Some(key) => key,
			None => {
				return Err("Client has not provided a pin yet.".to_string());
			}
		};

		// Generate a random server secret.
		let mut server_secret = [0u8; 16];
		openssl::rand::rand_bytes(&mut server_secret)
			.map_err(|e| format!("Failed to create random server secret: {e}"))?;
		client.server_secret = Some(server_secret);

		let mut decrypted = decrypt(Cipher::aes_128_ecb(), &challenge, key)
			.map_err(|e| format!("Failed to decrypt client challenge: {e}"))?;
		decrypted.extend_from_slice(self.server_certs.signature().as_slice());
		decrypted.extend_from_slice(&server_secret);

		let mut server_challenge = [0u8; 16];
		openssl::rand::rand_bytes(&mut server_challenge)
			.map_err(|e| format!("Failed to create random server challenge: {e}"))?;
		client.server_challenge = Some(server_challenge);

		let mut challenge_response = openssl::hash::hash(MessageDigest::sha256(), decrypted.as_slice())
			.map_err(|e| format!("Failed to hash client challenge response: {e}"))?
			.to_vec();
		challenge_response.extend(server_challenge);

		let cipher = Cipher::aes_128_ecb();
		let challenge_response = encrypt(cipher, &challenge_response, Some(key), None, false)
			.map_err(|e| format!("Failed to encrypt client challenge response: {e}"))?;

		Ok(challenge_response)
	}

	async fn server_challenge_response(
		&self,
		client: &mut PendingClient,
		challenge_response: Vec<u8>,
	) -> Result<Vec<u8>, String> {
		let key = match &client.key {
			Some(key) => key,
			None => {
				return Err("Client has not provided a pin yet.".to_string());
			}
		};

		let decrypted = decrypt(Cipher::aes_128_ecb(), &challenge_response, key)
			.map_err(|e| format!("Failed to decrypt server challenge response: {e}"))?;
		client.client_hash = Some(decrypted);

		let server_secret = client.server_secret
			.ok_or("Client does not have a server secret.".to_string())?;

		let mut pairing_secret = server_secret.to_vec();
		let signed = sign(&server_secret, &self.server_pkey)
			.map_err(|e| format!("Failed to sign server secret: {e}"))?;
		pairing_secret.extend(signed);

		Ok(pairing_secret)
	}
}

fn create_key(salt: &[u8; 16], pin: &str) -> Result<[u8; 16], String> {
	let mut key = Vec::with_capacity(salt.len() + pin.len());
	key.extend(salt);
	key.extend(pin.as_bytes());
	openssl::hash::hash(MessageDigest::sha256(), &key)
		.map_err(|e| format!("Failed to hash key for client: {e}"))?
		.to_vec()[..16]
		.try_into()
		.map_err(|e| format!("Received unexpected key result: {e}"))
}

fn sign<T>(data: &[u8], key: &PKeyRef<T>) -> Result<Vec<u8>, openssl::error::ErrorStack>
	where T: openssl::pkey::HasPrivate
{
	// Create the signature.
	let mut context = MdCtx::new()?;
	context.digest_sign_init(Some(Md::sha256()), key)?;
	context.digest_sign_update(data)?;

	// let mut signature = [0u8; 256];
	let mut signature = Vec::new();
	context.digest_sign_final_to_vec(&mut signature)?;

	Ok(signature)
}

async fn check_client_pairing_secret(client: &mut PendingClient, client_secret: Vec<u8>) -> Result<(), String> {
	let client_hash = match &client.client_hash {
		Some(client_hash) => client_hash,
		None => {
			return Err("We did not yet receive a client hash.".to_string());
		}
	};

	if client_secret.len() != 256 + 16 {
		return Err(format!("Expected client pairing secret to be of size {}, but got {} bytes.", 256 + 16, client_secret.len()));
	}
	let server_challenge = match client.server_challenge {
		Some(server_challenge) => server_challenge,
		None => return Err("Client does not have a server challenge, possibly incorrect pairing procedure?".to_string()),
	};

	let client_secret = &client_secret[..16];
	// let signed_client_secret = &client_pairing_secret[16..];

	let mut data = server_challenge.to_vec();
	data.extend(client.pem.signature().as_slice());
	data.extend(client_secret);

	let data = match openssl::hash::hash(MessageDigest::sha256(), &data) {
		Ok(data) => data,
		Err(e) => {
			return Err(format!("Failed to hash secret: {e}"));
		}
	};

	if !data.to_vec().eq(client_hash) {
		return Err("Client hash is not as expected, MITM?".to_string());
	}

	Ok(())
}
