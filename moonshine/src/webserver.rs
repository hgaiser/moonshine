use std::net::SocketAddr;
use std::collections::HashMap;

use tokio::net::TcpListener;
use hyper::{server::conn::Http, service::service_fn, Response, Body, header::CONTENT_TYPE, Request, Method, StatusCode};

// #[derive(Debug, Serialize)]
// struct DisplayMode {
// 	#[serde(rename(serialize = "Width"))]
// 	width: u32,
// 	#[serde(rename(serialize = "Height"))]
// 	height: u32,
// 	#[serde(rename(serialize = "RefreshRate"))]
// 	refresh_rate: u32,
// }

// #[derive(Debug, Serialize)]
// struct ServerInfo {
// 	#[serde(rename(serialize = "status_code"))]
// 	status_code: u32,
// 	#[serde(rename(serialize = "hostname"))]
// 	hostname: String,
// 	#[serde(rename(serialize = "appversion"))]
// 	app_version: String,
// 	#[serde(rename(serialize = "GfeVersion"))]
// 	gfe_version: String,
// 	#[serde(rename(serialize = "uniqueid"))]
// 	unique_id: String,
// 	#[serde(rename(serialize = "HttpsPort"))]
// 	https_port: u32,
// 	#[serde(rename(serialize = "ExternalPort"))]
// 	external_port: u32,
// 	#[serde(rename(serialize = "mac"))]
// 	mac_address: String,
// 	#[serde(rename(serialize = "MaxLumaPixelsHEVC"))]
// 	max_luma_pixels_hevc: u32,
// 	#[serde(rename(serialize = "LocalIP"))]
// 	local_ip: String,
// 	#[serde(rename(serialize = "ServerCodecModeSupport"))]
// 	server_codec_mode_support: u32,
// 	#[serde(rename(serialize = "SupportedDisplayMode"))]
// 	supported_display_modes: Vec<DisplayMode>,
// 	#[serde(rename(serialize = "PairStatus"))]
// 	pair_status: u32,
// 	#[serde(rename(serialize = "currentgame"))]
// 	current_game: u32,
// 	#[serde(rename(serialize = "state"))]
// 	state: String,
// }

fn server_info() -> Response<Body> {
	let mut response = Response::new(Body::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>
<root status_code=\"200\">
	<hostname>Moonshine Game PC</hostname>
	<appversion>7.1.431.0</appversion>
	<GfeVersion>3.23.0.74</GfeVersion>
	<uniqueid>7AD14F7C-2F8B-7329-AF86-42A06F6471FE</uniqueid>
	<HttpsPort>47984</HttpsPort>
	<ExternalPort>47989</ExternalPort>
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
	<PairStatus>0</PairStatus>
	<currentgame>0</currentgame>
	<state>SUNSHINE_SERVER_FREE</state>
</root>"));
	response.headers_mut().insert(CONTENT_TYPE, "application/json".parse().unwrap());

	response
}

fn pair(req: Request<Body>) -> Response<Body> {
	let params: HashMap<String, String> = req
		.uri()
		.query()
		.map(|v| {
			url::form_urlencoded::parse(v.as_bytes())
				.into_owned()
				.collect()
		})
		.unwrap_or_else(HashMap::new)
	;

	let client_cert = match params.get("clientcert") {
		Some(client_cert) => client_cert,
		None => {
			println!("Expected client certificate in pairing request, got {:?}.", params.keys());
			return Response::builder()
				.status(StatusCode::BAD_REQUEST)
				.body(Body::from("BAD REQUEST".to_string()))
				.unwrap()
		}
	};
	let pem = hex::decode(client_cert).unwrap();
	let pem = openssl::x509::X509::from_pem(pem.as_slice());

	println!("Client cert: {:#?}", pem);

	let response = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<root status_code=\"200\">
	<paired>1</paired>
</root>";
	let mut response = Response::new(Body::from(response));
	response.headers_mut().insert(CONTENT_TYPE, "application/json".parse().unwrap());

	response
}

async fn router(req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
	println!("{} '{}' request.", req.method(), req.uri().path());

	match (req.method(), req.uri().path()) {
		(&Method::GET, "/serverinfo") => Ok(server_info()),
		(&Method::GET, "/pair") => Ok(pair(req)),
		_ => Ok(
			Response::builder()
				.status(StatusCode::NOT_FOUND)
				.body(Body::from("NOT FOUND".to_string()))
				.unwrap()
		)
	}
}

pub(crate) async fn run(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
	let addr = SocketAddr::from(([0, 0, 0, 0], port));

	let listener = TcpListener::bind(addr).await?;
	println!("Listening on http://{}", addr);
	loop {
		let (stream, _) = listener.accept().await?;

		tokio::task::spawn(async move {
			if let Err(err) = Http::new().serve_connection(stream, service_fn(router)).await {
				println!("Error serving connection: {:?}", err);
			}
		});
	}
}
