use std::{
	collections::HashMap,
	convert::Infallible,
	net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
	path::PathBuf,
	str::FromStr,
};

use async_shutdown::ShutdownManager;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
	Method, Request, Response, StatusCode,
	body::Bytes,
	header::{self, HeaderValue},
	service::service_fn,
};
use hyper_util::rt::tokio::{TokioIo, TokioTimer};
use image::ImageFormat;
use image::imageops::FilterType;
use network_interface::NetworkInterfaceConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::{
	ShutdownReason,
	clients::ClientManager,
	ingress::Ingress,
	session::{
		APP_LAUNCH_HTTP_TIMEOUT_SECS, SessionContext, SessionKeyData, SessionKeys,
		application::ApplicationConfig,
		manager::SessionManager,
		negotiation::{self, DisplayMode},
	},
	tls::TlsAcceptor,
};

use self::pairing::handle_pair_request;

use super::session::stream::audio::AudioChannels;

mod bandwidth;
#[cfg(test)]
mod bandwidth_tests;
mod pairing;
#[cfg(test)]
mod security_tests;

/// Configuration for the embedded webserver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebserverConfig {
	/// Port of the webserver.
	pub port: u16,

	/// Port of the HTTPS webserver.
	pub port_https: u16,

	/// Whether to allow new clients to pair.
	#[serde(default = "default_enable_pairing")]
	pub enable_pairing: bool,

	/// Path to the certificate for SSL encryption.
	pub certificate: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

fn default_enable_pairing() -> bool {
	true
}

impl Default for WebserverConfig {
	fn default() -> Self {
		Self {
			port: 47989,
			port_https: 47984,
			enable_pairing: default_enable_pairing(),
			certificate: "$HOME/.config/moonshine/cert.pem".into(),
			private_key: "$HOME/.config/moonshine/key.pem".into(),
		}
	}
}

// The negative fourth value is to indicate that we are following the protocol introduced with Sunshine.
const SERVERINFO_APP_VERSION: &str = "7.1.431.-1";
const SERVERINFO_GFE_VERSION: &str = "3.23.0.74";

#[derive(Clone)]
pub struct Webserver {
	probe_slots: std::sync::Arc<tokio::sync::Semaphore>,
	name: String,
	rtsp_port: u16,
	webserver_config: WebserverConfig,
	applications: Vec<ApplicationConfig>,
	unique_id: String,
	client_manager: ClientManager,
	session_manager: SessionManager,
	server_certs: String,
	supported_codecs: u32,
	hdr_supported: bool,
	shutdown: ShutdownManager<ShutdownReason>,
	limits: WebLimits,
}

/// Concurrent connections per HTTP/HTTPS listener. Moonlight uses a handful
/// (server polling, app list, a few parallel box-art downloads).
const MAX_HTTP_CONNECTIONS: usize = 64;
/// Hyper read buffer bound per connection. Pairing requests carry a hex PEM
/// certificate in the query string, which fits comfortably.
const MAX_HTTP_BUFFER_BYTES: usize = 64 * 1024;
/// Bound for operator PIN submission bodies.
const MAX_PIN_BODY_BYTES: usize = 1024;

/// Deadlines applied to every accepted HTTP/HTTPS connection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WebLimits {
	/// Time for a client to complete the TLS handshake.
	pub tls_handshake_timeout: std::time::Duration,
	/// Time to receive request headers. Hyper also applies it while a keep-alive
	/// connection waits for the next request, bounding idle connections.
	pub header_read_timeout: std::time::Duration,
	/// Time to receive a request body the server reads (PIN submission).
	pub body_read_timeout: std::time::Duration,
	/// Time an operator has to approve a pairing request.
	pub pairing_approval_timeout: std::time::Duration,
}

impl Default for WebLimits {
	fn default() -> Self {
		Self {
			tls_handshake_timeout: std::time::Duration::from_secs(10),
			header_read_timeout: std::time::Duration::from_secs(30),
			body_read_timeout: std::time::Duration::from_secs(10),
			pairing_approval_timeout: std::time::Duration::from_secs(300),
		}
	}
}

impl Webserver {
	#[allow(clippy::result_unit_err)]
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		name: String,
		address: String,
		rtsp_port: u16,
		webserver_config: WebserverConfig,
		applications: Vec<ApplicationConfig>,
		supported_codecs: u32,
		hdr_supported: bool,
		unique_id: String,
		// Passing certificate content as string.
		server_certs: String,
		client_manager: ClientManager,
		session_manager: SessionManager,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		let server = Self {
			probe_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
			name,
			rtsp_port,
			webserver_config,
			applications,
			unique_id,
			client_manager,
			session_manager,
			server_certs,
			supported_codecs,
			hdr_supported,
			shutdown: shutdown.clone(),
			limits: WebLimits::default(),
		};

		// Run HTTP webserver.
		let http_address = resolve_listen_address(&address, server.webserver_config.port)?;
		server.spawn_listener("http", http_address, None, ShutdownReason::HttpShutdown);

		// PIN approval is restricted to loopback peers. When the configured
		// address does not accept loopback connections, give the operator a
		// loopback-only listener on the same port. Failing to bind it only
		// disables local approval, not the GameStream service.
		if let Some(operator_address) = operator_listen_address(http_address) {
			tokio::spawn({
				let server = server.clone();
				async move {
					let shutdown = server.shutdown.clone();
					let _ = shutdown
						.wrap_cancel(async move {
							let listener = bind_listener(operator_address).map_err(|e| {
								tracing::warn!(
									"Failed to bind the local pairing approval listener on {operator_address}: {e}"
								)
							})?;
							tracing::debug!("Local pairing approval available on {operator_address}");
							let ingress = Ingress::new("http-operator", MAX_HTTP_CONNECTIONS, server.shutdown.clone());
							server.serve_listener(listener, None, ingress).await
						})
						.await;
				}
			});
		}

		// Run HTTPS webserver.
		let https_address = resolve_listen_address(&address, server.webserver_config.port_https)?;
		let acceptor = TlsAcceptor::from_config(
			server.webserver_config.certificate.clone(),
			server.webserver_config.private_key.clone(),
		)?;
		server.spawn_listener(
			"https",
			https_address,
			Some(std::sync::Arc::new(acceptor)),
			ShutdownReason::HttpsShutdown,
		);

		Ok(server)
	}

	/// Run a listener until global shutdown; a listener failure shuts down the server.
	fn spawn_listener(
		&self,
		name: &'static str,
		address: SocketAddr,
		tls: Option<std::sync::Arc<TlsAcceptor>>,
		reason: ShutdownReason,
	) {
		let server = self.clone();
		tokio::spawn(async move {
			let shutdown = server.shutdown.clone();
			let _ = shutdown
				.wrap_cancel(shutdown.wrap_trigger_shutdown(reason, async move {
					let listener = bind_listener(address)
						.map_err(|e| tracing::error!("Failed to bind to address {address}: {e}"))?;
					tracing::debug!("{name} server listening for connections on {address}");
					let ingress = Ingress::new(name, MAX_HTTP_CONNECTIONS, server.shutdown.clone());
					server.serve_listener(listener, tls, ingress).await
				}))
				.await;
			tracing::debug!("{name} server shutting down.");
		});
	}

	/// Accept connections. Each one, including its TLS handshake, runs in its own
	/// bounded and cancellable task so a stalled client cannot block the others.
	async fn serve_listener(
		&self,
		listener: TcpListener,
		tls: Option<std::sync::Arc<TlsAcceptor>>,
		ingress: Ingress,
	) -> Result<(), ()> {
		loop {
			let (connection, address) = listener
				.accept()
				.await
				.map_err(|e| tracing::error!("Failed to accept connection: {e}"))?;
			tracing::trace!("Accepted connection from {address}.");

			let Some(permit) = ingress.admit(address) else {
				continue;
			};
			let server = self.clone();
			let tls = tls.clone();
			permit.spawn(async move { server.serve_connection(connection, address, tls).await });
		}
	}

	async fn serve_connection(
		&self,
		connection: tokio::net::TcpStream,
		address: SocketAddr,
		tls: Option<std::sync::Arc<TlsAcceptor>>,
	) {
		let peer_address = unmap_v4_mapped(address);
		let local_address = connection.local_addr().ok().map(unmap_v4_mapped);
		let mac_address = local_address.and_then(|address| get_mac_address(address.ip()).unwrap_or(None));

		let Some(acceptor) = tls else {
			self.serve_http(
				TokioIo::new(connection),
				local_address,
				peer_address,
				mac_address,
				false,
				None,
			)
			.await;
			return;
		};

		let connection =
			match tokio::time::timeout(self.limits.tls_handshake_timeout, acceptor.accept(connection)).await {
				Ok(Ok(connection)) => connection,
				Ok(Err(())) => return,
				Err(_) => {
					tracing::debug!("TLS handshake from {peer_address} timed out.");
					return;
				},
			};

		// Extract peer certificate fingerprint from TLS connection for mTLS verification.
		let peer_cert_fingerprint = connection
			.get_ref()
			.1
			.peer_certificates()
			.and_then(|certs| certs.first())
			.map(|cert| hex::encode(Sha256::digest(cert.as_ref())));

		self.serve_http(
			TokioIo::new(connection),
			local_address,
			peer_address,
			mac_address,
			true,
			peer_cert_fingerprint,
		)
		.await;
	}

	/// Serve HTTP/1.1 requests on an established (optionally TLS) connection.
	async fn serve_http<I>(
		&self,
		io: I,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<String>,
	) where
		I: hyper::rt::Read + hyper::rt::Write + Unpin,
	{
		let _ = hyper::server::conn::http1::Builder::new()
			.timer(TokioTimer::new())
			.header_read_timeout(self.limits.header_read_timeout)
			.max_buf_size(MAX_HTTP_BUFFER_BYTES)
			.serve_connection(
				io,
				service_fn(|request| {
					self.serve(
						request,
						local_address,
						peer_address,
						mac_address.clone(),
						https,
						peer_cert_fingerprint.clone(),
					)
				}),
			)
			.await;
	}

	async fn serve(
		&self,
		request: Request<hyper::body::Incoming>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<String>,
	) -> Result<Response<bandwidth::ResponseBody>, Infallible> {
		let params = request
			.uri()
			.query()
			.map(|v| url::form_urlencoded::parse(v.as_bytes()).into_owned().collect())
			.unwrap_or_default();

		tracing::debug!("Received {} request for {}.", request.method(), request.uri().path());

		let response = if https {
			match (request.method(), request.uri().path()) {
				(&Method::GET, "/serverinfo") => {
					self.server_info(
						params,
						local_address,
						peer_address,
						mac_address,
						https,
						peer_cert_fingerprint.as_ref(),
					)
					.await
				},
				(&Method::GET, "/pyrowave-bandwidth-probe") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					if self.supported_codecs & 0x01800000 == 0 {
						return Ok(not_found().map(BodyExt::boxed_unsync));
					}
					// Explicit calibration is unavailable during a session so a
					// bulk download never competes with interactive streaming.
					if !matches!(self.session_manager.get_session_context().await, Ok(None)) {
						return Ok(Response::builder()
							.status(StatusCode::CONFLICT)
							.body(Full::new(Bytes::from_static(
								b"End the stream before bandwidth calibration",
							)))
							.unwrap()
							.map(BodyExt::boxed_unsync));
					}
					let Ok(permit) = self.probe_slots.clone().try_acquire_owned() else {
						return Ok(Response::builder()
							.status(StatusCode::TOO_MANY_REQUESTS)
							.body(Full::new(Bytes::new()))
							.unwrap()
							.map(BodyExt::boxed_unsync));
					};
					return Ok(bandwidth::response(permit));
				},
				(&Method::GET, "/applist") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.app_list()
				},
				(&Method::GET, "/appasset") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.app_asset(params)
				},
				(&Method::GET, "/pair") => {
					if !self.webserver_config.enable_pairing {
						tracing::warn!("Pairing is disabled in configuration.");
						return Ok(bad_request("Pairing is disabled.".to_string()).map(BodyExt::boxed_unsync));
					}
					handle_pair_request(
						request,
						params,
						local_address,
						peer_address,
						&self.server_certs,
						&self.client_manager,
						self.webserver_config.port,
						self.limits.pairing_approval_timeout,
						&self.shutdown,
					)
					.await
				},
				(&Method::GET, "/unpair") => self.unpair(params).await,
				(&Method::GET, "/launch") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.launch(params, local_address, peer_address).await
				},
				(&Method::GET, "/resume") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.resume(params, local_address, peer_address).await
				},
				(&Method::GET, "/cancel") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.cancel().await
				},
				(method, uri) => {
					tracing::warn!("Unhandled {method} request with URI '{uri}'");
					not_found()
				},
			}
		} else {
			match (request.method(), request.uri().path()) {
				(&Method::GET, "/serverinfo") => {
					self.server_info(
						params,
						local_address,
						peer_address,
						mac_address,
						https,
						peer_cert_fingerprint.as_ref(),
					)
					.await
				},
				(&Method::GET, "/pair") => {
					if !self.webserver_config.enable_pairing {
						tracing::warn!("Pairing is disabled in configuration.");
						return Ok(bad_request("Pairing is disabled.".to_string()).map(BodyExt::boxed_unsync));
					}
					handle_pair_request(
						request,
						params,
						local_address,
						peer_address,
						&self.server_certs,
						&self.client_manager,
						self.webserver_config.port,
						self.limits.pairing_approval_timeout,
						&self.shutdown,
					)
					.await
				},
				(&Method::GET, "/pin") => {
					if !self.webserver_config.enable_pairing {
						return Ok(bad_request("Pairing is disabled.".to_string()).map(BodyExt::boxed_unsync));
					}
					if let Some(response) = self.verify_operator(&request, peer_address) {
						return Ok(response.map(BodyExt::boxed_unsync));
					}
					self.pin(params)
				},
				(&Method::POST, "/submit-pin") => {
					if !self.webserver_config.enable_pairing {
						return Ok(bad_request("Pairing is disabled.".to_string()).map(BodyExt::boxed_unsync));
					}
					if let Some(response) = self.verify_operator(&request, peer_address) {
						return Ok(response.map(BodyExt::boxed_unsync));
					}
					self.submit_pin(request).await
				},
				(&Method::GET, "/unpair") => self.unpair(params).await,
				(method, uri) => {
					tracing::warn!("Unhandled {method} request with URI '{uri}'");
					not_found()
				},
			}
		};

		Ok(response.map(BodyExt::boxed_unsync))
	}

	fn app_list(&self) -> Response<Full<Bytes>> {
		let mut response = "<root status_code=\"200\">".to_string();
		for application in self.applications.iter() {
			response += "<App>";

			let hdr_supported = u8::from(self.hdr_supported);
			response += format!("<IsHdrSupported>{hdr_supported}</IsHdrSupported>").as_ref();
			response += format!("<AppTitle>{}</AppTitle>", escape_xml(&application.title)).as_ref();
			response += format!("<ID>{}</ID>", application.id()).as_ref();

			response += "</App>";
		}

		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	fn app_asset(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let application_id = match params.remove("appid") {
			Some(application_id) => application_id,
			None => {
				let message = format!("Expected 'appasset' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let application_id: i32 = match application_id.parse() {
			Ok(application_id) => application_id,
			Err(e) => {
				let message = format!("Failed to parse application ID: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let application = match self.applications.iter().find(|&a| a.id() == application_id) {
			Some(application) => application,
			None => {
				let message = format!("Couldn't find application with ID {}.", application_id - 1);
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let boxart_path = match &application.boxart {
			Some(boxart) => boxart,
			None => {
				let message = format!("No boxart defined for app '{}'.", application.title);
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let boxart_path = boxart_path.to_string_lossy();
		let boxart_path = match shellexpand::full(&boxart_path) {
			Ok(boxart_path) => boxart_path,
			Err(e) => {
				let message = format!("Failed to expand boxart path: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let boxart_path = match PathBuf::from_str(&boxart_path) {
			Ok(boxart_path) => boxart_path,
			Err(e) => {
				let message = format!("Failed to create boxart path: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let image_bytes = match std::fs::read(&boxart_path) {
			Ok(bytes) => bytes,
			Err(e) => {
				let message = format!("Failed to read boxart at '{}': {e}", boxart_path.display());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let asset = match image::load_from_memory(&image_bytes) {
			Ok(asset) => asset,
			Err(e) => {
				let message = format!("Failed to load boxart at '{}': {e}", boxart_path.display());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		// Moonlight displays box art at a fixed 200x267 pixel area using stretch mode.
		// Icons that don't match this ratio (e.g. square desktop icons) get distorted.
		// Fit the image into a 600x801 canvas (same ratio as 200:267), preserving aspect ratio, centered.
		let asset = fit_to_boxart(asset);

		let mut buffer = std::io::Cursor::new(vec![]);
		if let Err(e) = asset.write_to(&mut buffer, ImageFormat::Png) {
			let message = format!("Failed to encode boxart: {e}");
			tracing::warn!("{message}");
			return bad_request(message);
		}

		let mut response = Response::new(Full::new(Bytes::from(buffer.into_inner())));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
		response
	}

	async fn server_info(
		&self,
		params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<&String>,
	) -> Response<Full<Bytes>> {
		let session_context = match self.session_manager.get_session_context().await {
			Ok(session_context) => session_context,
			Err(()) => {
				let message = "Failed to get session context".to_string();
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		// Seems we should only say we paired when using HTTPS.
		let paired = if https {
			match params.get("uniqueid") {
				Some(unique_id) if self.client_manager.is_paired(unique_id.clone()).unwrap_or(false) => "1",
				Some(_) | None => "0",
			}
		} else {
			"0"
		};

		let local_ip = local_address.map(|addr| addr.ip().to_string()).unwrap_or_default();

		// TODO: Check the use of some of these values, we leave most of them blank and Moonlight doesn't care.
		let mut response = "<root status_code=\"200\">".to_string();
		response += &format!("<hostname>{}</hostname>", escape_xml(&self.name));
		response += &format!("<appversion>{}</appversion>", SERVERINFO_APP_VERSION);
		response += &format!("<GfeVersion>{}</GfeVersion>", SERVERINFO_GFE_VERSION);
		response += &format!("<uniqueid>{}</uniqueid>", self.unique_id);
		response += &format!("<HttpsPort>{}</HttpsPort>", self.webserver_config.port_https);
		response += &format!("<ExternalPort>{}</ExternalPort>", self.webserver_config.port);
		response += &format!("<mac>{}</mac>", mac_address.unwrap_or("".to_string()));
		response += "<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>"; // TODO: Check if HEVC is supported, set this to 0 if it is not.
		response += &format!("<LocalIP>{}</LocalIP>", escape_xml(local_ip));
		// HDR444 alias for record clients; native orthogonal HDR bit remains.
		let server_codec_mode_support = self.supported_codecs
			| if self.supported_codecs & 0x03000000 == 0x03000000 {
				0x04000000
			} else {
				0
			};
		response += &format!(
			"<ServerCodecModeSupport>{}</ServerCodecModeSupport>",
			server_codec_mode_support
		);
		if https
			&& peer_cert_fingerprint.is_some_and(|fp| self.verify_paired_client(&Some(fp.clone())).is_none())
			&& self.supported_codecs & 0x01800000 != 0
		{
			let speed = local_address
				.map(|a| bandwidth::routed_link_mbps(a.ip(), peer_address.ip()))
				.unwrap_or(0);
			response += &format!(
				"<PyroWaveHostLinkMbps>{speed}</PyroWaveHostLinkMbps><PyroWaveBandwidthProbeBytes>{}</PyroWaveBandwidthProbeBytes>",
				bandwidth::PROBE_BYTES
			);
		}
		response += "<SupportedDisplayMode></SupportedDisplayMode>";
		response += &format!("<PairStatus>{paired}</PairStatus>");
		response += &format!(
			"<currentgame>{}</currentgame>",
			session_context.clone().map(|s| s.application_id).unwrap_or(0)
		);
		response += &format!(
			"<state>{}</state>",
			session_context
				.map(|_| "MOONSHINE_SERVER_BUSY")
				.unwrap_or("MOONSHINE_SERVER_FREE")
		);
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	fn pin(&self, params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let unique_id = params
			.get("uniqueid")
			.cloned()
			.map(|id| {
				id.chars()
					.filter(|c| c.is_ascii_hexdigit())
					.take(16)
					.collect::<String>()
			})
			.filter(|id| !id.is_empty())
			.unwrap_or_else(|| "0123456789ABCDEF".to_string());

		// The page approves one specific pending request: its approval token is
		// required on submission, so a request that replaces it (same client ID)
		// cannot inherit the operator's PIN. Show the requester so the operator
		// can recognize it.
		let Some(pending) = self.client_manager.pending_approval(&unique_id) else {
			return Response::builder()
				.status(StatusCode::NOT_FOUND)
				.header(header::CACHE_CONTROL, "no-store")
				.body(Full::new(Bytes::from("No pending pairing request for this client.")))
				.unwrap();
		};
		let content = include_bytes!("../../../assets/pin.html");
		let html = String::from_utf8_lossy(content);
		let html = html
			.replace("{{UNIQUE_ID}}", &unique_id)
			.replace("{{REQUEST}}", &pending.approval)
			.replace("{{REQUESTER}}", &escape_xml(pending.requester.to_string()))
			.replace(
				"{{FINGERPRINT}}",
				&escape_xml(pending.fingerprint.as_deref().unwrap_or("unknown")),
			);
		let mut response = Response::new(Full::new(Bytes::from(html)));
		let headers = response.headers_mut();
		headers.insert(
			header::CONTENT_TYPE,
			HeaderValue::from_static("text/html; charset=UTF-8"),
		);
		headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
		headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
		headers.insert(
			header::CONTENT_SECURITY_POLICY,
			HeaderValue::from_static("frame-ancestors 'none'"),
		);
		headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));

		response
	}

	async fn submit_pin(&self, request: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
		// Enforce a hard size limit and deadline while reading the body.
		let body = Limited::new(request.into_body(), MAX_PIN_BODY_BYTES).collect();
		let body = match tokio::time::timeout(self.limits.body_read_timeout, body).await {
			Ok(Ok(body)) => body.to_bytes(),
			Ok(Err(e)) => {
				tracing::warn!("Failed to read request body: {e}");
				return bad_request("Bad request.".to_string());
			},
			Err(_) => {
				tracing::warn!("Timed out reading PIN submission.");
				return bad_request("Bad request.".to_string());
			},
		};

		let params: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
		let (Some(unique_id), Some(pin), Some(approval)) =
			(params.get("uniqueid"), params.get("pin"), params.get("request"))
		else {
			tracing::warn!("PIN submission requires 'uniqueid', 'pin' and 'request'.");
			return bad_request("Bad request.".to_string());
		};

		match self.client_manager.register_pin(unique_id, pin, approval) {
			Ok(()) => {
				tracing::info!("PIN registered successfully.");
				Response::new(Full::new(Bytes::from("PIN accepted.")))
			},
			Err(()) => bad_request("Failed to register PIN.".to_string()),
		}
	}

	async fn unpair(&self, _params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let xml = r#"<root status_code="200"/>"#;
		let mut response = Response::new(Full::new(Bytes::from(xml)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	async fn launch(
		&self,
		mut params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
	) -> Response<Full<Bytes>> {
		let application_id = match params.remove("appid") {
			Some(application_id) => application_id,
			None => {
				let message = format!("Expected 'appid' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let application_id: i32 = match application_id.parse() {
			Ok(application_id) => application_id,
			Err(e) => {
				let message = format!("Failed to parse application ID: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		// Validate every negotiated value before the session is touched.
		let mode = match params.remove("mode") {
			Some(mode) => mode,
			None => {
				let message = format!("Expected 'mode' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let DisplayMode {
			width,
			height,
			refresh_rate,
		} = match DisplayMode::parse(&mode) {
			Ok(mode) => mode,
			Err(reason) => {
				let message = format!("Invalid mode in launch request: {reason}.");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let keys = match request_keys(&mut params, "launch") {
			Ok(keys) => keys,
			Err(message) => {
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		// TODO: localAudioPlayMode (host_audio) is not yet supported with the
		// per-session PulseServer approach.

		// Default: stereo (0x30002).
		let (audio_channels, audio_channel_mask) = match params.remove("surroundAudioInfo") {
			None => (AudioChannels::Stereo, 0x3),
			Some(value) => match negotiation::surround_audio_info(&value) {
				Ok(audio) => audio,
				Err(reason) => {
					let message = format!("Invalid launch request: {reason}.");
					tracing::warn!("{message}");
					return xml_error(400, &message);
				},
			},
		};

		let hdr = match params.remove("hdrMode").map(|value| value.parse::<u32>()) {
			None => false,
			Some(Ok(value)) => value != 0,
			Some(Err(error)) => return xml_error(400, &format!("Invalid hdrMode in launch request: {error}.")),
		};

		let application = match self.applications.iter().find(|&a| a.id() == application_id) {
			Some(application) => application,
			None => {
				let message = format!("Couldn't find application with ID {}.", application_id - 1);
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let initialize_result = self
			.session_manager
			.initialize_session(SessionContext {
				application: application.clone(),
				application_id,
				resolution: (width, height),
				refresh_rate,
				keys: SessionKeys::Keys(keys),
				audio_channels,
				audio_channel_mask,
				hdr,
				client_ip: peer_address.ip(),
			})
			.await;

		if initialize_result.is_err() {
			return xml_error(400, "Failed to start session");
		}

		match tokio::time::timeout(
			std::time::Duration::from_secs(APP_LAUNCH_HTTP_TIMEOUT_SECS),
			self.session_manager.launch_session(),
		)
		.await
		{
			Ok(Ok(())) => {},
			Ok(Err(())) => {
				let _ = self.session_manager.stop_session().await;
				return xml_error(
					503,
					"Application failed to start (check Moonshine logs for more information).",
				);
			},
			Err(_) => {
				tracing::error!("Timed out waiting for application launch result.");
				// Clean up the partially-initialized session to allow retries.
				let _ = self.session_manager.stop_session().await;
				return xml_error(
					503,
					"Application failed to start (check Moonshine logs for more information).",
				);
			},
		}

		let mut response = "<root status_code=\"200\">".to_string();
		response += "<gamesession>1</gamesession>";
		if let Some(addr) = local_address {
			response += &format!(
				"<sessionUrl0>rtsp://{}:{}</sessionUrl0>",
				rtsp_host(addr.ip()),
				self.rtsp_port
			);
		}
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));

		response
	}

	async fn resume(
		&self,
		mut params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
	) -> Response<Full<Bytes>> {
		// The whole request is validated before the session manager publishes
		// keys or rotates authorization, so a malformed resume changes nothing.
		let keys = match request_keys(&mut params, "resume") {
			Ok(keys) => keys,
			Err(message) => {
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let mut resume_request = crate::session::ResumeRequest::default();
		if let Some(mode) = params.remove("mode") {
			match DisplayMode::parse(&mode) {
				Ok(mode) => {
					resume_request.resolution = Some((mode.width, mode.height));
					resume_request.refresh_rate = Some(mode.refresh_rate);
				},
				Err(reason) => return xml_error(400, &format!("Invalid mode in resume request: {reason}.")),
			}
		}
		if let Some(hdr_mode) = params.remove("hdrMode") {
			match hdr_mode.parse::<u32>() {
				Ok(value) => resume_request.hdr = Some(value != 0),
				Err(error) => return xml_error(400, &format!("Invalid hdrMode in resume request: {error}.")),
			}
		}
		if let Some(surround_audio_info) = params.remove("surroundAudioInfo") {
			match negotiation::surround_audio_info(&surround_audio_info) {
				Ok((channels, mask)) => {
					resume_request.audio_channels = Some(channels);
					resume_request.audio_channel_mask = Some(mask);
				},
				Err(reason) => return xml_error(400, &format!("Invalid resume request: {reason}.")),
			}
		}

		match self
			.session_manager
			.resume_session(keys, resume_request, peer_address.ip())
			.await
		{
			Ok(()) => {},
			Err(()) => {
				let message = "Failed to update session keys".to_string();
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		}

		let mut response = "<root status_code=\"200\">".to_string();
		if let Some(addr) = local_address {
			response += &format!(
				"<sessionUrl0>rtsp://{}:{}</sessionUrl0>",
				rtsp_host(addr.ip()),
				self.rtsp_port
			);
		}
		response += "<resume>1</resume>";
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));

		response
	}

	async fn cancel(&self) -> Response<Full<Bytes>> {
		if self.session_manager.stop_session().await.is_err() {
			let message = "Failed to stop session".to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		}

		let mut response = "<root status_code=\"200\">".to_string();
		response += "<cancel>1</cancel>";
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	/// Operator approval is a host-local administrative action served on the
	/// client-reachable HTTP listener; reject anything else with 403.
	fn verify_operator(
		&self,
		request: &Request<hyper::body::Incoming>,
		peer_address: SocketAddr,
	) -> Option<Response<Full<Bytes>>> {
		let reason = authorize_operator_request(peer_address, request.headers(), self.webserver_config.port).err()?;
		tracing::warn!(%peer_address, reason, "Rejected pairing approval request");
		Some(
			Response::builder()
				.status(StatusCode::FORBIDDEN)
				.body(Full::new(Bytes::from(
					"Pairing approval is only available from the host itself (http://localhost).",
				)))
				.unwrap(),
		)
	}

	/// Verify that the connecting client has presented a TLS certificate
	/// that belongs to a paired client. Returns `None` if authorized,
	/// or `Some(response)` with a 401 response if not.
	fn verify_paired_client(&self, peer_cert_fingerprint: &Option<String>) -> Option<Response<Full<Bytes>>> {
		match peer_cert_fingerprint {
			Some(fingerprint) => match self.client_manager.is_cert_paired(fingerprint) {
				Ok(true) => None,
				Ok(false) => {
					tracing::warn!("Client certificate not recognized (fingerprint: {fingerprint})");
					Some(unauthorized("Client certificate is not from a paired client."))
				},
				Err(()) => Some(bad_request("Failed to verify client certificate.".to_string())),
			},
			None => {
				tracing::warn!("No client certificate provided for protected endpoint.");
				Some(unauthorized("No client certificate provided."))
			},
		}
	}
}

/// Authorize a request to the operator PIN approval routes.
///
/// - The peer must be loopback: only the host operator may approve pairing.
///   First pairing cannot rely on a paired client certificate.
/// - `Host` must name a loopback host on our port, defeating DNS rebinding of
///   an attacker domain to 127.0.0.1.
/// - Browser metadata, when present, must describe a same-origin request on a
///   loopback origin, so another website cannot submit a PIN through the
///   operator's browser (CSRF). Non-browser clients (curl) send neither header.
fn authorize_operator_request(peer: SocketAddr, headers: &header::HeaderMap, port: u16) -> Result<(), &'static str> {
	if !peer.ip().to_canonical().is_loopback() {
		return Err("non-loopback peer");
	}
	let host = headers
		.get(header::HOST)
		.and_then(|value| value.to_str().ok())
		.ok_or("missing Host")?;
	if !is_loopback_authority(host, port) {
		return Err("non-loopback Host");
	}
	if let Some(origin) = headers.get(header::ORIGIN) {
		let origin = origin.to_str().map_err(|_| "invalid Origin")?;
		let authority = origin.strip_prefix("http://").ok_or("cross-origin request")?;
		if !is_loopback_authority(authority, port) {
			return Err("cross-origin request");
		}
	}
	if let Some(site) = headers.get("sec-fetch-site")
		&& !matches!(site.as_bytes(), b"same-origin" | b"none")
	{
		return Err("cross-site request");
	}
	Ok(())
}

/// Whether `authority` (`host[:port]`) names a loopback host on `port`.
fn is_loopback_authority(authority: &str, port: u16) -> bool {
	let (host, authority_port) = match authority.strip_prefix('[') {
		Some(bracketed) => match bracketed.split_once(']') {
			Some((host, "")) => (host, None),
			Some((host, rest)) => match rest.strip_prefix(':') {
				Some(port) => (host, Some(port)),
				None => return false,
			},
			None => return false,
		},
		None => match authority.rsplit_once(':') {
			Some((host, port)) => (host, Some(port)),
			None => (authority, None),
		},
	};
	let port_matches = match authority_port {
		Some(authority_port) => authority_port.parse::<u16>().is_ok_and(|value| value == port),
		None => port == 80,
	};
	let loopback = host.eq_ignore_ascii_case("localhost")
		|| host.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback());
	port_matches && loopback
}

fn resolve_listen_address(address: &str, port: u16) -> Result<SocketAddr, ()> {
	(address, port)
		.to_socket_addrs()
		.map_err(|e| tracing::error!("Failed to resolve address '{address}' port {port}: {e}"))?
		.next()
		.ok_or_else(|| tracing::error!("Failed to resolve address '{address}' port {port}"))
}

/// Address of the loopback listener for operator PIN approval, needed only when
/// the configured HTTP listener cannot accept loopback connections.
fn operator_listen_address(http_address: SocketAddr) -> Option<SocketAddr> {
	let ip = http_address.ip();
	if ip.is_unspecified() || ip.to_canonical().is_loopback() {
		return None;
	}
	Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_address.port()))
}

/// Bind a TCP listener for the webserver. When the address is IPv6 we disable
/// `IPV6_V6ONLY` so the single socket also accepts IPv4-mapped connections. This
/// lets clients reach us over whichever family mDNS advertised (avahi publishes
/// both an A and AAAA record), avoiding the "online/offline" flip-flop that
/// happens when one family has no listener.
fn bind_listener(address: SocketAddr) -> std::io::Result<TcpListener> {
	let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
	if address.is_ipv6() {
		socket.set_only_v6(false)?;
	}
	socket.set_reuse_address(true)?;
	socket.bind(&address.into())?;
	socket.listen(1024)?;
	socket.set_nonblocking(true)?;
	TcpListener::from_std(socket.into())
}

/// Collapse an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) back to plain IPv4.
/// Connections arriving over IPv4 on a dual-stack socket report such an address;
/// normalizing keeps MAC lookups and session URLs using the real IPv4 address.
fn unmap_v4_mapped(addr: SocketAddr) -> SocketAddr {
	match addr.ip() {
		IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
			Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
			None => addr,
		},
		IpAddr::V4(_) => addr,
	}
}

/// Format an IP address for use as the host part of an RTSP URL. IPv6 addresses
/// must be wrapped in brackets (`rtsp://[::1]:48010`) to be a valid URL.
fn rtsp_host(ip: IpAddr) -> String {
	match ip {
		IpAddr::V4(v4) => v4.to_string(),
		IpAddr::V6(v6) => format!("[{v6}]"),
	}
}

fn bad_request(message: String) -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Full::new(Bytes::from(message)))
		.unwrap()
}

/// Validate the `rikey`/`rikeyid` pair of a launch/resume request. The error
/// is a client-facing message that never contains key material.
fn request_keys(params: &mut HashMap<String, String>, request: &str) -> Result<SessionKeyData, String> {
	let (Some(key), Some(key_id)) = (params.remove("rikey"), params.remove("rikeyid")) else {
		return Err(format!("Expected 'rikey' and 'rikeyid' in {request} request."));
	};
	SessionKeyData::from_params(&key, &key_id)
		.map_err(|error| format!("Invalid key parameters in {request} request: {error}."))
}

fn xml_error(status_code: u16, message: &str) -> Response<Full<Bytes>> {
	// Always return HTTP 200 so that Moonlight (Qt) reads the response body.
	// Qt treats HTTP 4xx/5xx as network errors and never reads the body,
	// so the XML error would be invisible to the client.
	// The actual status code is embedded in the XML body for Moonlight to parse.
	let body = format!(
		"<root status_code=\"{status_code}\" status_message=\"{}\"></root>",
		escape_xml(message)
	);
	match Response::builder()
		.status(StatusCode::OK)
		.body(Full::new(Bytes::from(body)))
	{
		Ok(mut response) => {
			response
				.headers_mut()
				.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
			response
		},
		Err(e) => {
			tracing::error!("Failed to build error response: {e}");
			bad_request("Failed to build error response.".to_string())
		},
	}
}

const BOXART_WIDTH: u32 = 600;
const BOXART_HEIGHT: u32 = 801;

fn fit_to_boxart(asset: image::DynamicImage) -> image::DynamicImage {
	let (w, h) = (asset.width(), asset.height());

	// Already the right aspect ratio (within a small tolerance), return as-is.
	let target_ratio = BOXART_WIDTH as f64 / BOXART_HEIGHT as f64;
	let image_ratio = w as f64 / h as f64;
	if (image_ratio - target_ratio).abs() < 0.01 {
		return asset;
	}

	// Scale the image to fit within the box art dimensions while preserving aspect ratio.
	let scale = f64::min(BOXART_WIDTH as f64 / w as f64, BOXART_HEIGHT as f64 / h as f64);
	let new_w = (w as f64 * scale).round() as u32;
	let new_h = (h as f64 * scale).round() as u32;
	let resized = asset.resize_exact(new_w, new_h, FilterType::Lanczos3);

	// Center the resized image on a transparent canvas.
	let mut canvas = image::RgbaImage::new(BOXART_WIDTH, BOXART_HEIGHT);
	let offset_x = (BOXART_WIDTH - new_w) / 2;
	let offset_y = (BOXART_HEIGHT - new_h) / 2;
	image::imageops::overlay(&mut canvas, &resized.to_rgba8(), offset_x as i64, offset_y as i64);

	image::DynamicImage::ImageRgba8(canvas)
}

fn unauthorized(message: &str) -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::UNAUTHORIZED)
		.body(Full::new(Bytes::from(message.to_string())))
		.unwrap()
}

fn not_found() -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body(Full::new(Bytes::from("NOT FOUND")))
		.unwrap()
}

fn get_mac_address(address: IpAddr) -> Result<Option<String>, ()> {
	let interfaces = network_interface::NetworkInterface::show()
		.map_err(|e| tracing::warn!("Failed to retrieve network interfaces: {e}"))?;

	for interface in interfaces {
		for interface_address in interface.addr {
			if interface_address.ip() == address {
				tracing::debug!(
					"Found MAC address for address {:?}: {:?}",
					address,
					interface.mac_addr.as_ref().unwrap_or(&"None".to_string())
				);
				return Ok(interface.mac_addr);
			}
		}
	}

	tracing::warn!("No interface found matching address {:?}", address);
	Ok(None)
}

fn escape_xml(input: impl AsRef<str>) -> String {
	input
		.as_ref()
		.replace("&", "&amp;")
		.replace("<", "&lt;")
		.replace(">", "&gt;")
		.replace("\"", "&quot;")
		.replace("'", "&apos;")
}
