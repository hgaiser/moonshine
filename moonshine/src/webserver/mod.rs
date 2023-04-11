use std::{path::Path, net::{SocketAddr, ToSocketAddrs}, collections::HashMap};

use crate::{util::flatten, config::ApplicationConfig, webserver::pairing::handle_pair_request, session::{SessionManagerCommand, SessionContext}};

use async_shutdown::Shutdown;
use hyper::{Response, StatusCode, Body, Request, Method, header};
use tokio::{net::TcpListener, io::{AsyncRead, AsyncWrite}, task::JoinHandle, try_join, sync::{mpsc, oneshot}};
use xml::{EmitterConfig, writer::XmlEvent};

use self::tls::TlsAcceptor;

use crate::session::clients::{
	ClientManagerCommand,
	IsPairedCommand,
	RegisterPinCommand, RemoveClientCommand,
};

mod tls;
mod pairing;

const SERVERINFO_APP_VERESION: &str = "7.1.450.0";
const SERVERINFO_GFE_VERESION: &str = "3.23.0.74";
const SERVERINFO_UNIQUE_ID: &str = "7AD14F7C-2F8B-7329-AF86-42A06F6471FE"; // Should we generate / randomize this?

pub enum WebserverError {
	Hyper(hyper::http::Error),
	Other(String),
}

impl std::error::Error for WebserverError {}

impl std::fmt::Display for WebserverError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			WebserverError::Hyper(error) => write!(f, "{error}"),
			WebserverError::Other(error) => write!(f, "{error}"),
		}
	}
}

impl std::fmt::Debug for WebserverError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Hyper(error) => f.debug_tuple("Hyper").field(error).finish(),
			Self::Other(error) => f.debug_tuple("Other").field(error).finish(),
		}
	}
}

pub async fn run<A, P>(
	hostname: String,
	http_address: A,
	https_address: A,
	certificate_chain_path: P,
	private_key_path: P,
	applications: Vec<ApplicationConfig>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
	session_command_tx: mpsc::Sender<SessionManagerCommand>,
	shutdown: Shutdown,
) -> Result<(), ()>
where
	A: ToSocketAddrs + std::fmt::Debug,
	P: AsRef<Path>,
{
	let server_pem = std::fs::read(certificate_chain_path.as_ref())
		.map_err(|e| log::error!("Failed to read server certificate: {e}"))?;
	let server_pem = openssl::x509::X509::from_pem(&server_pem)
		.map_err(|e| log::error!("Failed to parse server certificate: {e}"))?;

	// Run HTTP webserver.
	let http_address: SocketAddr = http_address.to_socket_addrs()
		.map_err(|e| log::error!("Failed to resolve address '{:?}': {e}", http_address))?
		.next()
		.ok_or_else(|| log::error!("Failed to resolve address '{:?}'", http_address))?;

	let listener = TcpListener::bind(http_address)
		.await
		.map_err(|e| log::error!("Failed to bind to address '{}': {e}", http_address))?;

	let http_server_task: JoinHandle<Result<(), ()>> = tokio::spawn(shutdown.wrap_vital({
		let shutdown = shutdown.clone();
		let applications = applications.clone();
		let server_pem = server_pem.clone();
		let hostname = hostname.clone();
		let client_command_tx = client_command_tx.clone();
		let session_command_tx = session_command_tx.clone();
		async move {
			loop {
				let (connection, address) = shutdown.wrap_cancel(listener.accept())
					.await
					.ok_or(())?
					.map_err(|e| log::error!("Failed to accept connection: {e}"))?;
				log::debug!("Accepted connection from {address}.");

				tokio::spawn(shutdown.wrap_cancel(handle_connection(
					connection,
					applications.clone(),
					server_pem.clone(),
					hostname.clone(),
					client_command_tx.clone(),
					session_command_tx.clone(),
					shutdown.clone(),
				)));
			}
		}
	}));
	log::info!("Http server listening for connections on {}", http_address);

	// Run HTTPS webserver.
	let https_address: SocketAddr = https_address.to_socket_addrs()
		.map_err(|e| log::error!("Failed to resolve address '{:?}': {e}", https_address))?
		.next()
		.ok_or_else(|| log::error!("Failed to resolve address '{:?}'", https_address))?;

	let listener = TcpListener::bind(https_address)
		.await
		.map_err(|e| log::error!("Failed to bind to address '{:?}': {e}", https_address))?;
	let acceptor = TlsAcceptor::from_config(certificate_chain_path.as_ref(), private_key_path.as_ref())?;

	let https_server_task: JoinHandle<Result<(), ()>> = tokio::spawn(shutdown.wrap_vital({
		let shutdown = shutdown.clone();
		let applications = applications.clone();
		let server_pem = server_pem.clone();
		let hostname = hostname.clone();
		let client_command_tx = client_command_tx.clone();
		let session_command_tx = session_command_tx.clone();
		async move {
			loop {
				let (connection, address) = shutdown.wrap_cancel(listener.accept())
					.await
					.ok_or(())?
					.map_err(|e| log::error!("Failed to accept TLS connection: {}", e))?;
				log::debug!("Accepted TLS connection from {}", address);

				match acceptor.accept(connection).await {
					Ok(connection) => {
						tokio::spawn(shutdown.wrap_cancel(handle_connection(
							connection,
							applications.clone(),
							server_pem.clone(),
							hostname.clone(),
							client_command_tx.clone(),
							session_command_tx.clone(),
							shutdown.clone(),
						)));
					},
					// Ignore connection errors, they have been logged already.
					Err(()) => continue,
				};
			}
		}
	}));
	log::info!("Https server listening for connections on {}", https_address);

	match try_join!(
		flatten(http_server_task),
		flatten(https_server_task),
	) {
		Ok(_) => Ok(()),
		Err(_) => Err(()),
	}
}

async fn handle_connection<C>(
	connection: C,
	applications: Vec<ApplicationConfig>,
	server_pem: openssl::x509::X509,
	hostname: String,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
	session_command_tx: mpsc::Sender<SessionManagerCommand>,
	shutdown: Shutdown,
) -> Result<(), ()>
where
	C: AsyncRead + AsyncWrite + Unpin + 'static,
{
	let result = shutdown.wrap_cancel(hyper::server::conn::Http::new()
		.serve_connection(connection, hyper::service::service_fn(|request| {
			serve(
				request,
				applications.clone(),
				server_pem.clone(),
				hostname.clone(),
				client_command_tx.clone(),
				session_command_tx.clone(),
			)
		}))
	)
		.await
		.ok_or(())?;

	match result {
		Err(e) => {
			let message = e.to_string();
			if !message.starts_with("error shutting down connection:") {
				log::error!("Failed to serve connection: {}", message);
				Err(())
			} else {
				Ok(())
			}
		},
		Ok(()) => Ok(()),
	}
}

async fn serve(
	request: Request<Body>,
	applications: Vec<ApplicationConfig>,
	server_pem: openssl::x509::X509,
	hostname: String,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
	session_command_tx: mpsc::Sender<SessionManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	log::info!("{} '{}' request.", request.method(), request.uri().path());

	let params = request.uri()
		.query()
		.map(|v| {
			url::form_urlencoded::parse(v.as_bytes())
				.into_owned()
				.collect()
		})
		.unwrap_or_else(HashMap::new);

	match (request.method(), request.uri().path()) {
		(&Method::GET, "/applist") => app_list(applications),
		(&Method::GET, "/pair") => handle_pair_request(params, server_pem, client_command_tx).await,
		(&Method::GET, "/pin") => pin(params, client_command_tx).await,
		(&Method::GET, "/serverinfo") => server_info(params, hostname, client_command_tx).await,
		(&Method::GET, "/unpair") => unpair(params, client_command_tx).await,
		(&Method::GET, "/launch") => launch(params, client_command_tx, session_command_tx).await,
		(method, uri) => {
			log::warn!("Unhandled {method} request with URI '{uri}'");
			not_found()
		}
	}
}

fn app_list(applications: Vec<ApplicationConfig>) -> Result<Response<Body>, WebserverError> {
	let mut buffer = Vec::new();
	let mut writer = EmitterConfig::new()
		.write_document_declaration(true)
		.perform_indent(true)
		.create_writer(&mut buffer);

	writer.write(XmlEvent::start_element("root")
		.attr("status_code", "200")).unwrap();

	for (i, application) in applications.iter().enumerate() {
		writer.write(XmlEvent::start_element("App")).unwrap();

		// TODO: Fix HDR support.
		writer.write(XmlEvent::start_element("IsHdrSupported")).unwrap();
		writer.write(XmlEvent::characters("0")).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("AppTitle")).unwrap();
		writer.write(XmlEvent::characters(&application.title)).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		writer.write(XmlEvent::start_element("ID")).unwrap();
		writer.write(XmlEvent::characters(format!("{}", i).as_str())).unwrap();
		writer.write(XmlEvent::end_element()).unwrap();

		// </App>
		writer.write(XmlEvent::end_element()).unwrap();
	}

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer).unwrap()));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
	Ok(response)
}

async fn pin(
	params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>
) -> Result<Response<Body>, WebserverError> {
	let unique_id = match params.get("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let pin = match params.get("pin") {
		Some(pin) => pin,
		None => {
			log::error!("Expected 'pin' in pin request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let (response_tx, response_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::RegisterPin(RegisterPinCommand {
		id: unique_id.to_string(),
		pin: pin.to_string(),
		response: response_tx,
	}))
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to send pin to client manager: {e}")))?;

	let response = response_rx
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to wait for response to pin command from client manager: {e}")))?;

	match response {
		Ok(()) =>
			Response::builder()
				.status(StatusCode::OK)
				.body(Body::from(format!("Successfully received pin '{}' for unique id '{}'.", pin, unique_id)))
				.map_err(WebserverError::Hyper),
		Err(e) =>
			Response::builder()
				.status(StatusCode::INTERNAL_SERVER_ERROR)
				.body(Body::from(e.to_string()))
				.map_err(WebserverError::Hyper),
	}
}

async fn server_info(
	params: HashMap<String, String>,
	hostname: String,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let unique_id = match params.get("uniqueid") {
		Some(unique_id) => unique_id.clone(),
		None => {
			log::error!("Expected 'uniqueid' in /serverinfo request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let paired = get_paired_status(unique_id, &client_command_tx)
		.await
		.map_err(|_| WebserverError::Other("Failed to get paired status.".to_string()))?;

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
	writer.write(XmlEvent::characters(&hostname)).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	writer.write(XmlEvent::start_element("appversion")).unwrap();
	writer.write(XmlEvent::characters(SERVERINFO_APP_VERESION)).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	writer.write(XmlEvent::start_element("GfeVersion")).unwrap();
	writer.write(XmlEvent::characters(SERVERINFO_GFE_VERESION)).unwrap();
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
	writer.write(XmlEvent::characters("0")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	writer.write(XmlEvent::start_element("state")).unwrap();
	writer.write(XmlEvent::characters("MOONSHINE_SERVER_FREE")).unwrap();
	writer.write(XmlEvent::end_element()).unwrap();

	// </root>
	writer.write(XmlEvent::end_element()).unwrap();

	let mut response = Response::new(Body::from(String::from_utf8(buffer).unwrap()));
	response.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
	Ok(response)
}

async fn get_paired_status(id: String, client_command_tx: &mpsc::Sender<ClientManagerCommand>) -> Result<bool, ()> {
	let (paired_request_tx, paired_request_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::IsPaired(IsPairedCommand {
		id,
		response: paired_request_tx,
	}))
		.await
		.map_err(|e| log::error!("Failed to request client paired status: {e}"))?;

	paired_request_rx
		.await
		.map_err(|e| log::error!("Failed to wait for paired status request: {e}"))
}

async fn unpair(
	mut params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in unpair request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let (remove_client_tx, remove_client_rx) = oneshot::channel();
	client_command_tx.send(ClientManagerCommand::RemoveClient(RemoveClientCommand {
		id,
		response: remove_client_tx,
	}))
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to send remove client request to client manager: {e}")))?;

	let result = remove_client_rx
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to receive remove client response from client manager: {e}")))?;

	match result {
		Ok(()) =>
			Ok(Response::builder()
				.status(StatusCode::OK)
				.body(Body::from("Successfully unpaired.".to_string()))
				.unwrap()),
		Err(e) => Err(WebserverError::Other(e)),
	}
}

async fn launch(
	mut params: HashMap<String, String>,
	client_command_tx: mpsc::Sender<ClientManagerCommand>,
	session_command_tx: mpsc::Sender<SessionManagerCommand>,
) -> Result<Response<Body>, WebserverError> {
	let unique_id = match params.remove("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::warn!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let paired = get_paired_status(unique_id, &client_command_tx)
		.await
		.map_err(|_| WebserverError::Other("Failed to get pairing status.".to_string()))?;
	if !paired {
		log::warn!("Can't launch a session for an unpaired client.");
		return bad_request();
	}

	let application_id: u32 = match params.remove("appid") {
		Some(application_id) => application_id.parse().map_err(|e| WebserverError::Other(format!("Failed to parse application id: {e}")))?,
		None => {
			log::warn!("Expected 'appid' in launch request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let mode: String = match params.remove("mode") {
		Some(mode) => mode,
		None => {
			log::warn!("Expected 'mode' in launch request, got {:?}.", params.keys());
			return bad_request();
		}
	};
	let mode_parts: Vec<&str> = mode.split('x').collect();
	if mode_parts.len() != 3 {
		log::warn!("Expected mode in format WxHxR, but got '{mode}'.");
		return bad_request();
	}
	let width: u32 = mode_parts[0].parse().map_err(|e| WebserverError::Other(format!("Failed to parse width: {e}")))?;
	let height: u32 = mode_parts[0].parse().map_err(|e| WebserverError::Other(format!("Failed to parse height: {e}")))?;
	let refresh_rate: u32 = mode_parts[0].parse().map_err(|e| WebserverError::Other(format!("Failed to parse refresh_rate: {e}")))?;

	let remote_input_key = match params.remove("rikey") {
		Some(remote_input_key) => hex::decode(remote_input_key).map_err(|e| WebserverError::Other(format!("Failed to decode rikey: {e}")))?,
		None => {
			log::warn!("Expected 'rikey' in launch request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let remote_input_key_id: String = match params.remove("rikeyid") {
		Some(remote_input_key_id) => remote_input_key_id,
		None => {
			log::warn!("Expected 'rikey_id' in launch request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let command = SessionManagerCommand::LaunchSession(SessionContext {
		application_id,
		resolution: (width, height),
		refresh_rate,
		remote_input_key,
		remote_input_key_id,
	});
	session_command_tx.send(command)
		.await
		.map_err(|e| WebserverError::Other(format!("Failed to send session command: {e}")))?;

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

fn bad_request() -> Result<Response<Body>, WebserverError> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Body::from("BAD REQUEST".to_string()))
		.map_err(WebserverError::Hyper)
}

fn not_found() -> Result<Response<Body>, WebserverError> {
	Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body(Body::from("NOT FOUND".to_string()))
		.map_err(WebserverError::Hyper)
}
