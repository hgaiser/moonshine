//! Real loopback HTTPS requests through the normal TLS and Webserver handlers.
use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(
	server: Webserver,
	acceptor: Option<TlsAcceptor>,
	tls: Option<rustls::ClientConfig>,
	path: &str,
) -> Vec<u8> {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let task = tokio::spawn(async move {
		let (socket, peer) = listener.accept().await.unwrap();
		if let Some(acceptor) = acceptor {
			let connection = acceptor.accept(socket).await.unwrap();
			let fingerprint = connection
				.get_ref()
				.1
				.peer_certificates()
				.and_then(|c| c.first())
				.map(|c| hex::encode(Sha256::digest(c.as_ref())));
			hyper::server::conn::http1::Builder::new()
				.serve_connection(
					TokioIo::new(connection),
					service_fn(move |req| {
						let server = server.clone();
						let fingerprint = fingerprint.clone();
						async move { server.serve(req, Some(address), peer, None, true, fingerprint).await }
					}),
				)
				.await
				.unwrap();
		} else {
			hyper::server::conn::http1::Builder::new()
				.serve_connection(
					TokioIo::new(socket),
					service_fn(move |req| {
						let server = server.clone();
						async move { server.serve(req, Some(address), peer, None, false, None).await }
					}),
				)
				.await
				.unwrap();
		}
	});
	let socket = tokio::net::TcpStream::connect(address).await.unwrap();
	let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
	let mut response = Vec::new();
	if let Some(config) = tls {
		let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
			.connect(ServerName::try_from("localhost").unwrap(), socket)
			.await
			.unwrap();
		stream.write_all(request.as_bytes()).await.unwrap();
		stream.read_to_end(&mut response).await.unwrap();
	} else {
		let mut stream = socket;
		stream.write_all(request.as_bytes()).await.unwrap();
		stream.read_to_end(&mut response).await.unwrap();
	}
	task.await.unwrap();
	response
}

#[tokio::test]
async fn authenticated_probe_discovery_exact_payload_and_admission() {
	let directory = tempfile::tempdir().unwrap();
	let host = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
	let host_cert = directory.path().join("host.pem");
	let host_key = directory.path().join("host.key");
	std::fs::write(&host_cert, host.cert.pem()).unwrap();
	std::fs::write(&host_key, host.signing_key.serialize_pem()).unwrap();
	let (client_cert, client_key) = crate::tls::create_certificate().unwrap();
	let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut client_cert.as_bytes())
		.collect::<Result<_, _>>()
		.unwrap();
	let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut client_key.as_bytes())
		.unwrap()
		.unwrap();
	let mut roots = rustls::RootCertStore::empty();
	roots.add(host.cert.der().clone()).unwrap();
	let paired = rustls::ClientConfig::builder()
		.with_root_certificates(roots.clone())
		.with_client_auth_cert(certs.clone(), key)
		.unwrap();
	let anonymous = rustls::ClientConfig::builder()
		.with_root_certificates(roots)
		.with_no_client_auth();
	let shutdown = ShutdownManager::new();
	let session_manager = SessionManager::new(
		Default::default(),
		Default::default(),
		Default::default(),
		Default::default(),
		"127.0.0.1".into(),
		30,
		false,
		shutdown.clone(),
	)
	.unwrap();
	let client_manager = ClientManager::isolated(directory.path().join("state.toml"));
	client_manager
		.persistent_state()
		.add_paired_cert(hex::encode(Sha256::digest(certs[0].as_ref())))
		.unwrap();
	let server = Webserver {
		probe_slots: Arc::new(tokio::sync::Semaphore::new(1)),
		name: "ProbeTest".into(),
		rtsp_port: 1,
		webserver_config: WebserverConfig {
			certificate: host_cert.clone(),
			private_key: host_key.clone(),
			..Default::default()
		},
		applications: vec![],
		unique_id: "test".into(),
		client_manager,
		session_manager,
		server_certs: host.cert.pem(),
		supported_codecs: crate::healthcheck::CODEC_PYROWAVE,
		hdr_supported: false,
		shutdown,
	};
	let acceptor = || TlsAcceptor::from_config(&host_cert, &host_key).unwrap();
	let response = request(
		server.clone(),
		Some(acceptor()),
		Some(paired.clone()),
		"/pyrowave-bandwidth-probe",
	)
	.await;
	let offset = response.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
	assert!(response.starts_with(b"HTTP/1.1 200"));
	assert_eq!(response.len() - offset, bandwidth::PROBE_BYTES);
	assert!(response[offset..].iter().all(|b| *b == 0x50));
	let response = request(
		server.clone(),
		Some(acceptor()),
		Some(anonymous.clone()),
		"/pyrowave-bandwidth-probe",
	)
	.await;
	assert!(response.starts_with(b"HTTP/1.1 401"));
	let response = request(server.clone(), None, None, "/pyrowave-bandwidth-probe").await;
	assert!(response.starts_with(b"HTTP/1.1 404"));
	let response = request(server.clone(), Some(acceptor()), Some(paired.clone()), "/serverinfo").await;
	assert!(
		String::from_utf8_lossy(&response)
			.contains("<PyroWaveBandwidthProbeBytes>33554432</PyroWaveBandwidthProbeBytes>")
	);
	let response = request(server.clone(), Some(acceptor()), Some(anonymous), "/serverinfo").await;
	assert!(!String::from_utf8_lossy(&response).contains("PyroWaveBandwidthProbeBytes"));
	let permit = server.probe_slots.clone().try_acquire_owned().unwrap();
	let response = request(
		server.clone(),
		Some(acceptor()),
		Some(paired.clone()),
		"/pyrowave-bandwidth-probe",
	)
	.await;
	assert!(response.starts_with(b"HTTP/1.1 429"));
	drop(permit);
	let mut unsupported = server;
	unsupported.supported_codecs = crate::healthcheck::CODEC_H264;
	let response = request(unsupported, Some(acceptor()), Some(paired), "/pyrowave-bandwidth-probe").await;
	assert!(response.starts_with(b"HTTP/1.1 404"));
}
