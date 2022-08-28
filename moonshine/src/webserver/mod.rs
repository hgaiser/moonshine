use std::convert::Infallible;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;

use hyper::Uri;
use hyper::body::Bytes;
use hyper::server::conn::AddrIncoming;
use openssl::ssl::Ssl;
use openssl::ssl::SslAcceptor;
use openssl::ssl::SslContext;
use openssl::ssl::SslFiletype;
use openssl::ssl::SslMethod;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use hyper::{service::service_fn, Response, Body, header::CONTENT_TYPE, Request, Method, StatusCode};

mod pairing;
use pairing::Clients;
use tokio_openssl::SslStream;

mod tls;
use tls::TlsAcceptor;

use crate::config;

type Params = HashMap<String, String>;

pub(crate) async fn run(config: config::Config) -> Result<(), ()> {
	let clients: Clients = Arc::new(Mutex::new(HashMap::new()));

	let make_service = hyper::service::make_service_fn({
		let clients = clients.clone();
		let config = config.clone();
		move |_| {
			let clients = clients.clone();
			let config = config.clone();
			async {
				Ok::<_, String>(service_fn(move |req| {
					let clients = clients.clone();
					let config = config.clone();
					async {
						Ok::<_, String>(serve(req, config, clients).await)
					}
				}))
			}
		}
	});

	let http_address = (config.address.clone(), config.port).to_socket_addrs().unwrap().next()
		.ok_or(format!("no address resolved for {}:{}", config.address, config.port)).unwrap();
	log::info!("Binding http webserver to {}", http_address);
	tokio::spawn(hyper::Server::try_bind(&http_address)
		.map_err(|e| log::error!("failed to bind to {}: {}", e, &http_address))?
		.serve(make_service));

	let https_address = (config.address.clone(), config.tls.port).to_socket_addrs()
		.map_err(|e| log::error!("No address resolved for '{}:{}': {}", config.address, config.tls.port, e))?
		.next()
		.ok_or_else(|| log::error!("No address resolved for {}:{}", config.address, config.tls.port))?;
	log::info!("Binding https webserver to {}", https_address);

	let listener = TcpListener::bind(https_address)
		.await
		.map_err(|e| log::error!("Failed to bind to {}: {}", https_address, e))?;

	let acceptor = TlsAcceptor::from_config(&config.tls)?;

	loop {
		let (connection, address) = listener.accept()
			.await
			.map_err(|e| log::error!("Failed to accept connection: {}", e))?;
		log::debug!("Accepted connection from {}", address);

		let connection = acceptor.accept(connection).await?;
		tokio::spawn({
			let clients = clients.clone();
			let config = config.clone();
			async move {
				let result = hyper::server::conn::Http::new()
					.serve_connection(connection, hyper::service::service_fn(move |request| {
						let clients = clients.clone();
						let config = config.clone();
						async move {
							Ok::<_, String>(serve(request, config, clients).await)
						}
					}))
					.await;
				if let Err(e) = result {
					let message = e.to_string();
					if !message.starts_with("error shutting down connection:") {
						log::error!("Error in connection with {}: {}", address, message);
					}
				}
			}
		});
	}
}

async fn serve(req: Request<Body>, config: config::Config, clients: Clients) -> Response<Body> {
	log::info!("{} '{}' request.", req.method(), req.uri().path());

	match (req.method(), req.uri().path()) {
		(&Method::GET, "/pin") => pairing::pin(req, clients).await,
		(&Method::GET, "/serverinfo") => server_info(req, config, clients).await,
		(&Method::GET, "/pair") => pairing::pair(req, clients).await,
		(&Method::GET, "/unpair") => pairing::unpair(req, clients).await,
		_ => not_found()
	}
}

async fn server_info(req: Request<Body>, config: config::Config, clients: Clients) -> Response<Body> {
	let params = parse_params(req.uri());

	let unique_id = match params.get("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let paired = if clients.lock().await.contains_key(unique_id) {
		"1"
	} else {
		"0"
	};

	let mut response = Response::new(Body::from(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>
<root status_code=\"200\">
	<hostname>{}</hostname>
	<appversion>7.1.431.0</appversion>
	<GfeVersion>3.23.0.74</GfeVersion>
	<uniqueid>7AD14F7C-2F8B-7329-AF86-42A06F6471FE</uniqueid>
	<HttpsPort>{}</HttpsPort>
	<ExternalPort>{}</ExternalPort>
	<mac>64:bc:58:be:e5:88</mac>
	<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>
	<LocalIP>10.0.5.137</LocalIP>
	<ServerCodecModeSupport>259</ServerCodecModeSupport>
	<SupportedDisplayMode>
		<DisplayMode>
			<Width>2560</Width>
			<Height>1440</Height>
			<RefreshRate>120</RefreshRate>
		</DisplayMode>
	</SupportedDisplayMode>
	<PairStatus>{}</PairStatus>
	<currentgame>0</currentgame>
	<state>SUNSHINE_SERVER_FREE</state>
</root>",
		config.name,
		config.tls.port,
		config.port,
		paired,
	)));
	response.headers_mut().insert(CONTENT_TYPE, "application/xml".parse().unwrap());

	response
}

fn parse_params(uri: &Uri) -> Params {
	uri
		.query()
		.map(|v| {
			url::form_urlencoded::parse(v.as_bytes())
				.into_owned()
				.collect()
		})
		.unwrap_or_else(HashMap::new)
}

fn bad_request() -> Response<Body> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Body::from("BAD REQUEST".to_string()))
		.unwrap()
}

fn not_found() -> Response<Body> {
	Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body(Body::from("NOT FOUND".to_string()))
		.unwrap()
}
