use std::{
	collections::HashMap,
	convert::Infallible,
	net::{IpAddr, SocketAddr, ToSocketAddrs},
	path::PathBuf,
	str::FromStr,
};

use async_shutdown::ShutdownManager;
use http_body_util::Full;
use hyper::{
	body::Bytes,
	header::{self, HeaderValue},
	service::service_fn,
	Method, Request, Response, StatusCode,
};
use hyper_util::rt::tokio::TokioIo;
use image::ImageFormat;
use network_interface::NetworkInterfaceConfig;
use tokio::net::TcpListener;

use crate::{
	clients::ClientManager,
	config::Config,
	session::{manager::SessionManager, SessionContext, SessionKeys},
	webserver::tls::TlsAcceptor,
};

use self::pairing::handle_pair_request;

mod pairing;
mod tls;

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
		shutdown: ShutdownManager<i32>,
	) -> Result<Self, ()> {
		let server = Self {
			config: config.clone(),
			unique_id,
			client_manager,
			session_manager,
			server_certs,
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
					.wrap_cancel(shutdown.wrap_trigger_shutdown(1, async move {
						let listener = TcpListener::bind(http_address)
							.await
							.map_err(|e| tracing::error!("Failed to bind to address {http_address}: {e}"))?;

						tracing::debug!("HTTP server listening for connections on {http_address}");
						loop {
							let (connection, address) = listener
								.accept()
								.await
								.map_err(|e| tracing::error!("Failed to accept connection: {e}"))?;
							tracing::trace!("Accepted connection from {address}.");

							let address = connection.local_addr().ok();
							let mac_address = if let Some(address) = address {
								get_mac_address(address.ip()).unwrap_or(None)
							} else {
								None
							};

							let io = TokioIo::new(connection);

							tokio::spawn({
								let server = server.clone();
								async move {
									let _ = hyper::server::conn::http1::Builder::new()
										.serve_connection(
											io,
											service_fn(|request| {
												server.serve(request, address, mac_address.clone(), false)
											}),
										)
										.await;
								}
							});
						}

						// Is there another way to define the return type of this function?
						#[allow(unreachable_code)]
						Ok::<(), ()>(())
					}))
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
					.wrap_cancel(shutdown.wrap_trigger_shutdown(2, async move {
						let listener = TcpListener::bind(https_address)
							.await
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

							let address = connection.local_addr().ok();
							let mac_address = if let Some(address) = address {
								get_mac_address(address.ip()).unwrap_or(None)
							} else {
								None
							};

							let connection = match acceptor.accept(connection).await {
								Ok(connection) => connection,
								Err(()) => continue,
							};

							let io = TokioIo::new(connection);

							tokio::spawn({
								let server = server.clone();
								async move {
									let _ = hyper::server::conn::http1::Builder::new()
										.serve_connection(
											io,
											service_fn(|request| {
												server.serve(request, address, mac_address.clone(), true)
											}),
										)
										.await;
								}
							});
						}

						// Is there another way to define the return type of this function?
						#[allow(unreachable_code)]
						Ok::<(), ()>(())
					}))
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
				(&Method::GET, "/applist") => self.app_list(),
				(&Method::GET, "/appasset") => self.app_asset(params),
				(&Method::GET, "/pair") => {
					handle_pair_request(request, params, local_address, &self.server_certs, &self.client_manager).await
				},
				// (&Method::GET, "/unpair") => self.unpair(params).await,
				(&Method::GET, "/launch") => self.launch(params).await,
				(&Method::GET, "/resume") => self.resume(params).await,
				(&Method::GET, "/cancel") => self.cancel().await,
				(method, uri) => {
					tracing::warn!("Unhandled {method} request with URI '{uri}'");
					not_found()
				},
			}
		} else {
			match (request.method(), request.uri().path()) {
				(&Method::GET, "/serverinfo") => self.server_info(params, mac_address, https).await,
				(&Method::GET, "/pair") => {
					handle_pair_request(request, params, local_address, &self.server_certs, &self.client_manager).await
				},
				(&Method::GET, "/pin") => self.pin().await,
				(&Method::GET, "/submit-pin") => self.submit_pin(params).await,
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

			// TODO: Fix HDR support.
			response += "<IsHdrSupported>0</IsHdrSupported>";
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
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id.clone(),
			None => {
				let message = format!("Expected 'uniqueid' in /serverinfo request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

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
			if self.client_manager.is_paired(unique_id).await.unwrap_or(false) {
				"1"
			} else {
				"0"
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
			| (ServerCodecModeSupport::HevcRext10444 as u32);
			// | (ServerCodecModeSupport::Av1Main8 as u32)
			// | (ServerCodecModeSupport::Av1High8444 as u32)
			// | (ServerCodecModeSupport::Av1Main10 as u32)
			// | (ServerCodecModeSupport::Av1High10444 as u32);
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

	async fn pin(&self) -> Response<Full<Bytes>> {
		let content = include_bytes!("../../assets/pin.html");
		let mut response = Response::new(Full::new(Bytes::from_static(content)));
		response.headers_mut().insert(
			header::CONTENT_TYPE,
			HeaderValue::from_static("text/html; charset=UTF-8"),
		);

		response
	}

	async fn submit_pin(&self, params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let pin = match params.get("pin") {
			Some(pin) => pin,
			None => {
				let message = format!("Expected 'pin' in pin request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let response = self.client_manager.register_pin(unique_id, pin).await;
		match response {
			Ok(()) => match Response::builder()
				.status(StatusCode::OK)
				.body(Full::new(Bytes::from(format!(
					"Successfully received pin '{}' for unique id '{}'.",
					pin, unique_id
				)))) {
				Ok(response) => response,
				Err(e) => {
					let message = format!("Failed to create '/pin' response: {e}");
					tracing::warn!("{message}");
					bad_request(message)
				},
			},
			Err(()) => bad_request("Failed to register pin".to_string()),
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

	async fn launch(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let unique_id = match params.remove("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		match self.client_manager.is_paired(unique_id).await {
			Ok(paired) => paired,
			Err(()) => return bad_request("Failed to check client paired status".to_string()),
		};

		let application_id = match params.remove("appid") {
			Some(application_id) => application_id,
			None => {
				let message = format!("Expected 'appid' in launch request, got {:?}.", params.keys());
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

		let mode = match params.remove("mode") {
			Some(mode) => mode,
			None => {
				let message = format!("Expected 'mode' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let mode_parts: Vec<&str> = mode.split('x').collect();
		if mode_parts.len() != 3 {
			let message = format!("Expected mode in format WxHxR, but got '{mode}'.");
			tracing::warn!("{message}");
			return bad_request(message);
		}
		let width: u32 = match mode_parts[0].parse() {
			Ok(width) => width,
			Err(e) => {
				let message = format!("Failed to parse width: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let height: u32 = match mode_parts[1].parse() {
			Ok(height) => height,
			Err(e) => {
				let message = format!("Failed to parse height: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let refresh_rate: u32 = match mode_parts[2].parse() {
			Ok(refresh_rate) => refresh_rate,
			Err(e) => {
				let message = format!("Failed to parse refresh rate: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let remote_input_key = match params.remove("rikey") {
			Some(remote_input_key) => remote_input_key,
			None => {
				let message = format!("Expected 'rikey' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let remote_input_key = match hex::decode(remote_input_key) {
			Ok(remote_input_key) => remote_input_key,
			Err(e) => {
				let message = format!("Failed to decode remote input key: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let remote_input_key_id: String = match params.remove("rikeyid") {
			Some(remote_input_key_id) => remote_input_key_id,
			None => {
				let message = format!("Expected 'rikey_id' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let remote_input_key_id: i64 = match remote_input_key_id.parse() {
			Ok(remote_input_key_id) => remote_input_key_id,
			Err(e) => {
				let message =
					format!("Couldn't parse 'rikey_id' in launch request, got '{remote_input_key_id}' with error: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let host_audio = match params.remove("localAudioPlayMode") {
			Some(host_audio) => host_audio == "1",
			None => false,
		};

		let application = match self.config.applications.iter().find(|&a| a.id() == application_id) {
			Some(application) => application,
			None => {
				let message = format!("Couldn't find application with ID {}.", application_id - 1);
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let initialize_result = self
			.session_manager
			.initialize_session(SessionContext {
				application: application.clone(),
				application_id,
				resolution: (width, height),
				_refresh_rate: refresh_rate,
				keys: SessionKeys {
					remote_input_key,
					remote_input_key_id,
				},
				host_audio,
			})
			.await;

		if initialize_result.is_err() {
			return bad_request("Failed to start session".to_string());
		}

		let mut response = "<root status_code=\"200\">".to_string();
		response += "<gamesession>1</gamesession>";
		response += "</root>";

		// TODO: Return sessionUrl0.

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));

		response
	}

	async fn resume(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let unique_id = match params.remove("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in resume request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		match self.client_manager.is_paired(unique_id).await {
			Ok(paired) => paired,
			Err(()) => return bad_request("Failed to check client paired status".to_string()),
		};

		let remote_input_key = match params.remove("rikey") {
			Some(remote_input_key) => remote_input_key,
			None => {
				let message = format!("Expected 'rikey' in resume request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let remote_input_key = match hex::decode(remote_input_key) {
			Ok(remote_input_key) => remote_input_key,
			Err(e) => {
				let message = format!("Failed to decode remote input key: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let remote_input_key_id: String = match params.remove("rikeyid") {
			Some(remote_input_key_id) => remote_input_key_id,
			None => {
				let message = format!("Expected 'rikey_id' in resume request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let remote_input_key_id: i64 = match remote_input_key_id.parse() {
			Ok(remote_input_key_id) => remote_input_key_id,
			Err(e) => {
				let message =
					format!("Couldn't parse 'rikey_id' in resume request, got '{remote_input_key_id}' with error: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let update_result = self
			.session_manager
			.update_keys(SessionKeys {
				remote_input_key,
				remote_input_key_id,
			})
			.await;
		if update_result.is_err() {
			return bad_request("Failed to update session keys".to_string());
		}

		let mut response = "<root status_code=\"200\">".to_string();

		// TODO: Return sessionUrl0.

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
}

fn bad_request(message: String) -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Full::new(Bytes::from(message)))
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
		.map_err(|e| tracing::error!("Failed to retrieve network interfaces: {e}"))?;

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
