//! Pairing approval authority and network ingress regressions (SEC-001,
//! STAB-005), driven through the normal HTTP handlers.
use super::*;
use crate::clients::{aes_decrypt_ecb, aes_encrypt_ecb, create_key, extract_certificate_signature, sign};
use crate::rtsp::{RtspLimits, RtspServer};
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HTTP_PORT: u16 = 47989;
const REMOTE: &str = "192.168.1.50:50000";
const LOCAL: &str = "127.0.0.1:40000";

struct Fixture {
	server: Webserver,
	client_manager: ClientManager,
	_directory: tempfile::TempDir,
}

fn fixture(enable_pairing: bool, limits: WebLimits) -> Fixture {
	let directory = tempfile::tempdir().unwrap();
	let (server_cert, server_key) = crate::tls::create_certificate().unwrap();
	let certificate = directory.path().join("host.pem");
	let private_key = directory.path().join("host.key");
	std::fs::write(&certificate, &server_cert).unwrap();
	std::fs::write(&private_key, &server_key).unwrap();
	let client_manager =
		ClientManager::isolated_with_identity(directory.path().join("state.toml"), server_cert.clone(), server_key);
	let shutdown = ShutdownManager::new();
	let server = Webserver {
		probe_slots: Arc::new(tokio::sync::Semaphore::new(1)),
		name: "SecurityTest".into(),
		rtsp_port: 48010,
		webserver_config: WebserverConfig {
			port: HTTP_PORT,
			enable_pairing,
			certificate,
			private_key,
			..Default::default()
		},
		applications: vec![],
		unique_id: "test".into(),
		client_manager: client_manager.clone(),
		session_manager: SessionManager::for_test(shutdown.clone()),
		server_certs: server_cert,
		supported_codecs: 0,
		hdr_supported: false,
		shutdown,
		limits,
	};
	Fixture {
		server,
		client_manager,
		_directory: directory,
	}
}

/// One HTTP/1.1 exchange over an in-memory connection from `peer`.
async fn http(server: &Webserver, peer: &str, request: String) -> (u16, String) {
	let (client, serving) = tokio::io::duplex(256 * 1024);
	let server = server.clone();
	let peer: SocketAddr = peer.parse().unwrap();
	let task = tokio::spawn(async move {
		let local = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), HTTP_PORT));
		server
			.serve_http(TokioIo::new(serving), local, peer, None, false, None)
			.await;
	});
	let (mut read, mut write) = tokio::io::split(client);
	write.write_all(request.as_bytes()).await.unwrap();
	let mut response = Vec::new();
	read.read_to_end(&mut response).await.unwrap();
	task.await.unwrap();
	let response = String::from_utf8(response).unwrap();
	let status = response[9..12].parse().unwrap();
	let body = response
		.split_once("\r\n\r\n")
		.map(|(_, body)| body.to_string())
		.unwrap();
	(status, body)
}

fn get(path: &str, host: &str) -> String {
	format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

fn post(path: &str, host: &str, headers: &str, body: &str) -> String {
	format!(
		"POST {path} HTTP/1.1\r\nHost: {host}\r\n{headers}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	)
}

fn submission(unique_id: &str, request: &str, pin: &str) -> String {
	format!("uniqueid={unique_id}&request={request}&pin={pin}")
}

fn xml_value(body: &str, tag: &str) -> String {
	let start = body.find(&format!("<{tag}>")).unwrap() + tag.len() + 2;
	let end = body.find(&format!("</{tag}>")).unwrap();
	body[start..end].to_string()
}

/// The approval token embedded in the operator page.
async fn operator_page_token(server: &Webserver, unique_id: &str) -> String {
	let (status, page) = http(
		server,
		LOCAL,
		get(&format!("/pin?uniqueid={unique_id}"), "localhost:47989"),
	)
	.await;
	assert_eq!(status, 200, "{page}");
	assert!(page.contains("192.168.1.50"), "page shows the requester");
	let start = page.find("id=\"request-value\" value=\"").unwrap() + 26;
	page[start..start + 32].to_string()
}

/// Moonlight's side of the pairing protocol.
struct PairingClient {
	unique_id: String,
	cert_pem: String,
	key_pem: String,
	salt: [u8; 16],
	pin: String,
}

impl PairingClient {
	fn new(unique_id: &str, pin: &str) -> Self {
		let (cert_pem, key_pem) = crate::tls::create_certificate().unwrap();
		Self {
			unique_id: unique_id.into(),
			cert_pem,
			key_pem,
			salt: [0x5a; 16],
			pin: pin.into(),
		}
	}

	fn fingerprint(&self) -> String {
		let (_, pem) = x509_parser::pem::parse_x509_pem(self.cert_pem.as_bytes()).unwrap();
		hex::encode(Sha256::digest(&pem.contents))
	}

	/// Step 1, which blocks until the operator approves.
	fn request_server_cert(&self, server: &Webserver) -> tokio::task::JoinHandle<(u16, String)> {
		let path = format!(
			"/pair?uniqueid={}&devicename=roth&updateState=1&phrase=getservercert&salt={}&clientcert={}",
			self.unique_id,
			hex::encode(self.salt),
			hex::encode(&self.cert_pem)
		);
		let server = server.clone();
		tokio::spawn(async move { http(&server, REMOTE, get(&path, "192.168.1.10:47989")).await })
	}

	async fn wait_pending(&self, client_manager: &ClientManager) {
		let deadline = Instant::now() + Duration::from_secs(5);
		while client_manager.pending_approval(&self.unique_id).is_none() {
			assert!(Instant::now() < deadline, "pairing request was not registered");
			tokio::task::yield_now().await;
		}
	}

	/// Steps 2–5 over HTTP from the client's address. Returns whether the
	/// server accepted the final pairing secret.
	async fn complete(&self, server: &Webserver) -> bool {
		let key = create_key(&self.salt, &self.pin).unwrap();
		let pair = |query: String| {
			let request = get(
				&format!("/pair?uniqueid={}&{query}", self.unique_id),
				"192.168.1.10:47989",
			);
			async move { http(server, REMOTE, request).await }
		};

		let challenge = aes_encrypt_ecb(&[0x11; 16], &key).unwrap();
		let (status, body) = pair(format!("clientchallenge={}", hex::encode(challenge))).await;
		if status != 200 {
			return false;
		}
		let response = aes_decrypt_ecb(&hex::decode(xml_value(&body, "challengeresponse")).unwrap(), &key).unwrap();
		let server_challenge = &response[32..48];

		let client_secret = [0x22; 16];
		let mut hashed = server_challenge.to_vec();
		hashed.extend(extract_certificate_signature(&self.cert_pem).unwrap());
		hashed.extend(client_secret);
		let client_hash = aes_encrypt_ecb(&Sha256::digest(&hashed), &key).unwrap();
		let (status, _) = pair(format!("serverchallengeresp={}", hex::encode(client_hash))).await;
		if status != 200 {
			return false;
		}
		let (status, _) = pair("phrase=pairchallenge".into()).await;
		if status != 200 {
			return false;
		}
		let mut pairing_secret = client_secret.to_vec();
		pairing_secret.extend(sign(&client_secret, &self.key_pem).unwrap());
		let (status, _) = pair(format!("clientpairingsecret={}", hex::encode(pairing_secret))).await;
		status == 200
	}
}

#[test]
fn operator_requests_require_local_peer_host_and_origin() {
	let headers = |pairs: &[(&str, &str)]| {
		let mut map = header::HeaderMap::new();
		for (name, value) in pairs {
			map.insert(
				header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
				value.parse().unwrap(),
			);
		}
		map
	};
	let local_host = headers(&[("host", "localhost:47989")]);
	for peer in ["127.0.0.1:1", "127.0.0.53:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
		assert_eq!(
			authorize_operator_request(peer.parse().unwrap(), &local_host, HTTP_PORT),
			Ok(()),
			"{peer}"
		);
	}
	for peer in ["192.168.1.50:1", "[::ffff:192.168.1.50]:1", "[fd00::1]:1", "0.0.0.0:1"] {
		assert!(
			authorize_operator_request(peer.parse().unwrap(), &local_host, HTTP_PORT).is_err(),
			"{peer}"
		);
	}

	let local: SocketAddr = LOCAL.parse().unwrap();
	for host in ["localhost:47989", "LocalHost:47989", "127.0.0.1:47989", "[::1]:47989"] {
		assert_eq!(
			authorize_operator_request(local, &headers(&[("host", host)]), HTTP_PORT),
			Ok(()),
			"{host}"
		);
	}
	// DNS rebinding: an attacker domain resolving to 127.0.0.1.
	for host in [
		"rebind.example:47989",
		"localhost.example:47989",
		"localhost:8080",
		"localhost",
		"192.168.1.10:47989",
		"[::1]",
		"[::1:47989",
	] {
		assert!(
			authorize_operator_request(local, &headers(&[("host", host)]), HTTP_PORT).is_err(),
			"{host}"
		);
	}
	assert!(authorize_operator_request(local, &header::HeaderMap::new(), HTTP_PORT).is_err());

	// CSRF: browser metadata must describe a same-origin loopback request.
	for (name, value) in [
		("origin", "http://localhost:47989"),
		("origin", "http://127.0.0.1:47989"),
		("sec-fetch-site", "same-origin"),
		("sec-fetch-site", "none"),
	] {
		assert_eq!(
			authorize_operator_request(
				local,
				&headers(&[("host", "localhost:47989"), (name, value)]),
				HTTP_PORT
			),
			Ok(()),
			"{name}: {value}"
		);
	}
	for (name, value) in [
		("origin", "https://attacker.example"),
		("origin", "http://attacker.example:47989"),
		("origin", "http://localhost:3000"),
		("origin", "null"),
		("sec-fetch-site", "cross-site"),
		("sec-fetch-site", "same-site"),
	] {
		assert!(
			authorize_operator_request(
				local,
				&headers(&[("host", "localhost:47989"), (name, value)]),
				HTTP_PORT
			)
			.is_err(),
			"{name}: {value}"
		);
	}
}

#[test]
fn loopback_operator_listener_only_when_needed() {
	for bound in ["0.0.0.0:47989", "[::]:47989", "127.0.0.1:47989", "[::1]:47989"] {
		assert_eq!(operator_listen_address(bound.parse().unwrap()), None, "{bound}");
	}
	assert_eq!(
		operator_listen_address("192.168.1.10:47989".parse().unwrap()),
		Some("127.0.0.1:47989".parse().unwrap())
	);
}

/// A remote requester cannot approve its own pairing; the host operator can.
#[tokio::test]
async fn pairing_requires_loopback_operator_approval() {
	let Fixture {
		server,
		client_manager,
		_directory,
	} = fixture(true, WebLimits::default());
	let client = PairingClient::new("0123456789ABCDEF", "4321");
	let server_cert = client.request_server_cert(&server);
	client.wait_pending(&client_manager).await;
	let token = client_manager.pending_approval(&client.unique_id).unwrap().approval;

	// Self-approval from the requester, even knowing the token, over IPv4 or
	// IPv4-mapped IPv6 and with a spoofed loopback Host.
	for peer in [REMOTE, "[::ffff:192.168.1.50]:50000"] {
		for host in ["192.168.1.10:47989", "localhost:47989"] {
			let (status, _) = http(&server, peer, get("/pin", host)).await;
			assert_eq!(status, 403);
			let body = submission(&client.unique_id, &token, &client.pin);
			let (status, _) = http(&server, peer, post("/submit-pin", host, "", &body)).await;
			assert_eq!(status, 403, "{peer} {host}");
		}
	}
	// Cross-site submission through the operator's own browser.
	let body = submission(&client.unique_id, &token, &client.pin);
	let csrf = "Origin: https://attacker.example\r\nSec-Fetch-Site: cross-site\r\n";
	let (status, _) = http(&server, LOCAL, post("/submit-pin", "localhost:47989", csrf, &body)).await;
	assert_eq!(status, 403);
	assert!(!server_cert.is_finished(), "still waiting for operator approval");
	assert!(!client.complete(&server).await, "no pairing without approval");

	// The operator approves the displayed request from the host.
	assert_eq!(operator_page_token(&server, &client.unique_id).await, token);
	let wrong_request = submission(&client.unique_id, &"0".repeat(32), &client.pin);
	let (status, _) = http(
		&server,
		LOCAL,
		post("/submit-pin", "localhost:47989", "", &wrong_request),
	)
	.await;
	assert_eq!(status, 400, "PIN must name the displayed request");
	let (status, _) = http(
		&server,
		LOCAL,
		post(
			"/submit-pin",
			"localhost:47989",
			"",
			&submission(&client.unique_id, &token, "12ab"),
		),
	)
	.await;
	assert_eq!(status, 400, "PIN must be numeric");
	let same_origin = "Origin: http://localhost:47989\r\nSec-Fetch-Site: same-origin\r\n";
	let (status, body) = http(
		&server,
		"[::1]:40000",
		post("/submit-pin", "localhost:47989", same_origin, &body),
	)
	.await;
	assert_eq!(status, 200, "{body}");
	let (status, body) = tokio::time::timeout(Duration::from_secs(5), server_cert)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(status, 200, "{body}");
	assert!(body.contains("<plaincert>"));

	assert!(client.complete(&server).await);
	assert!(client_manager.is_cert_paired(&client.fingerprint()).unwrap());
}

/// A wrong operator PIN cannot complete pairing.
#[tokio::test]
async fn wrong_pin_does_not_pair() {
	let Fixture {
		server,
		client_manager,
		_directory,
	} = fixture(true, WebLimits::default());
	let client = PairingClient::new("0123456789ABCDEF", "1111");
	let server_cert = client.request_server_cert(&server);
	client.wait_pending(&client_manager).await;
	let token = operator_page_token(&server, &client.unique_id).await;
	let body = submission(&client.unique_id, &token, "2222");
	let (status, _) = http(&server, LOCAL, post("/submit-pin", "localhost:47989", "", &body)).await;
	assert_eq!(status, 200);
	assert_eq!(server_cert.await.unwrap().0, 200);
	assert!(!client.complete(&server).await);
	assert!(!client_manager.is_cert_paired(&client.fingerprint()).unwrap());
	// The pending request accepts only one PIN.
	let body = submission(&client.unique_id, &token, &client.pin);
	let (status, _) = http(&server, LOCAL, post("/submit-pin", "localhost:47989", "", &body)).await;
	assert_eq!(status, 400);
}

/// An approval applies only to the request the operator was shown: a request
/// that replaces it under the same client ID does not inherit the PIN, and an
/// unapproved request expires without removing its replacement.
#[tokio::test]
async fn approval_is_bound_to_the_displayed_request() {
	let limits = WebLimits {
		pairing_approval_timeout: Duration::from_millis(300),
		..Default::default()
	};
	let Fixture {
		server,
		client_manager,
		_directory,
	} = fixture(true, limits);
	let first = PairingClient::new("0123456789ABCDEF", "1234");
	let first_cert = first.request_server_cert(&server);
	first.wait_pending(&client_manager).await;
	let shown = operator_page_token(&server, &first.unique_id).await;

	let replacement = PairingClient::new("0123456789ABCDEF", "9999");
	let replacement_cert = replacement.request_server_cert(&server);
	let deadline = Instant::now() + Duration::from_secs(5);
	while client_manager.pending_approval(&first.unique_id).unwrap().approval == shown {
		assert!(Instant::now() < deadline);
		tokio::task::yield_now().await;
	}
	let body = submission(&first.unique_id, &shown, &first.pin);
	let (status, _) = http(&server, LOCAL, post("/submit-pin", "localhost:47989", "", &body)).await;
	assert_eq!(status, 400);

	let (status, body) = first_cert.await.unwrap();
	assert_eq!(status, 400, "{body}");
	let (status, body) = replacement_cert.await.unwrap();
	assert_eq!(status, 400, "{body}");
	assert!(
		client_manager.pending_approval(&first.unique_id).is_none(),
		"expired requests are removed"
	);
	assert!(!replacement.complete(&server).await);
}

#[tokio::test]
async fn disabled_pairing_rejects_every_pairing_route() {
	let Fixture { server, _directory, .. } = fixture(false, WebLimits::default());
	for (peer, request) in [
		(
			REMOTE,
			get("/pair?uniqueid=1&phrase=getservercert", "192.168.1.10:47989"),
		),
		(LOCAL, get("/pin", "localhost:47989")),
		(
			LOCAL,
			post("/submit-pin", "localhost:47989", "", "uniqueid=1&request=1&pin=1234"),
		),
	] {
		let (status, body) = http(&server, peer, request).await;
		assert_eq!((status, body.as_str()), (400, "Pairing is disabled."));
	}
}

fn tls_client() -> tokio_rustls::TlsConnector {
	#[derive(Debug)]
	struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);
	impl rustls::client::danger::ServerCertVerifier for AcceptAny {
		fn verify_server_cert(
			&self,
			_: &rustls::pki_types::CertificateDer<'_>,
			_: &[rustls::pki_types::CertificateDer<'_>],
			_: &ServerName<'_>,
			_: &[u8],
			_: rustls::pki_types::UnixTime,
		) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
			Ok(rustls::client::danger::ServerCertVerified::assertion())
		}
		fn verify_tls12_signature(
			&self,
			message: &[u8],
			cert: &rustls::pki_types::CertificateDer<'_>,
			dss: &rustls::DigitallySignedStruct,
		) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
			rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
		}
		fn verify_tls13_signature(
			&self,
			message: &[u8],
			cert: &rustls::pki_types::CertificateDer<'_>,
			dss: &rustls::DigitallySignedStruct,
		) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
			rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
		}
		fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
			self.0.signature_verification_algorithms.supported_schemes()
		}
	}
	let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
	let config = rustls::ClientConfig::builder_with_provider(provider.clone())
		.with_safe_default_protocol_versions()
		.unwrap()
		.dangerous()
		.with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
		.with_no_client_auth();
	tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn serve_https(server: &Webserver) -> SocketAddr {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let acceptor = TlsAcceptor::from_config(
		server.webserver_config.certificate.clone(),
		server.webserver_config.private_key.clone(),
	)
	.unwrap();
	let ingress = Ingress::new("https-test", MAX_HTTP_CONNECTIONS, server.shutdown.clone());
	let server = server.clone();
	tokio::spawn(async move {
		let shutdown = server.shutdown.clone();
		shutdown
			.wrap_cancel(server.serve_listener(listener, Some(Arc::new(acceptor)), ingress))
			.await
	});
	address
}

async fn https_server_info(address: SocketAddr) -> String {
	let socket = tokio::net::TcpStream::connect(address).await.unwrap();
	let mut stream = tls_client()
		.connect(ServerName::try_from("localhost").unwrap(), socket)
		.await
		.unwrap();
	stream
		.write_all(get("/serverinfo", "localhost").as_bytes())
		.await
		.unwrap();
	let mut response = Vec::new();
	stream.read_to_end(&mut response).await.unwrap();
	String::from_utf8(response).unwrap()
}

/// A client that never sends a ClientHello neither blocks other clients nor
/// holds its connection beyond the handshake deadline.
#[tokio::test]
async fn idle_tls_connection_does_not_block_valid_clients() {
	let limits = WebLimits {
		tls_handshake_timeout: Duration::from_millis(400),
		..Default::default()
	};
	let Fixture { server, _directory, .. } = fixture(true, limits);
	let address = serve_https(&server).await;
	let mut idle = tokio::net::TcpStream::connect(address).await.unwrap();

	let started = Instant::now();
	let response = tokio::time::timeout(Duration::from_secs(5), https_server_info(address))
		.await
		.expect("valid client served while another connection is idle");
	assert!(response.starts_with("HTTP/1.1 200"));
	assert!(
		started.elapsed() < limits.tls_handshake_timeout,
		"not serialized behind the idle socket"
	);

	let mut buf = [0u8; 16];
	let closed = tokio::time::timeout(Duration::from_secs(5), idle.read(&mut buf))
		.await
		.unwrap();
	assert!(
		matches!(closed, Ok(0) | Err(_)),
		"idle handshake closed at its deadline"
	);
	server.shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
}

/// Request headers that never complete are closed at the header deadline.
#[tokio::test]
async fn incomplete_http_request_is_closed() {
	let limits = WebLimits {
		header_read_timeout: Duration::from_millis(300),
		..Default::default()
	};
	let Fixture { server, _directory, .. } = fixture(true, limits);
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let ingress = Ingress::new("http-test", MAX_HTTP_CONNECTIONS, server.shutdown.clone());
	tokio::spawn({
		let server = server.clone();
		async move { server.serve_listener(listener, None, ingress).await }
	});
	let mut slow = tokio::net::TcpStream::connect(address).await.unwrap();
	slow.write_all(b"GET /serverinfo HTTP/1.1\r\nHost: local")
		.await
		.unwrap();
	let mut response = Vec::new();
	tokio::time::timeout(Duration::from_secs(5), slow.read_to_end(&mut response))
		.await
		.expect("closed at the header deadline")
		.ok();
	assert!(!response.starts_with(b"HTTP/1.1 200"));
	server.shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
}

/// Global shutdown completes promptly with stalled HTTPS, HTTP, RTSP and
/// pairing clients, and every handler releases the shared session manager
/// (whose delay token holds shutdown open until it is dropped).
#[tokio::test]
async fn shutdown_releases_stalled_connection_handlers() {
	let Fixture {
		server,
		client_manager,
		_directory,
	} = fixture(true, WebLimits::default());
	let shutdown = server.shutdown.clone();
	let https = serve_https(&server).await;
	let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let http_address = http_listener.local_addr().unwrap();
	tokio::spawn({
		let server = server.clone();
		let ingress = Ingress::new("http-test", MAX_HTTP_CONNECTIONS, shutdown.clone());
		async move {
			let cancel = server.shutdown.clone();
			cancel
				.wrap_cancel(server.serve_listener(http_listener, None, ingress))
				.await
		}
	});
	server
		.session_manager
		.authorize_client_for_test(IpAddr::V4(Ipv4Addr::LOCALHOST))
		.await;
	let rtsp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let rtsp_address = rtsp_listener.local_addr().unwrap();
	tokio::spawn({
		let rtsp = RtspServer::for_test(server.session_manager.clone(), RtspLimits::default());
		let ingress = Ingress::new("rtsp-test", 16, shutdown.clone());
		let cancel = shutdown.clone();
		async move { cancel.wrap_cancel(rtsp.serve_listener(rtsp_listener, ingress)).await }
	});

	let _idle_tls = tokio::net::TcpStream::connect(https).await.unwrap();
	let mut partial_http = tokio::net::TcpStream::connect(http_address).await.unwrap();
	partial_http.write_all(b"GET /serverinfo HTTP/1.1\r\n").await.unwrap();
	let mut partial_rtsp = tokio::net::TcpStream::connect(rtsp_address).await.unwrap();
	partial_rtsp.write_all(b"OPTIONS rtsp://x RTSP/1.0\r\n").await.unwrap();
	let pairing = PairingClient::new("0123456789ABCDEF", "1234");
	let waiting_for_pin = pairing.request_server_cert(&server);
	pairing.wait_pending(&client_manager).await;
	tokio::time::sleep(Duration::from_millis(50)).await;

	drop(server);
	shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
	tokio::time::timeout(Duration::from_secs(2), shutdown.wait_shutdown_complete())
		.await
		.expect("connection handlers released the session manager and their sockets");
	let (status, body) = waiting_for_pin.await.unwrap();
	assert_eq!((status, body.as_str()), (400, "Server is shutting down."));
	assert!(client_manager.pending_approval(&pairing.unique_id).is_none());
}
