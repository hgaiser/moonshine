use std::{collections::HashMap, sync::Arc};

use hyper::{Response, Body, header};
use tokio::sync::{Notify, mpsc, oneshot};
use xml::{EmitterConfig, writer::XmlEvent};

use crate::{session::clients::{PendingClient, ClientManagerCommand, StartPairingCommand, ClientChallengeCommand, ServerChallengeResponseCommand, AddClientCommand, CheckClientPairingSecretCommand}, webserver::bad_request};

use super::WebserverError;

/// Handle a pairing request from a client.
///
/// This request consists of multiple steps, all are handled by this function.
/// The pairing process follows these steps:
///
///   1. /pair?phrase=getservercert&clientcert=...&salt=...&uniqueid=...
///      Retrieve the server certificate and provide the server with the client certificate and salt.
///   2. /pair?clientchallenge=...
///      Challenge the server with a test (?).
///   3. /pair?serverchallengeresp=...
///   4. /pair?phrase=pairchallenge
///   5. /pair?clientpairingsecret=...
///
/// After completing these steps, we have paired with the client.
pub async fn handle_pair_request(
	mut params: HashMap<String, String>,
	server_pem: openssl::x509::X509,
	client_command_tx: mpsc::Sender<ClientManagerCommand>
) -> Result<Response<Body>, WebserverError> {
	if params.contains_key("phrase") {
		match params.remove("phrase").unwrap().as_str() {
			"getservercert" => get_server_cert(params, server_pem, client_command_tx).await,
			"pairchallenge" => pair_challenge(params, client_command_tx).await,
			unknown => {
				log::error!("Unknown pair phrase received: {}", unknown);
				bad_request()
			}
		}
	} else if params.contains_key("clientchallenge") {
		client_challenge(params, client_command_tx).await
	} else if params.contains_key("serverchallengeresp") {
		server_challenge_response(params, client_command_tx).await
	} else if params.contains_key("clientpairingsecret") {
		client_pairing_secret(params, client_command_tx).await
	} else {
		log::warn!("Unknown pair command with params: {:?}", params);
		bad_request()
	}
}

async fn get_server_cert(
	mut params: HashMap<String, String>,
	server_pem: openssl::x509::X509,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let client_cert = match params.remove("clientcert") {
		Some(client_cert) => hex::decode(client_cert).map_err(|e| WebserverError::Other(e.to_string()))?,
		None => {
			log::error!("Expected 'clientcert' in get server cert request, got {:?}.", params.keys());
			return bad_request();
		}
	};
	let unique_id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in get server cert request, got {:?}.", params.keys());
			return bad_request();
		}
	};
	let salt = match params.remove("salt") {
		Some(salt) => hex::decode(salt).map_err(|e| WebserverError::Other(e.to_string()))?,
		None => {
			log::error!("Expected 'salt' in get server cert request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let pem = openssl::x509::X509::from_pem(client_cert.as_slice())
		.map_err(|e| WebserverError::Other(e.to_string()))?;

	let pin_notifier = {
		let pending_client = PendingClient {
			id: unique_id.clone(),
			pem,
			salt: salt.clone().try_into()
				.map_err(|e| WebserverError::Other(format!("failed to parse salt value, expected exactly 16 values but got {e:?}")))?,
			pin_notify: Arc::new(Notify::new()),
			key: None,
			server_secret: None,
			server_challenge: None,
			client_hash: None,
		};
		let notify = pending_client.pin_notify.clone();

		client_command_tx.send(ClientManagerCommand::StartPairing(StartPairingCommand { pending_client }))
			.await
			.map_err(|e| WebserverError::Other(format!("failed to send pairing command to client manager: {e}")))?;

		notify
	};

	log::info!("Waiting for pin to be sent at /pin?uniqueid={}&pin=<PIN>", &unique_id);
	pin_notifier.notified().await;

	let mut buffer = Vec::new();
	let mut writer = EmitterConfig::new()
		.write_document_declaration(true)
		.perform_indent(true)
		.create_writer(&mut buffer);

	writer.write(XmlEvent::start_element("root")
		.attr("status_code", "200")).unwrap();

	writer.write(XmlEvent::start_element("paired")).unwrap();
	writer.write(XmlEvent::characters("1")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	let serialized_server_pem = server_pem.to_pem()
		.map_err(|e| WebserverError::Other(e.to_string()))?;
	writer.write(XmlEvent::start_element("plaincert")).unwrap();
	writer.write(XmlEvent::characters(&hex::encode(serialized_server_pem))).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer)
		.map_err(|e| WebserverError::Other(e.to_string()))?));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

	Ok(response)
}

async fn client_challenge(
	mut params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let unique_id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in get server cert request, got {:?}.", params.keys());
			return bad_request();
		}
	};
	let challenge = match params.remove("clientchallenge") {
		Some(challenge) => hex::decode(challenge).map_err(|e| WebserverError::Other(e.to_string()))?,
		None => {
			log::error!("Expected 'clientchallenge' in get server cert request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let (response_tx, response_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::ClientChallenge(ClientChallengeCommand {
		id: unique_id,
		challenge,
		response: response_tx,
	}))
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to send client challenge to client manager: {e}")))?;

	let challenge_response = match response_rx.await.map_err(|e| WebserverError::Other(format!("Failed to receive client challenge response from client manager: {e}")))? {
		Ok(response) => response,
		Err(e) => return Err(WebserverError::Other(e)),
	};

	let mut buffer = Vec::new();
	let mut writer = EmitterConfig::new()
		.write_document_declaration(true)
		.perform_indent(true)
		.create_writer(&mut buffer);

	writer.write(XmlEvent::start_element("root")
		.attr("status_code", "200")).unwrap();

	writer.write(XmlEvent::start_element("paired")).unwrap();
	writer.write(XmlEvent::characters("1")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	writer.write(XmlEvent::start_element("challengeresponse")).unwrap();
	writer.write(XmlEvent::characters(&hex::encode(challenge_response))).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer)
		.map_err(|e| WebserverError::Other(e.to_string()))?));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

	Ok(response)
}

async fn server_challenge_response(
	mut params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let server_challenge_response = match params.remove("serverchallengeresp") {
		Some(server_challenge_response) => hex::decode(server_challenge_response).map_err(|e| WebserverError::Other(e.to_string()))?,
		None => {
			return Err(WebserverError::Other(format!("Expected 'serverchallengeresp' in server challenge response request, got {:?}.", params.keys())));
		}
	};
	let unique_id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			return Err(WebserverError::Other(format!("Expected 'uniqueid' in server challenge response request, got {:?}.", params.keys())));
		}
	};

	let (response_tx, response_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::ServerChallengeResponse(ServerChallengeResponseCommand {
		id: unique_id,
		challenge_response: server_challenge_response,
		response: response_tx,
	}))
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to send server challenge response to client manager: {e}")))?;

	let pairing_secret = match response_rx.await.map_err(|e| WebserverError::Other(format!("Failed to receive server challenge response from client manager: {e}")))? {
		Ok(response) => response,
		Err(e) => return Err(WebserverError::Other(e)),
	};

	let mut buffer = Vec::new();
	let mut writer = EmitterConfig::new()
		.write_document_declaration(true)
		.perform_indent(true)
		.create_writer(&mut buffer);

	writer.write(XmlEvent::start_element("root")
		.attr("status_code", "200")).unwrap();

	writer.write(XmlEvent::start_element("paired")).unwrap();
	writer.write(XmlEvent::characters("1")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	writer.write(XmlEvent::start_element("pairingsecret")).unwrap();
	writer.write(XmlEvent::characters(&hex::encode(pairing_secret))).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer)
		.map_err(|e| WebserverError::Other(e.to_string()))?));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

	Ok(response)
}

async fn pair_challenge(
	mut params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let unique_id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in pair challenge, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let (response_tx, response_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::AddClient(AddClientCommand {
		id: unique_id,
		response: response_tx,
	}))
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to add client to client manager: {e}")))?;

	match response_rx.await.map_err(|e| WebserverError::Other(format!("Failed to receive add client response from client manager: {e}")))? {
		Ok(response) => response,
		Err(e) => return Err(WebserverError::Other(e)),
	};

	let mut buffer = Vec::new();
	let mut writer = EmitterConfig::new()
		.write_document_declaration(true)
		.perform_indent(true)
		.create_writer(&mut buffer);

	writer.write(XmlEvent::start_element("root")
		.attr("status_code", "200")).unwrap();

	writer.write(XmlEvent::start_element("paired")).unwrap();
	writer.write(XmlEvent::characters("1")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer)
		.map_err(|e| WebserverError::Other(e.to_string()))?));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

	Ok(response)
}

async fn client_pairing_secret(
	mut params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let client_pairing_secret = match params.remove("clientpairingsecret") {
		Some(client_pairing_secret) => hex::decode(client_pairing_secret).map_err(|e| WebserverError::Other(e.to_string()))?,
		None => {
			log::error!("Expected 'clientpairingsecret' in client pairing secret request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let unique_id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in client pairing secret request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let (response_tx, response_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::CheckClientPairingSecret(CheckClientPairingSecretCommand {
		id: unique_id,
		client_secret: client_pairing_secret,
		response: response_tx,
	}))
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to check client pairing secret with client manager: {e}")))?;

	match response_rx.await.map_err(|e| WebserverError::Other(format!("Failed to receive client pairing secret response from client manager: {e}")))? {
		Ok(response) => response,
		Err(e) => return Err(WebserverError::Other(e)),
	};

	// TODO: Verify x509 cert.

	let mut buffer = Vec::new();
	let mut writer = EmitterConfig::new()
		.write_document_declaration(true)
		.perform_indent(true)
		.create_writer(&mut buffer);

	writer.write(XmlEvent::start_element("root")
		.attr("status_code", "200")).unwrap();

	writer.write(XmlEvent::start_element("paired")).unwrap();
	writer.write(XmlEvent::characters("1")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer)
		.map_err(|e| WebserverError::Other(e.to_string()))?));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

	Ok(response)
}
