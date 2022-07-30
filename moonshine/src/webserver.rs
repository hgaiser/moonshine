use std::net::ToSocketAddrs;

use axum::{routing::get, Router, response::IntoResponse, body::Body, http::{Request, header}};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct DisplayMode {
	#[serde(rename(serialize = "Width"))]
	width: u32,
	#[serde(rename(serialize = "Height"))]
	height: u32,
	#[serde(rename(serialize = "RefreshRate"))]
	refresh_rate: u32,
}

#[derive(Debug, Serialize)]
struct ServerInfo {
	#[serde(rename(serialize = "status_code"))]
	status_code: u32,
	#[serde(rename(serialize = "hostname"))]
	hostname: String,
	#[serde(rename(serialize = "appversion"))]
	app_version: String,
	#[serde(rename(serialize = "GfeVersion"))]
	gfe_version: String,
	#[serde(rename(serialize = "uniqueid"))]
	unique_id: String,
	#[serde(rename(serialize = "HttpsPort"))]
	https_port: u32,
	#[serde(rename(serialize = "ExternalPort"))]
	external_port: u32,
	#[serde(rename(serialize = "mac"))]
	mac_address: String,
	#[serde(rename(serialize = "MaxLumaPixelsHEVC"))]
	max_luma_pixels_hevc: u32,
	#[serde(rename(serialize = "LocalIP"))]
	local_ip: String,
	#[serde(rename(serialize = "ServerCodecModeSupport"))]
	server_codec_mode_support: u32,
	#[serde(rename(serialize = "SupportedDisplayMode"))]
	supported_display_modes: Vec<DisplayMode>,
	#[serde(rename(serialize = "PairStatus"))]
	pair_status: u32,
	#[serde(rename(serialize = "currentgame"))]
	current_game: u32,
	#[serde(rename(serialize = "state"))]
	state: String,
}

async fn server_info(request: Request<Body>) -> impl IntoResponse {
	println!("Got '{}' request.", request.uri());

	// let server_info = ServerInfo {
	// 	status_code: 200,
	// 	hostname: "Moonshine".to_string(),
	// 	app_version: "0.0.1".to_string(),
	// 	gfe_version: "3.23.0.74".to_string(),
	// 	unique_id: "7AD14F7C-2F8B-7329-AF86-42A06F6471FE".to_string(),
	// 	https_port: 47984,
	// 	external_port: 47989,
	// 	mac_address: "64:bc:58:be:e5:88".to_string(),
	// 	max_luma_pixels_hevc: 1869449984,
	// 	local_ip: "10.0.5.137".to_string(),
	// 	server_codec_mode_support: 259,
	// 	supported_display_modes: vec![DisplayMode {
	// 		width: 2560,
	// 		height: 1440,
	// 		refresh_rate: 60
	// 	}],
	// 	pair_status: 0,
	// 	current_game: 0,
	// 	state: "MOONSHINE_SERVER_FREE".to_string(),
	// };

	let response = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
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
</root>";

    ([(header::CONTENT_TYPE, "application/json")], response)
}

async fn pair(request: Request<Body>) -> impl IntoResponse {
	println!("Got '{}' request.", request.uri());
    ([(header::CONTENT_TYPE, "application/json")], "<root/>")
}

async fn fallback(request: Request<Body>) {
	println!("Got '{}' request.", request.uri());
}

pub(crate) async fn run(port: u16) {
	let app = Router::new()
		.route("/serverinfo", get(server_info))
		.route("/pair", get(pair))
		.fallback(get(fallback))
	;

	let server = axum::Server::bind(
		// TODO: Find a cleaner way to convert to SocketAddr...
		&("0.0.0.0", port).to_socket_addrs().unwrap().next().unwrap()
	)
		.serve(app.into_make_service())
	;

	println!("Server is waiting for connections on port {}", port);

	server
		.await
		.unwrap()
	;
}
