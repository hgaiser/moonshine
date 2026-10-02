use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use async_shutdown::ShutdownManager;
use http_body_util::Full;
use hyper::{
	Request, Response,
	body::Bytes,
	header::{self, HeaderValue},
};
use notify_rust::Notification;
use tokio::sync::Notify;

use crate::ShutdownReason;
use crate::clients::ClientManager;
use crate::clients::PendingClient;
use crate::clients::new_approval_token;
use crate::webserver::bad_request;

/// Extract a required query parameter, or return a 400 bad-request response.
macro_rules! require_param {
	($params:expr, $key:expr) => {
		match $params.remove($key) {
			Some(v) => v,
			None => {
				let msg = format!("Expected '{}' in request, got {:?}.", $key, $params.keys());
				tracing::warn!("{msg}");
				return bad_request(msg);
			},
		}
	};
}

/// Build a `<root status_code="200"><paired>1</paired>…</root>` XML response.
fn paired_xml_response(inner: impl std::fmt::Display) -> Response<Full<Bytes>> {
	let body = format!("<root status_code=\"200\"><paired>1</paired>{inner}</root>");
	let mut response = Response::new(Full::new(Bytes::from(body)));
	response
		.headers_mut()
		.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
	response
}

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
///
/// Step 1 waits until the host operator enters the client's PIN on the
/// loopback-only approval page (`/pin`), bounded by `approval_timeout`.
#[allow(clippy::too_many_arguments)]
pub async fn handle_pair_request(
	request: Request<hyper::body::Incoming>,
	mut params: HashMap<String, String>,
	local_address: Option<SocketAddr>,
	peer_address: SocketAddr,
	server_certs: &str, // Pass as string (PEM)
	client_manager: &ClientManager,
	http_port: u16,
	approval_timeout: Duration,
	shutdown: &ShutdownManager<ShutdownReason>,
) -> Response<Full<Bytes>> {
	if params.contains_key("phrase") {
		match params.remove("phrase").unwrap().as_str() {
			"getservercert" => {
				get_server_cert(
					request,
					params,
					local_address,
					peer_address,
					server_certs,
					client_manager,
					http_port,
					approval_timeout,
					shutdown,
				)
				.await
			},
			"pairchallenge" => pair_challenge(params),
			unknown => {
				let message = format!("Unknown pair phrase received: {}", unknown);
				tracing::warn!("{message}");
				bad_request(message)
			},
		}
	} else if params.contains_key("clientchallenge") {
		client_challenge(params, client_manager)
	} else if params.contains_key("serverchallengeresp") {
		server_challenge_response(params, client_manager)
	} else if params.contains_key("clientpairingsecret") {
		client_pairing_secret(params, client_manager)
	} else {
		let message = format!("Unknown pair command with params: {:?}", params);
		tracing::warn!("{message}");
		bad_request(message)
	}
}

#[allow(clippy::too_many_arguments)]
async fn get_server_cert(
	_request: Request<hyper::body::Incoming>,
	mut params: HashMap<String, String>,
	local_address: Option<SocketAddr>,
	peer_address: SocketAddr,
	server_pem_str: &str,
	client_manager: &ClientManager,
	http_port: u16,
	approval_timeout: Duration,
	shutdown: &ShutdownManager<ShutdownReason>,
) -> Response<Full<Bytes>> {
	let client_cert = require_param!(params, "clientcert");
	let client_cert = match hex::decode(client_cert) {
		Ok(cert) => cert,
		Err(e) => {
			let message = e.to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};

	// PEM is expected to be a string
	let client_pem = match String::from_utf8(client_cert) {
		Ok(s) => s,
		Err(e) => {
			let message = format!("Failed to parse client cert as utf8: {e}");
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};

	let unique_id = require_param!(params, "uniqueid");

	let salt = require_param!(params, "salt");
	let salt = match hex::decode(salt) {
		Ok(salt) => salt,
		Err(e) => {
			let message = e.to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};
	let salt: [u8; 16] = match salt.try_into() {
		Ok(salt) => salt,
		Err(e) => {
			let message = format!("Failed to parse salt value, expected exactly 16 values but got {e:?}");
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};

	let Ok(approval) = new_approval_token() else {
		return bad_request("Failed to start pairing client".to_string());
	};
	let pin_notify = Arc::new(Notify::new());
	// Register interest before the request becomes visible to the operator, so a
	// PIN submitted immediately cannot be missed.
	let pin_notified = pin_notify.notified();
	tokio::pin!(pin_notified);
	pin_notified.as_mut().enable();

	let pending_client = PendingClient {
		id: unique_id.clone(),
		pem: client_pem,
		salt,
		pin_notify: pin_notify.clone(),
		approval: approval.clone(),
		requester: peer_address.ip(),
		key: None,
		server_secret: None,
		server_challenge: None,
		client_hash: None,
	};
	if client_manager.start_pairing(pending_client).is_err() {
		let message = "Failed to start pairing client".to_string();
		tracing::warn!("{message}");
		return bad_request(message);
	}

	// Emit a notification, allowing the user to automatically open the PIN page.
	// Approval is only accepted from the host itself, so link to loopback rather
	// than to the address the client used.
	if local_address.is_some() {
		let encoded_id: String = url::form_urlencoded::byte_serialize(unique_id.as_bytes()).collect();
		let pin_url = format!("http://localhost:{http_port}/pin?uniqueid={encoded_id}");
		tracing::info!(requester = %peer_address, "Waiting for the host operator to enter the PIN at {pin_url}");

		let _ = std::thread::Builder::new()
			.name("pin-notification".to_string())
			.spawn(move || {
				Notification::new()
					.appname("Moonshine")
					.summary("Received pairing request.")
					.body(&format!("Open {pin_url} to enter the PIN."))
					.action("default", "default")
					.action("open", "Enter PIN")
					.show()
					.map_err(|e| tracing::warn!("Failed to show PIN notification: {e}"))?
					.wait_for_action(|action| {
						if action != "__closed"
							&& let Err(e) = open::that(&pin_url)
						{
							tracing::warn!(
								"Couldn't open the PIN page automatically ({e}). Open it manually: {pin_url}"
							);
						}
					});

				Ok::<(), ()>(())
			});
	}

	tokio::select! {
		_ = &mut pin_notified => {},
		_ = tokio::time::sleep(approval_timeout) => {
			tracing::warn!(requester = %peer_address, "Pairing request was not approved in time.");
			client_manager.cancel_pairing(&unique_id, &approval);
			return bad_request("Pairing was not approved in time.".to_string());
		},
		_ = shutdown.wait_shutdown_triggered() => {
			tracing::info!("Shutdown triggered, aborting pairing.");
			client_manager.cancel_pairing(&unique_id, &approval);
			return bad_request("Server is shutting down.".to_string());
		},
	}

	paired_xml_response(format!("<plaincert>{}</plaincert>", hex::encode(server_pem_str)))
}

fn client_challenge(mut params: HashMap<String, String>, client_manager: &ClientManager) -> Response<Full<Bytes>> {
	let unique_id = require_param!(params, "uniqueid");
	let challenge = require_param!(params, "clientchallenge");
	let challenge = match hex::decode(challenge) {
		Ok(challenge) => challenge,
		Err(e) => {
			let message = e.to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};

	let challenge_response = match client_manager.client_challenge(&unique_id, challenge) {
		Ok(challenge_response) => challenge_response,
		Err(()) => {
			return bad_request("Failed to process client challenge".to_string());
		},
	};

	paired_xml_response(format!(
		"<challengeresponse>{}</challengeresponse>",
		hex::encode(challenge_response)
	))
}

fn server_challenge_response(
	mut params: HashMap<String, String>,
	client_manager: &ClientManager,
) -> Response<Full<Bytes>> {
	let server_challenge_response = require_param!(params, "serverchallengeresp");
	let server_challenge_response = match hex::decode(server_challenge_response) {
		Ok(server_challenge_response) => server_challenge_response,
		Err(e) => {
			let message = e.to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};

	let unique_id = require_param!(params, "uniqueid");

	let pairing_secret = match client_manager.server_challenge_response(&unique_id, server_challenge_response) {
		Ok(pairing_secret) => pairing_secret,
		Err(()) => {
			return bad_request("Failed to process server challenge response".to_string());
		},
	};

	paired_xml_response(format!(
		"<pairingsecret>{}</pairingsecret>",
		hex::encode(pairing_secret)
	))
}

fn pair_challenge(params: HashMap<String, String>) -> Response<Full<Bytes>> {
	if !params.contains_key("uniqueid") {
		let message = format!("Expected 'uniqueid' in pair challenge, got {:?}.", params.keys());
		tracing::warn!("{message}");
		return bad_request(message);
	}

	// Client is not persisted here; it is only persisted after the
	// RSA signature verification succeeds in the clientpairingsecret step.
	paired_xml_response("")
}

fn client_pairing_secret(mut params: HashMap<String, String>, client_manager: &ClientManager) -> Response<Full<Bytes>> {
	let client_pairing_secret = require_param!(params, "clientpairingsecret");
	let client_pairing_secret = match hex::decode(client_pairing_secret) {
		Ok(client_pairing_secret) => client_pairing_secret,
		Err(e) => {
			let message = e.to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		},
	};

	let unique_id = require_param!(params, "uniqueid");

	if client_manager
		.check_client_pairing_secret(&unique_id, client_pairing_secret)
		.is_err()
	{
		return bad_request("Failed to check client pairing secret".to_string());
	}

	paired_xml_response("")
}
