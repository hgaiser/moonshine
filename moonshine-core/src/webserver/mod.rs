use std::{
	collections::HashMap,
	convert::Infallible,
	net::{IpAddr, SocketAddr, ToSocketAddrs},
	path::PathBuf,
	str::FromStr,
};

use crate::ShutdownReason;
use async_shutdown::ShutdownManager;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
	body::Bytes,
	header::{self, HeaderValue},
	service::service_fn,
	Method, Request, Response, StatusCode,
};
use hyper_util::rt::tokio::TokioIo;
use image::imageops::FilterType;
use image::ImageFormat;
use network_interface::NetworkInterfaceConfig;
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::{
	clients::ClientManager,
	config::{Config, StreamUseIpv6},
	session::{manager::SessionManager, SessionContext, SessionKeyData, SessionKeys, APP_LAUNCH_HTTP_TIMEOUT_SECS},
	tls::TlsAcceptor,
};

use self::pairing::handle_pair_request;

mod pairing;

// The negative fourth value is to indicate that we are following the protocol introduced with Sunshine.
const SERVERINFO_APP_VERSION: &str = "7.1.431.-1";
const SERVERINFO_GFE_VERSION: &str = "3.23.0.74";

#[repr(u32)]
#[allow(dead_code)]
enum ServerCodecModeSupport {
	H264 = 0x00000001,
	Hevc = 0x00000100,
	HevcMain10 = 0x00000200,
	Av1Main8 = 0x00010000,      // Sunshine extension
	Av1Main10 = 0x00020000,     // Sunshine extension
	H264High8444 = 0x00040000,  // Sunshine extension
	HevcRext8444 = 0x00080000,  // Sunshine extension
	HevcRext10444 = 0x00100000, // Sunshine extension
	Av1High8444 = 0x00200000,   // Sunshine extension
	Av1High10444 = 0x00400000,  // Sunshine extension
}

#[derive(Clone)]
pub struct Webserver {
	config: Config,
	unique_id: String,
	client_manager: ClientManager,
	session_manager: SessionManager,
	server_certs: String,
	hdr_supported: bool,
	shutdown: ShutdownManager<ShutdownReason>,
}

impl Webserver {
	#[allow(clippy::result_unit_err)]
	pub fn new(
		config: Config,
		unique_id: String,
		// Passing certificate content as string.
		server_certs: String,
		client_manager: ClientManager,
		session_manager: SessionManager,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		// Gate HDR advertisement on both the config flag and a runtime
		// GPU capability probe (10-bit or FP16 render formats).
		let hdr_supported = config.compositor.hdr && super::session::compositor::probe_hdr_support(&config.compositor);

		let server = Self {
			config: config.clone(),
			unique_id,
			client_manager,
			session_manager,
			server_certs,
			hdr_supported,
			shutdown: shutdown.clone(),
		};

		// Run HTTP webserver.
		let http_address = (config.address.clone(), config.webserver.port)
			.to_socket_addrs()
			.map_err(|e| {
				tracing::error!(
					"Failed to resolve address '{}:{}': {e}",
					config.address,
					config.webserver.port
				)
			})?
			.next()
			.ok_or_else(|| {
				tracing::error!(
					"Failed to resolve address '{}:{}'",
					config.address,
					config.webserver.port
				)
			})?;

		tokio::spawn({
			let server = server.clone();
			let shutdown = shutdown.clone();

			async move {
				let server = server.clone();
				let _ = shutdown
					.wrap_cancel(
						shutdown.wrap_trigger_shutdown(ShutdownReason::HttpShutdown, async move {
							let listener = bind_listener(http_address)
								.map_err(|e| tracing::error!("Failed to bind to address {http_address}: {e}"))?;

							tracing::debug!("HTTP server listening for connections on {http_address}");
							loop {
								let (connection, address) = listener
									.accept()
									.await
									.map_err(|e| tracing::error!("Failed to accept connection: {e}"))?;
								tracing::trace!("Accepted connection from {address}.");

								let address = connection.local_addr().ok().map(unmap_v4_mapped);
								let mac_address = if let Some(address) = address {
									get_mac_address(address.ip()).unwrap_or(None)
								} else {
									None
								};

								let io = TokioIo::new(connection);

								tokio::spawn({
									let server = server.clone();
									let shutdown = server.shutdown.clone();
									async move {
										let _ = shutdown
											.wrap_cancel(async move {
												let _ = hyper::server::conn::http1::Builder::new()
													.serve_connection(
														io,
														service_fn(|request| {
															server.serve(
																request,
																address,
																mac_address.clone(),
																false,
																None,
															)
														}),
													)
													.await;
											})
											.await;
									}
								});
							}

							// Is there another way to define the return type of this function?
							#[allow(unreachable_code)]
							Ok::<(), ()>(())
						}),
					)
					.await;

				tracing::debug!("HTTP server shutting down.");
			}
		});

		// Run HTTPS webserver.
		let https_address = (config.address.clone(), config.webserver.port_https)
			.to_socket_addrs()
			.map_err(|e| {
				tracing::error!(
					"Failed to resolve address '{}:{}': {e}",
					config.address,
					config.webserver.port_https
				)
			})?
			.next()
			.ok_or_else(|| {
				tracing::error!(
					"Failed to resolve address '{}:{}'",
					config.address,
					config.webserver.port_https
				)
			})?;

		tokio::spawn({
			let server = server.clone();
			async move {
				let _ = shutdown
					.wrap_cancel(
						shutdown.wrap_trigger_shutdown(ShutdownReason::HttpsShutdown, async move {
							let listener = bind_listener(https_address)
								.map_err(|e| tracing::error!("Failed to bind to address '{:?}': {e}", https_address))?;
							let acceptor =
								TlsAcceptor::from_config(config.webserver.certificate, config.webserver.private_key)?;

							tracing::debug!("HTTPS server listening for connections on {https_address}");
							loop {
								let (connection, address) = listener
									.accept()
									.await
									.map_err(|e| tracing::error!("Failed to accept connection: {e}"))?;
								tracing::trace!("Accepted TLS connection from {address}.");

								let address = connection.local_addr().ok().map(unmap_v4_mapped);
								let mac_address = if let Some(address) = address {
									get_mac_address(address.ip()).unwrap_or(None)
								} else {
									None
								};

								let connection = match acceptor.accept(connection).await {
									Ok(connection) => connection,
									Err(()) => continue,
								};

								// Extract peer certificate fingerprint from TLS connection for mTLS verification.
								let peer_cert_fingerprint = connection
									.get_ref()
									.1
									.peer_certificates()
									.and_then(|certs| certs.first())
									.map(|cert| hex::encode(Sha256::digest(cert.as_ref())));

								let io = TokioIo::new(connection);

								tokio::spawn({
									let server = server.clone();
									let shutdown = server.shutdown.clone();
									async move {
										let _ = shutdown
											.wrap_cancel(async move {
												let _ = hyper::server::conn::http1::Builder::new()
													.serve_connection(
														io,
														service_fn(|request| {
															server.serve(
																request,
																address,
																mac_address.clone(),
																true,
																peer_cert_fingerprint.clone(),
															)
														}),
													)
													.await;
											})
											.await;
									}
								});
							}

							// Is there another way to define the return type of this function?
							#[allow(unreachable_code)]
							Ok::<(), ()>(())
						}),
					)
					.await;

				tracing::debug!("HTTPS server shutting down.");
			}
		});

		Ok(server)
	}

	async fn serve(
		&self,
		request: Request<hyper::body::Incoming>,
		local_address: Option<SocketAddr>,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<String>,
	) -> Result<Response<Full<Bytes>>, Infallible> {
		let params = request
			.uri()
			.query()
			.map(|v| url::form_urlencoded::parse(v.as_bytes()).into_owned().collect())
			.unwrap_or_default();

		tracing::debug!("Received {} request for {}.", request.method(), request.uri().path());

		let response = if https {
			match (request.method(), request.uri().path()) {
				(&Method::GET, "/serverinfo") => self.server_info(params, mac_address, https).await,
				(&Method::GET, "/applist") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp);
					}
					self.app_list()
				},
				(&Method::GET, "/appasset") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp);
					}
					self.app_asset(params)
				},
				(&Method::GET, "/pair") => {
					if !self.config.webserver.enable_pairing {
						tracing::warn!("Pairing is disabled in configuration.");
						return Ok(bad_request("Pairing is disabled.".to_string()));
					}
					handle_pair_request(
						request,
						params,
						local_address,
						&self.server_certs,
						&self.client_manager,
						self.config.webserver.port,
						&self.shutdown,
					)
					.await
				},
				// (&Method::GET, "/unpair") => self.unpair(params).await,
				(&Method::GET, "/launch") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp);
					}
					self.launch(params, local_address).await
				},
				(&Method::GET, "/resume") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp);
					}
					self.resume(params, local_address).await
				},
				(&Method::GET, "/cancel") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint) {
						return Ok(resp);
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
				(&Method::GET, "/serverinfo") => self.server_info(params, mac_address, https).await,
				(&Method::GET, "/pair") => {
					if !self.config.webserver.enable_pairing {
						tracing::warn!("Pairing is disabled in configuration.");
						return Ok(bad_request("Pairing is disabled.".to_string()));
					}
					handle_pair_request(
						request,
						params,
						local_address,
						&self.server_certs,
						&self.client_manager,
						self.config.webserver.port,
						&self.shutdown,
					)
					.await
				},
				(&Method::GET, "/pin") => {
					if !self.config.webserver.enable_pairing {
						return Ok(bad_request("Pairing is disabled.".to_string()));
					}
					self.pin(params)
				},
				(&Method::POST, "/submit-pin") => {
					if !self.config.webserver.enable_pairing {
						return Ok(bad_request("Pairing is disabled.".to_string()));
					}
					self.submit_pin(request).await
				},
				(method, uri) => {
					tracing::warn!("Unhandled {method} request with URI '{uri}'");
					not_found()
				},
			}
		};

		Ok(response)
	}

	fn app_list(&self) -> Response<Full<Bytes>> {
		let mut response = "<root status_code=\"200\">".to_string();
		for application in self.config.applications.iter() {
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

		let application = match self.config.applications.iter().find(|&a| a.id() == application_id) {
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

		let asset = match image::open(&boxart_path) {
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
		mac_address: Option<String>,
		https: bool,
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

		// TODO: Check the use of some of these values, we leave most of them blank and Moonlight doesn't care.
		let mut response = "<root status_code=\"200\">".to_string();
		response += &format!("<hostname>{}</hostname>", escape_xml(&self.config.name));
		response += &format!("<appversion>{}</appversion>", SERVERINFO_APP_VERSION);
		response += &format!("<GfeVersion>{}</GfeVersion>", SERVERINFO_GFE_VERSION);
		response += &format!("<uniqueid>{}</uniqueid>", self.unique_id);
		response += &format!("<HttpsPort>{}</HttpsPort>", self.config.webserver.port_https);
		response += "<ExternalPort></ExternalPort>";
		response += &format!("<mac>{}</mac>", mac_address.unwrap_or("".to_string()));
		response += "<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>"; // TODO: Check if HEVC is supported, set this to 0 if it is not.
		response += "<LocalIP></LocalIP>";
		let server_codec_mode_support = (ServerCodecModeSupport::H264 as u32)
			| (ServerCodecModeSupport::H264High8444 as u32)
			| (ServerCodecModeSupport::Hevc as u32)
			| (ServerCodecModeSupport::HevcRext8444 as u32)
			| (ServerCodecModeSupport::HevcMain10 as u32)
			| (ServerCodecModeSupport::HevcRext10444 as u32)
			| (ServerCodecModeSupport::Av1Main8 as u32)
			| (ServerCodecModeSupport::Av1High8444 as u32)
			| (ServerCodecModeSupport::Av1Main10 as u32)
			| (ServerCodecModeSupport::Av1High10444 as u32);
		response += &format!(
			"<ServerCodecModeSupport>{}</ServerCodecModeSupport>",
			server_codec_mode_support
		);
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
		let content = include_bytes!("../../../assets/pin.html");
		let html = String::from_utf8_lossy(content);
		let html = html.replace("{{UNIQUE_ID}}", &unique_id);
		let mut response = Response::new(Full::new(Bytes::from(html)));
		response.headers_mut().insert(
			header::CONTENT_TYPE,
			HeaderValue::from_static("text/html; charset=UTF-8"),
		);

		response
	}

	async fn submit_pin(&self, request: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
		// Enforce a hard 1 KB limit while reading the body to reject oversized requests early.
		let body = match Limited::new(request.into_body(), 1024).collect().await {
			Ok(body) => body.to_bytes(),
			Err(e) => {
				tracing::warn!("Failed to read request body: {e}");
				return bad_request("Bad request.".to_string());
			},
		};

		let params: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();

		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				tracing::warn!("Missing 'uniqueid' in PIN submission.");
				return bad_request("Bad request.".to_string());
			},
		};

		let pin = match params.get("pin") {
			Some(pin) => pin,
			None => {
				tracing::warn!("Missing 'pin' in PIN submission.");
				return bad_request("Bad request.".to_string());
			},
		};

		let response = self.client_manager.register_pin(unique_id, pin);
		match response {
			Ok(()) => {
				tracing::info!("PIN registered successfully.");
				match Response::builder()
					.status(StatusCode::OK)
					.body(Full::new(Bytes::from("PIN accepted.")))
				{
					Ok(response) => response,
					Err(e) => {
						tracing::warn!("Failed to create response: {e}");
						bad_request("Bad request.".to_string())
					},
				}
			},
			Err(()) => bad_request("Failed to register PIN.".to_string()),
		}
	}

	// This is disabled, because all moonlight clients seem to share the same uniqueid.
	// This means that if we 'unpair', we unpair all moonlight clients.
	// TODO: Collaborate with moonlight to give clients a truly unique ID.
	// async fn unpair(
	// 	&self,
	// 	mut params: HashMap<String, String>,
	// ) -> Response<Full<Bytes>> {
	// 	let unique_id = match params.remove("uniqueid") {
	// 		Some(unique_id) => unique_id,
	// 		None => {
	// 			let message = format!("Expected 'uniqueid' in unpair request, got {:?}.", params.keys());
	// 			tracing::warn!("{message}");
	// 			return bad_request(message);
	// 		}
	// 	};

	// 	match self.client_manager.remove_client(&unique_id).await {
	// 		Ok(()) =>
	// 			Response::builder()
	// 				.status(StatusCode::OK)
	// 				.body(Full::new(Bytes::from("Successfully unpaired.".to_string())))
	// 				.unwrap(),
	// 		Err(()) => bad_request("Failed to remove client".to_string()),
	// 	}
	// }

	async fn launch(
		&self,
		mut params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
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

		let mode = match params.remove("mode") {
			Some(mode) => mode,
			None => {
				let message = format!("Expected 'mode' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let mode_parts: Vec<&str> = mode.split('x').collect();
		if mode_parts.len() != 3 {
			let message = format!("Expected mode in format WxHxR, but got '{mode}'.");
			tracing::warn!("{message}");
			return xml_error(400, &message);
		}
		let width: u32 = match mode_parts[0].parse() {
			Ok(width) => width,
			Err(e) => {
				let message = format!("Failed to parse width: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let height: u32 = match mode_parts[1].parse() {
			Ok(height) => height,
			Err(e) => {
				let message = format!("Failed to parse height: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let refresh_rate: u32 = match mode_parts[2].parse() {
			Ok(refresh_rate) => refresh_rate,
			Err(e) => {
				let message = format!("Failed to parse refresh rate: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let remote_input_key = match params.remove("rikey") {
			Some(remote_input_key) => remote_input_key,
			None => {
				let message = format!("Expected 'rikey' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let remote_input_key = match hex::decode(remote_input_key) {
			Ok(remote_input_key) => remote_input_key,
			Err(e) => {
				let message = format!("Failed to decode remote input key: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let remote_input_key_id: String = match params.remove("rikeyid") {
			Some(remote_input_key_id) => remote_input_key_id,
			None => {
				let message = format!("Expected 'rikey_id' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let remote_input_key_id: i64 = match remote_input_key_id.parse() {
			Ok(remote_input_key_id) => remote_input_key_id,
			Err(e) => {
				let message =
					format!("Couldn't parse 'rikey_id' in launch request, got '{remote_input_key_id}' with error: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		// TODO: localAudioPlayMode (host_audio) is not yet supported with the
		// per-session PulseServer approach.

		let surround_audio_info: u32 = params
			.remove("surroundAudioInfo")
			.and_then(|s| s.parse().ok())
			.unwrap_or(196610); // Default: stereo (0x30002)
		let audio_channels = super::session::stream::AudioChannels::from((surround_audio_info & 0xFFFF) as u8);
		let audio_channel_mask = surround_audio_info >> 16;

		let hdr_mode: u32 = params.remove("hdrMode").and_then(|s| s.parse().ok()).unwrap_or(0);
		let hdr = hdr_mode != 0;

		let application = match self.config.applications.iter().find(|&a| a.id() == application_id) {
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
				keys: SessionKeys::new(remote_input_key, remote_input_key_id),
				audio_channels,
				audio_channel_mask,
				hdr,
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
			let session_ip = session_url_ip(&self.config, addr);
			response += &format!(
				"<sessionUrl0>rtsp://{}:{}</sessionUrl0>",
				rtsp_host(session_ip),
				self.config.stream.port
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
	) -> Response<Full<Bytes>> {
		let remote_input_key = match params.remove("rikey") {
			Some(remote_input_key) => remote_input_key,
			None => {
				let message = format!("Expected 'rikey' in resume request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let remote_input_key = match hex::decode(remote_input_key) {
			Ok(remote_input_key) => remote_input_key,
			Err(e) => {
				let message = format!("Failed to decode remote input key: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let remote_input_key_id: String = match params.remove("rikeyid") {
			Some(remote_input_key_id) => remote_input_key_id,
			None => {
				let message = format!("Expected 'rikey_id' in resume request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let remote_input_key_id: i64 = match remote_input_key_id.parse() {
			Ok(remote_input_key_id) => remote_input_key_id,
			Err(e) => {
				let message =
					format!("Couldn't parse 'rikey_id' in resume request, got '{remote_input_key_id}' with error: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		match self
			.session_manager
			.update_keys(SessionKeyData {
				remote_input_key,
				remote_input_key_id,
			})
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
			let session_ip = session_url_ip(&self.config, addr);
			response += &format!(
				"<sessionUrl0>rtsp://{}:{}</sessionUrl0>",
				rtsp_host(session_ip),
				self.config.stream.port
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

fn session_url_ip(config: &Config, local_address: SocketAddr) -> IpAddr {
	let local_ip = local_address.ip();
	match config.stream_use_ipv6 {
		StreamUseIpv6::Auto => local_ip,
		StreamUseIpv6::No if local_ip.is_ipv4() => local_ip,
		StreamUseIpv6::Yes if local_ip.is_ipv6() => local_ip,
		StreamUseIpv6::No => interface_ip_for_family(local_ip, StreamUseIpv6::No)
			.or_else(|| first_interface_ip(StreamUseIpv6::No))
			.unwrap_or_else(|| {
				tracing::warn!("Configured stream_use_ipv6 is no, but no IPv4 address was found; using {local_ip}.");
				local_ip
			}),
		StreamUseIpv6::Yes => interface_ip_for_family(local_ip, StreamUseIpv6::Yes)
			.or_else(|| first_interface_ip(StreamUseIpv6::Yes))
			.unwrap_or_else(|| {
				tracing::warn!("Configured stream_use_ipv6 is yes, but no IPv6 address was found; using {local_ip}.");
				local_ip
			}),
	}
}

fn interface_ip_for_family(local_ip: IpAddr, stream_use_ipv6: StreamUseIpv6) -> Option<IpAddr> {
	let interfaces = network_interface::NetworkInterface::show().ok()?;
	for interface in interfaces {
		if interface.addr.iter().any(|address| address.ip() == local_ip) {
			return interface
				.addr
				.into_iter()
				.map(|address| address.ip())
				.find(|ip| ip_matches_stream_ipv6(*ip, stream_use_ipv6) && !ip.is_loopback());
		}
	}
	None
}

fn first_interface_ip(stream_use_ipv6: StreamUseIpv6) -> Option<IpAddr> {
	let interfaces = network_interface::NetworkInterface::show().ok()?;
	interfaces
		.into_iter()
		.flat_map(|interface| interface.addr)
		.map(|address| address.ip())
		.find(|ip| ip_matches_stream_ipv6(*ip, stream_use_ipv6) && !ip.is_loopback())
}

fn ip_matches_stream_ipv6(ip: IpAddr, stream_use_ipv6: StreamUseIpv6) -> bool {
	matches!(
		(ip, stream_use_ipv6),
		(IpAddr::V4(_), StreamUseIpv6::No) | (IpAddr::V6(_), StreamUseIpv6::Yes)
	)
}

fn bad_request(message: String) -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Full::new(Bytes::from(message)))
		.unwrap()
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
