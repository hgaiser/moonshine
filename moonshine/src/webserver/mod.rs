use std::{net::ToSocketAddrs, collections::HashMap, convert::Infallible};

use async_shutdown::ShutdownManager;
use http_body_util::Full;
use hyper::{service::service_fn, Response, Request, body::Bytes, StatusCode, header, Method};
use hyper_util::rt::TokioIo;
use openssl::x509::X509;
use tokio::net::TcpListener;
use xml::{EmitterConfig, writer::XmlEvent};

use crate::{config::Config, clients::ClientManager, session::{SessionManager, SessionContext}, webserver::tls::TlsAcceptor};

use self::pairing::handle_pair_request;

mod pairing;
mod tls;

const SERVERINFO_APP_VERSION: &str = "7.1.450.0";
const SERVERINFO_GFE_VERSION: &str = "3.23.0.74";
const SERVERINFO_UNIQUE_ID: &str = "7AD14F7C-2F8B-7329-AF86-42A06F6471FE"; // Should we generate / randomize this?

#[derive(Clone)]
pub struct Webserver {
	config: Config,
	client_manager: ClientManager,
	session_manager: SessionManager,
	server_certs: X509,
}

impl Webserver {
	pub fn new(
		config: Config,
		server_certs: X509,
		client_manager: ClientManager,
		session_manager: SessionManager,
		shutdown: ShutdownManager<i32>,
	) -> Result<Self, ()> {
		let server = Self {
			config: config.clone(),
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
			shutdown.wrap_cancel(shutdown.wrap_trigger_shutdown(1, async move {
				let listener = TcpListener::bind(http_address).await
					.map_err(|e| log::error!("Failed to bind to address {http_address}: {e}"))?;

				log::info!("Http server listening for connections on {http_address}");
				loop {
					let (connection, address) = listener.accept().await
						.map_err(|e| log::error!("Failed to accept connection: {e}"))?;
					log::debug!("Accepted connection from {address}.");

					let io = TokioIo::new(connection);

					tokio::spawn({
						let server = server.clone();
						async move {
							let _ = hyper::server::conn::http1::Builder::new()
								.serve_connection(io, service_fn(|request| {
									server.serve(request)
								})).await;
						}
					});
				}
				Ok::<(), ()>(())
			}))
		});

		// Run HTTPS webserver.
		let https_address = (config.address.clone(), config.webserver.port_https).to_socket_addrs()
			.map_err(|e| log::error!("Failed to resolve address '{}:{}': {e}", config.address, config.webserver.port_https))?
			.next()
			.ok_or_else(|| log::error!("Failed to resolve address '{}:{}'", config.address, config.webserver.port_https))?;

		tokio::spawn({
			let server = server.clone();
			shutdown.wrap_cancel(shutdown.wrap_trigger_shutdown(1, async move {
				let listener = TcpListener::bind(https_address).await
					.map_err(|e| log::error!("Failed to bind to address '{:?}': {e}", https_address))?;
				let acceptor = TlsAcceptor::from_config(config.webserver.certificate_chain, config.webserver.private_key)?;

				log::info!("Https server listening for connections on {https_address}");
				loop {
					let (connection, address) = listener.accept().await
						.map_err(|e| log::error!("Failed to accept connection: {e}"))?;
					let connection = match acceptor.accept(connection).await {
						Ok(connection) => connection,
						Err(()) => continue,
					};
					log::debug!("Accepted TLS connection from {address}.");

					let io = TokioIo::new(connection);

					tokio::spawn({
						let server = server.clone();
						async move {
							let _ = hyper::server::conn::http1::Builder::new()
								.serve_connection(io, service_fn(|request| {
									server.serve(request)
								})).await;
						}
					});
				}
				Ok::<(), ()>(())
			}))
		});

		Ok(server)
	}

	async fn serve(&self, request: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
		let params = request.uri()
			.query()
			.map(|v| {
				url::form_urlencoded::parse(v.as_bytes())
					.into_owned()
					.collect()
			})
			.unwrap_or_default();

		let response = match (request.method(), request.uri().path()) {
			(&Method::GET, "/serverinfo") => self.server_info(params).await,
			(&Method::GET, "/applist") => self.app_list(),
			(&Method::GET, "/pair") => handle_pair_request(params, &self.server_certs, &self.client_manager).await,
			(&Method::GET, "/pin") => self.pin(params).await,
			(&Method::GET, "/unpair") => self.unpair(params).await,
			(&Method::GET, "/launch") => self.launch(params).await,
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
			.perform_indent(true)
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
	) -> Response<Full<Bytes>> {
		let unique_id = match params.get("uniqueid") {
			Some(unique_id) => unique_id.clone(),
			None => {
				let message = format!("Expected 'uniqueid' in /serverinfo request, got {:?}.", params.keys());
				log::warn!("{message}");
				return bad_request(message);
			}
		};

		let current_session = match self.session_manager.get_current_session().await {
			Ok(current_session) => current_session,
			Err(()) => {
				let message = "Failed to get current session".to_string();
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
			.perform_indent(true)
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
		writer.write(XmlEvent::characters(SERVERINFO_UNIQUE_ID)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("HttpsPort")).unwrap();
		writer.write(XmlEvent::characters("")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("ExternalPort")).unwrap();
		writer.write(XmlEvent::characters("")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("mac")).unwrap();
		writer.write(XmlEvent::characters("")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("MaxLumaPixelsHEVC")).unwrap();
		writer.write(XmlEvent::characters("")).unwrap();
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
		writer.write(XmlEvent::characters(&current_session.clone().map(|s| s.context().application_id).unwrap_or(0).to_string())).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("state")).unwrap();
		writer.write(XmlEvent::characters(current_session.map(|_| "MOONSHINE_SERVER_BUSY").unwrap_or("MOONSHINE_SERVER_FREE"))).unwrap();
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
		let height: u32 = match mode_parts[0].parse() {
			Ok(height) => height,
			Err(e) => {
				let message = format!("Failed to parse height: {e}");
				log::warn!("{message}");
				return bad_request(message);
			}
		};
		let refresh_rate: u32 = match mode_parts[0].parse() {
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

		let launch_result = self.session_manager.launch(SessionContext {
			application_id,
			resolution: (width, height),
			refresh_rate,
			remote_input_key,
			remote_input_key_id,
		}).await;

		if launch_result.is_err() {
			return bad_request("Failed to launch session".to_string());
		}

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
			.perform_indent(true)
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

	pub async fn stop(&self) -> Result<(), ()> {
		self.session_manager.stop_session().await
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
