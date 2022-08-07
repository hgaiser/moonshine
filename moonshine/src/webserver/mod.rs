use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;

use hyper::Uri;
use hyper::server::conn::AddrIncoming;
use openssl::ssl::SslContext;
use openssl::ssl::SslFiletype;
use openssl::ssl::SslMethod;
use tls_listener::TlsListener;
use tokio::sync::Mutex;
use hyper::{service::service_fn, Response, Body, header::CONTENT_TYPE, Request, Method, StatusCode};

mod pairing;
use pairing::Clients;

type Params = HashMap<String, String>;

#[derive(Clone, Debug)]
pub struct WebserverConfig {
	pub name: String,
	pub address: String,
	pub port: u16,
	pub tls_port: u16,

	pub cert: PathBuf,
	pub key: PathBuf,
}

pub(crate) async fn run(config: WebserverConfig) -> Result<(), hyper::Error> {
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
	println!("Binding http webserver to {}", http_address);
	tokio::spawn(hyper::Server::try_bind(&http_address)?.serve(make_service));

	let https_address = (config.address.clone(), config.tls_port).to_socket_addrs().unwrap().next()
		.ok_or(format!("no address resolved for {}:{}", config.address, config.tls_port)).unwrap();
	println!("Binding https webserver to {}", https_address);

	let mut builder = SslContext::builder(SslMethod::tls_server()).unwrap();
	builder.set_certificate_file(&config.cert, SslFiletype::PEM).unwrap();
	builder.set_private_key_file(&config.key, SslFiletype::PEM).unwrap();
	let incoming = TlsListener::new(builder.build(), AddrIncoming::bind(&https_address)?);

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

	hyper::Server::builder(incoming).serve(make_service).await?;

	Ok(())
}

async fn serve(req: Request<Body>, config: WebserverConfig, clients: Clients) -> Response<Body> {
	println!("{} '{}' request.", req.method(), req.uri().path());

	match (req.method(), req.uri().path()) {
		(&Method::GET, "/pin") => pairing::pin(req, clients).await,
		(&Method::GET, "/serverinfo") => server_info(req, config, clients).await,
		(&Method::GET, "/pair") => pairing::pair(req, clients).await,
		(&Method::GET, "/unpair") => pairing::unpair(req, clients).await,
		_ => not_found()
	}
}

async fn server_info(req: Request<Body>, config: WebserverConfig, clients: Clients) -> Response<Body> {
	let params = parse_params(req.uri());

	let unique_id = match params.get("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			println!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
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
		config.tls_port,
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
