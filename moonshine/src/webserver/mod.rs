use std::{net::{ToSocketAddrs, IpAddr}, collections::HashMap, convert::Infallible};

use async_shutdown::ShutdownManager;
use http_body_util::Full;
use hyper::{service::service_fn, Response, Request, body::Bytes, StatusCode, header, Method};
use hyper_util::rt::TokioIo;
use network_interface::NetworkInterfaceConfig;
use openssl::x509::X509;
use tokio::net::TcpListener;
use xml::{EmitterConfig, writer::XmlEvent};

use crate::{config::Config, clients::ClientManager, webserver::tls::TlsAcceptor, session::{manager::SessionManager, SessionContext, SessionKeys}};

use self::pairing::handle_pair_request;

mod pairing;
mod tls;

const SERVERINFO_APP_VERSION: &str = "7.1.450.0";
const SERVERINFO_GFE_VERSION: &str = "3.23.0.74";

#[derive(Clone)]
pub struct Webserver {
	config: Config,
	unique_id: String,
	client_manager: ClientManager,
	session_manager: SessionManager,
	server_certs: X509,
}

impl Webserver {
	#[allow(clippy::result_unit_err)]
	pub fn new(
		config: Config,
		unique_id: String,
		server_certs: X509,
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
		let http_address = (config.address.clone(), config.webserver.port).to_socket_addrs()
			.map_err(|e| log::error!("Failed to resolve address '{}:{}': {e}", config.address, config.webserver.port))?
			.next()
			.ok_or_else(|| log::error!("Failed to resolve address '{}:{}'", config.address, config.webserver.port))?;

		tokio::spawn({
			let server = server.clone();
			let shutdown = shutdown.clone();

			async move {
				let server = server.clone();
				let _ = shutdown.wrap_cancel(shutdown.wrap_trigger_shutdown(1, async move {
					let listener = TcpListener::bind(http_address).await
						.map_err(|e| log::error!("Failed to bind to address {http_address}: {e}"))?;

					log::info!("HTTP server listening for connections on {http_address}");
					loop {
						let (connection, address) = listener.accept().await
							.map_err(|e| log::error!("Failed to accept connection: {e}"))?;
						log::trace!("Accepted connection from {address}.");

						let mac_address = if let Ok(local_address) = connection.local_addr() {
							get_mac_address(local_address.ip()).unwrap_or(None)
						} else {
							None
						};

						let io = TokioIo::new(connection);

						tokio::spawn({
							let server = server.clone();
							async move {
								let _ = hyper::server::conn::http1::Builder::new()
									.serve_connection(io, service_fn(|request| {
										server.serve(request, mac_address.clone())
									})).await;
							}
						});
					}

					// Is there another way to define the return type of this function?
					#[allow(unreachable_code)]
					Ok::<(), ()>(())
				})).await;

				log::debug!("HTTP server shutting down.");
			}
		});

		// Run HTTPS webserver.
		let https_address = (config.address.clone(), config.webserver.port_https).to_socket_addrs()
			.map_err(|e| log::error!("Failed to resolve address '{}:{}': {e}", config.address, config.webserver.port_https))?
			.next()
			.ok_or_else(|| log::error!("Failed to resolve address '{}:{}'", config.address, config.webserver.port_https))?;

		tokio::spawn({
			let server = server.clone();
			async move {
				let _ = shutdown.wrap_cancel(shutdown.wrap_trigger_shutdown(2, async move {
					let listener = TcpListener::bind(https_address).await
						.map_err(|e| log::error!("Failed to bind to address '{:?}': {e}", https_address))?;
					let acceptor = TlsAcceptor::from_config(config.webserver.certificate_chain, config.webserver.private_key)?;

					log::info!("HTTPS server listening for connections on {https_address}");
					loop {
						let (connection, address) = listener.accept().await
							.map_err(|e| log::error!("Failed to accept connection: {e}"))?;
						log::trace!("Accepted TLS connection from {address}.");

						let mac_address = if let Ok(local_address) = connection.local_addr() {
							get_mac_address(local_address.ip()).unwrap_or(None)
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
									.serve_connection(io, service_fn(|request| {
										server.serve(request, mac_address.clone())
									})).await;
							}
						});
					}

					// Is there another way to define the return type of this function?
					#[allow(unreachable_code)]
					Ok::<(), ()>(())
				})).await;

				log::debug!("HTTPS server shutting down.");
			}
		});

		Ok(server)
	}

	async fn serve(&self, request: Request<hyper::body::Incoming>, mac_address: Option<String>) -> Result<Response<Full<Bytes>>, Infallible> {
		let params = request.uri()
			.query()
			.map(|v| {
				url::form_urlencoded::parse(v.as_bytes())
					.into_owned()
					.collect()
			})
			.unwrap_or_default();

		log::info!("Received {} request for {}.", request.method(), request.uri().path());

		let response = match (request.method(), request.uri().path()) {
			(&Method::GET, "/serverinfo") => self.server_info(params, mac_address).await,
			(&Method::GET, "/applist") => self.app_list(),
			(&Method::GET, "/pair") => handle_pair_request(params, &self.server_certs, &self.client_manager).await,
			(&Method::GET, "/pin") => self.pin(params).await,
			(&Method::GET, "/unpair") => self.unpair(params).await,
			(&Method::GET, "/launch") => self.launch(params).await,
			(&Method::GET, "/resume") => self.resume(params).await,
			(&Method::GET, "/cancel") => self.cancel().await,
			(method, uri) => {
				log::warn!("Unhandled {method} request with URI '{uri}'");
				not_found()
			}
		};

		Ok(response)
	}

	fn app_list(&self) -> Response<Full<Bytes>> {
		let mut buffer = Vec::new();
		let mut writer = EmitterConfig::new()
			.write_document_declaration(true)
			.create_writer(&mut buffer);

		writer.write(XmlEvent::start_element("root")
			.attr("status_code", "200")).unwrap();

		for (i, application) in self.config.applications.iter().enumerate() {
			writer.write(XmlEvent::start_element("App")).unwrap();

			// TODO: Fix HDR support.
			writer.write(XmlEvent::start_element("IsHdrSupported")).unwrap();
			writer.write(XmlEvent::characters("0")).unwrap();
			writer.write(XmlEvent::end_element()).unwrap();

			writer.write(XmlEvent::start_element("AppTitle")).unwrap();
			writer.write(XmlEvent::characters(&application.title)).unwrap();
			writer.write(XmlEvent::end_element()).unwrap();

			writer.write(XmlEvent::start_element("ID")).unwrap();
			writer.write(XmlEvent::characters(&(i + 1).to_string())).unwrap();
			writer.write(XmlEvent::end_element()).unwrap();

			// </App>
			writer.write(XmlEvent::end_element()).unwrap();
		}

		// </root>
		writer.write(XmlEvent::end_element()).unwrap();

		let mut response = Response::new(Full::new(Bytes::from(String::from_utf8(buffer).unwrap())));
		response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
		response
	}

	async fn server_info(
		&self,
		params: HashMap<String, String>,
		mac_address: Option<String>,
	) -> Response<Full<Bytes>> {
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id.clone(),
			None => {
				let message = format!("Expected 'uniqueid' in /serverinfo request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let session_context = match self.session_manager.get_session_context().await {
			Ok(session_context) => session_context,
			Err(()) => {
				let message = "Failed to get session context".to_string();
				log::warn!("{message}");
				return bad_request(message);
			},
		};

		let paired = self.client_manager.is_paired(unique_id).await.unwrap_or(false);
		let paired = if paired {
			"1"
		} else {
			"0"
		};

		let mut buffer = Vec::new();
		let mut writer = EmitterConfig::new()
			.write_document_declaration(true)
			.create_writer(&mut buffer);

		// TODO: Check the use of some of these values, we leave most of them blank and Moonlight doesn't care.
		writer.write(XmlEvent::start_element("root")
			.attr("status_code", "200")).unwrap();

		writer.write(XmlEvent::start_element("hostname")).unwrap();
		writer.write(XmlEvent::characters(&self.config.name)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("appversion")).unwrap();
		writer.write(XmlEvent::characters(SERVERINFO_APP_VERSION)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("GfeVersion")).unwrap();
		writer.write(XmlEvent::characters(SERVERINFO_GFE_VERSION)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("uniqueid")).unwrap();
		writer.write(XmlEvent::characters(&self.unique_id)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("HttpsPort")).unwrap();
		writer.write(XmlEvent::characters(&self.config.webserver.port_https.to_string())).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("ExternalPort")).unwrap();
		writer.write(XmlEvent::characters("")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("mac")).unwrap();
		writer.write(XmlEvent::characters(&mac_address.unwrap_or("".to_string()))).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("MaxLumaPixelsHEVC")).unwrap();
		writer.write(XmlEvent::characters("1869449984")).unwrap(); // TODO: Check if HEVC is supported, set this to 0 if it is not.
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("LocalIP")).unwrap();
		writer.write(XmlEvent::characters("")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("ServerCodecModeSupport")).unwrap();
		writer.write(XmlEvent::characters("259")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("SupportedDisplayMode")).unwrap();

		// for display_mode in display_modes { ... }

		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("PairStatus")).unwrap();
		writer.write(XmlEvent::characters(paired)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("currentgame")).unwrap();
		writer.write(XmlEvent::characters(&session_context.clone().map(|s| s.application_id).unwrap_or(0).to_string())).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("state")).unwrap();
		writer.write(XmlEvent::characters(session_context.map(|_| "MOONSHINE_SERVER_BUSY").unwrap_or("MOONSHINE_SERVER_FREE"))).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		// </root>
		writer.write(XmlEvent::end_element()).unwrap();

		let mut response = Response::new(Full::new(Bytes::from(String::from_utf8(buffer).unwrap())));
		response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
		response
	}

	async fn pin(
		&self,
		params: HashMap<String, String>,
	) -> Response<Full<Bytes>> {
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let pin = match params.get("pin") {
			Some(pin) => pin,
			None => {
				let message = format!("Expected 'pin' in pin request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let response = self.client_manager.register_pin(unique_id, pin).await;
		match response {
			Ok(()) =>
				Response::builder()
					.status(StatusCode::OK)
					.body(Full::new(Bytes::from(format!("Successfully received pin '{}' for unique id '{}'.", pin, unique_id)))).unwrap(),
			Err(()) =>
				bad_request("Failed to register pin".to_string()),
		}
	}

	async fn unpair(
		&self,
		mut params: HashMap<String, String>,
	) -> Response<Full<Bytes>> {
		let unique_id = match params.remove("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in unpair request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		match self.client_manager.remove_client(&unique_id).await {
			Ok(()) =>
				Response::builder()
					.status(StatusCode::OK)
					.body(Full::new(Bytes::from("Successfully unpaired.".to_string())))
					.unwrap(),
			Err(()) => bad_request("Failed to remove client".to_string()),
		}
	}

	async fn launch(
		&self,
		mut params: HashMap<String, String>,
	) -> Response<Full<Bytes>> {
		let unique_id = match params.remove("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in launch request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		match self.client_manager.is_paired(unique_id).await {
			Ok(paired) => paired,
			Err(()) => return bad_request("Failed to check client paired status".to_string()),
		};

		let application_id = match params.remove("appid") {
			Some(application_id) => application_id,
			None => {
				let message = format!("Expected 'appid' in launch request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let application_id: u32 = match application_id.parse() {
			Ok(application_id) => application_id,
			Err(e) => {
				let message = format!("Failed to parse application ID: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let mode = match params.remove("mode") {
			Some(mode) => mode,
			None => {
				let message = format!("Expected 'mode' in launch request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let mode_parts: Vec<&str> = mode.split('x').collect();
		if mode_parts.len() != 3 {
			let message = format!("Expected mode in format WxHxR, but got '{mode}'.");
			log::warn!("{message}");
			return bad_request(message);
		}
		let width: u32 = match mode_parts[0].parse() {
			Ok(width) => width,
			Err(e) => {
				let message = format!("Failed to parse width: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let height: u32 = match mode_parts[1].parse() {
			Ok(height) => height,
			Err(e) => {
				let message = format!("Failed to parse height: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let refresh_rate: u32 = match mode_parts[2].parse() {
			Ok(refresh_rate) => refresh_rate,
			Err(e) => {
				let message = format!("Failed to parse refresh rate: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let remote_input_key = match params.remove("rikey") {
			Some(remote_input_key) => remote_input_key,
			None => {
				let message = format!("Expected 'rikey' in launch request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let remote_input_key = match hex::decode(remote_input_key) {
			Ok(remote_input_key) => remote_input_key,
			Err(e) => {
				let message = format!("Failed to decode remote input key: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let remote_input_key_id: String = match params.remove("rikeyid") {
			Some(remote_input_key_id) => remote_input_key_id,
			None => {
				let message = format!("Expected 'rikey_id' in launch request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let remote_input_key_id: i64 = match remote_input_key_id.parse() {
			Ok(remote_input_key_id) => remote_input_key_id,
			Err(e) => {
				let message = format!("Couldn't parse 'rikey_id' in launch request, got '{remote_input_key_id}' with error: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let initialize_result = self.session_manager.initialize_session(SessionContext {
			application_id,
			resolution: (width, height),
			refresh_rate,
			keys: SessionKeys {
				remote_input_key,
				remote_input_key_id,
			}
		}).await;

		if initialize_result.is_err() {
			return bad_request("Failed to start session".to_string());
		}

		let mut buffer = Vec::new();
		let mut writer = EmitterConfig::new()
			.write_document_declaration(true)
			.create_writer(&mut buffer);

		writer.write(XmlEvent::start_element("root")
			.attr("status_code", "200")).unwrap();

		writer.write(XmlEvent::start_element("gamesession")).unwrap();
		writer.write(XmlEvent::characters("1")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		// TODO: Return sessionUrl0.

		// </root>
		writer.write(XmlEvent::end_element()).unwrap();

		let mut response = Response::new(Full::new(Bytes::from(String::from_utf8(buffer).unwrap())));
		response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

		response
	}

	async fn resume(
		&self,
		mut params: HashMap<String, String>,
	) -> Response<Full<Bytes>> {
		let unique_id = match params.remove("uniqueid") {
			Some(unique_id) => unique_id,
			None => {
				let message = format!("Expected 'uniqueid' in resume request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		match self.client_manager.is_paired(unique_id).await {
			Ok(paired) => paired,
			Err(()) => return bad_request("Failed to check client paired status".to_string()),
		};

		let remote_input_key = match params.remove("rikey") {
			Some(remote_input_key) => remote_input_key,
			None => {
				let message = format!("Expected 'rikey' in resume request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let remote_input_key = match hex::decode(remote_input_key) {
			Ok(remote_input_key) => remote_input_key,
			Err(e) => {
				let message = format!("Failed to decode remote input key: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let remote_input_key_id: String = match params.remove("rikeyid") {
			Some(remote_input_key_id) => remote_input_key_id,
			None => {
				let message = format!("Expected 'rikey_id' in resume request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let remote_input_key_id: i64 = match remote_input_key_id.parse() {
			Ok(remote_input_key_id) => remote_input_key_id,
			Err(e) => {
				let message = format!("Couldn't parse 'rikey_id' in resume request, got '{remote_input_key_id}' with error: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let update_result = self.session_manager.update_keys(SessionKeys {
			remote_input_key,
			remote_input_key_id,
		}).await;
		if update_result.is_err() {
			return bad_request("Failed to update session keys".to_string());
		}

		let mut buffer = Vec::new();
		let mut writer = EmitterConfig::new()
			.write_document_declaration(true)
			.create_writer(&mut buffer);

		writer.write(XmlEvent::start_element("root")
			.attr("status_code", "200")).unwrap();

		// TODO: Return sessionUrl0.

		writer.write(XmlEvent::start_element("resume")).unwrap();
		writer.write(XmlEvent::characters("1")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		// </root>
		writer.write(XmlEvent::end_element()).unwrap();

		let mut response = Response::new(Full::new(Bytes::from(String::from_utf8(buffer).unwrap())));
		response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

		response
	}

	async fn cancel(&self) -> Response<Full<Bytes>> {
		if self.session_manager.stop_session().await.is_err() {
			let message = "Failed to stop session".to_string();
			log::warn!("{message}");
			return bad_request(message);
		}

		let mut buffer = Vec::new();
		let mut writer = EmitterConfig::new()
			.write_document_declaration(true)
			.create_writer(&mut buffer);

		// TODO: Check the use of some of these values, we leave most of them blank and Moonlight doesn't care.
		writer.write(XmlEvent::start_element("root")
			.attr("status_code", "200")).unwrap();

		writer.write(XmlEvent::start_element("cancel")).unwrap();
		writer.write(XmlEvent::characters("1")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		// </root>
		writer.write(XmlEvent::end_element()).unwrap();

		let mut response = Response::new(Full::new(Bytes::from(String::from_utf8(buffer).unwrap())));
		response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
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
		.map_err(|e| log::error!("Failed to retrieve network interfaces: {e}"))?;

	for interface in interfaces {
		for interface_address in interface.addr {
			if interface_address.ip() == address {
				log::debug!("Found MAC address for address {:?}: {:?}", address, interface.mac_addr.as_ref().unwrap_or(&"None".to_string()));
				return Ok(interface.mac_addr);
			}
		}
	}

	log::warn!("No interface found matching address {:?}", address);
	Ok(None)
}