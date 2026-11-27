//! The web control panel of Moonshine.
//!
//! This crate serves the page, its JSON API and a WebSocket with live updates. It knows nothing
//! about Moonshine itself: a [`Backend`] gives it the data and performs the actions, so the
//! `moonshine` crate can depend on this one.

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_tungstenite::HyperWebsocket;
use hyper_tungstenite::tungstenite::Message;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;

mod access;
mod logs;
pub mod protocol;
mod stats;
pub mod stream;

pub use access::Network;
pub use logs::{LogBuffer, LogLayer, LogLine};
use protocol::{Action, Snapshot, StatsEvent, StreamStart};
use stats::StatsHistory;

/// What the control panel needs from Moonshine.
pub trait Backend: Send + Sync + 'static {
	/// Everything the pages show, except statistics and logs.
	fn snapshot(&self) -> impl Future<Output = Snapshot> + Send;

	/// Do what the user asked. Returns a message for the user, or an error that says what went
	/// wrong and how to fix it.
	fn perform(&self, action: Action) -> impl Future<Output = Result<String, String>> + Send;

	/// The video stream in progress, if any, and a receiver for everything after it.
	fn stats(&self) -> (Option<StreamStart>, broadcast::Receiver<StatsEvent>);

	/// A plain-text report for bug reports.
	fn report(&self) -> impl Future<Output = String> + Send;

	/// Look for applications that were installed or removed.
	fn refresh(&self) -> impl Future<Output = ()> + Send;

	/// Stream to a browser: start a session, or join the running one.
	fn start_stream(
		&self,
		request: stream::StreamRequest,
	) -> impl Future<Output = Result<stream::StreamLink, String>> + Send;
}

/// Where and for whom the control panel is served.
pub struct Options {
	/// Addresses to listen on.
	pub listen: Vec<SocketAddr>,
	/// Address ranges that can open the control panel.
	pub allowed_networks: Vec<Network>,
	/// Names of this computer that the `Host` header may hold, besides IP addresses and
	/// `localhost`.
	pub host_names: Vec<String>,
	/// The TLS configuration for HTTPS. With it, the control panel serves HTTPS and HTTP on the
	/// same port: plain HTTP stays on loopback connections (which browsers treat as secure) and
	/// gets a redirect to HTTPS on all others. Without it, the control panel serves HTTP only.
	pub tls: Option<Arc<rustls::ServerConfig>>,
	/// The certificate for streaming to the browser (WebTransport, UDP on the same port).
	pub stream_certificate: stream::WebTransportCertificate,
	/// The address or name that browsers connect to for the stream. Without it, a page that comes
	/// through a reverse proxy streams from the address that the proxy connected to, and any
	/// other page from the address in its own URL.
	pub stream_host: Option<String>,
	/// Whether to stream to the browser at all. Off: no WebTransport server and no tokens.
	pub streaming: bool,
}

/// How a connection reached the control panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
	/// Plain HTTP that the control panel serves.
	Http,
	/// HTTPS.
	Https,
	/// Plain HTTP that only gets a redirect to HTTPS.
	RedirectToHttps,
}

/// How long a new connection can take to send its first byte.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);

/// The first byte of a TLS handshake record.
const TLS_HANDSHAKE: u8 = 0x16;

/// How often the snapshot is rebuilt while nothing pokes it.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

/// How often the applications are checked for changes.
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// How many log lines a new page gets.
const LOG_TAIL: usize = 1000;

/// The largest request body the API reads.
const MAX_BODY: usize = 64 * 1024;

/// The running control panel. Dropping it stops the control panel.
pub struct UiServer {
	tasks: Vec<JoinHandle<()>>,
}

impl Drop for UiServer {
	fn drop(&mut self) {
		for task in &self.tasks {
			task.abort();
		}
	}
}

struct State<B> {
	backend: Arc<B>,
	options: Options,
	/// The server for streams to the browser, unless it failed to start.
	webtransport: Option<Arc<stream::WebTransport>>,
	snapshot: watch::Receiver<Arc<String>>,
	poke: Arc<Notify>,
	stats: StatsHistory,
	logs: LogBuffer,
}

/// Start the control panel. Fails when it cannot listen on any address.
pub async fn serve<B: Backend>(backend: Arc<B>, mut options: Options, logs: LogBuffer) -> Result<UiServer, String> {
	let mut listeners = Vec::new();
	let mut errors = Vec::new();
	for address in &options.listen {
		match TcpListener::bind(address).await {
			Ok(listener) => listeners.push(listener),
			Err(e) => errors.push(format!("{address}: {e}")),
		}
	}
	if listeners.is_empty() {
		return Err(format!(
			"Failed to listen for the control panel ({}).",
			errors.join(", ")
		));
	}
	for error in errors {
		tracing::debug!("Control panel does not listen on {error}.");
	}

	let mut tasks = Vec::new();
	let poke = Arc::new(Notify::new());
	let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(snapshot_json(&backend.snapshot().await)));
	tasks.push(tokio::spawn({
		let backend = backend.clone();
		let poke = poke.clone();
		async move {
			loop {
				tokio::select! {
					_ = tokio::time::sleep(SNAPSHOT_INTERVAL) => {},
					_ = poke.notified() => {},
				}
				let json = snapshot_json(&backend.snapshot().await);
				snapshot_tx.send_if_modified(|current| {
					if **current == json {
						false
					} else {
						*current = Arc::new(json);
						true
					}
				});
			}
		}
	}));
	tasks.push(tokio::spawn({
		let backend = backend.clone();
		let poke = poke.clone();
		async move {
			loop {
				tokio::time::sleep(REFRESH_INTERVAL).await;
				backend.refresh().await;
				poke.notify_one();
			}
		}
	}));

	let stats = StatsHistory::new();
	let (current, events) = backend.stats();
	tasks.push(tokio::spawn(stats.clone().run(current, events)));

	// Streaming to the browser listens on the same addresses and port, over UDP.
	let stream_addresses: Vec<SocketAddr> = listeners
		.iter()
		.filter_map(|listener| listener.local_addr().ok())
		.collect();
	let port = stream_addresses.first().map(SocketAddr::port).unwrap_or_default();
	let stream_access = Arc::new(stream::Access {
		allowed_networks: options.allowed_networks.clone(),
		host_names: options.host_names.clone(),
	});
	let certificate = std::mem::replace(
		&mut options.stream_certificate,
		stream::WebTransportCertificate::SelfSigned { names: Vec::new() },
	);
	let webtransport = if !options.streaming {
		tracing::info!("Streaming to the browser is off ([ui] streaming = false).");
		None
	} else {
		match stream::serve(backend.clone(), &stream_addresses, port, certificate, stream_access).await {
			Ok((webtransport, stream_tasks)) => {
				tasks.extend(stream_tasks);
				Some(webtransport)
			},
			Err(e) => {
				tracing::warn!("{e} The control panel cannot stream to a browser.");
				None
			},
		}
	};

	let state = Arc::new(State {
		backend,
		options,
		webtransport,
		snapshot: snapshot_rx,
		poke,
		stats,
		logs,
	});

	let scheme = if state.options.tls.is_some() { "https" } else { "http" };
	for listener in listeners {
		if let Ok(address) = listener.local_addr() {
			tracing::info!("Control panel listening on {scheme}://{address}");
		}
		tasks.push(tokio::spawn(accept(listener, state.clone())));
	}

	Ok(UiServer { tasks })
}

fn snapshot_json(snapshot: &Snapshot) -> String {
	serde_json::to_string(snapshot).unwrap_or_else(|e| {
		tracing::warn!("Failed to serialize the control panel snapshot: {e}");
		"{}".to_string()
	})
}

async fn accept<B: Backend>(listener: TcpListener, state: Arc<State<B>>) {
	loop {
		let (connection, peer) = match listener.accept().await {
			Ok(accepted) => accepted,
			Err(e) => {
				tracing::warn!("Control panel failed to accept a connection: {e}");
				tokio::time::sleep(Duration::from_millis(100)).await;
				continue;
			},
		};
		let allowed = state
			.options
			.allowed_networks
			.iter()
			.any(|network| network.contains(peer.ip()));
		if !allowed {
			tracing::debug!("Control panel refused {peer}: not in [ui] allowed_networks.");
		}

		let Ok(local) = connection.local_addr() else {
			continue;
		};
		let peer = Peer { remote: peer, local };
		let state = state.clone();
		tokio::spawn(async move {
			let Some(tls) = state.options.tls.clone() else {
				serve_connection(connection, state, allowed, peer, Scheme::Http).await;
				return;
			};

			// TLS and plain HTTP share the port: a TLS connection starts with a handshake record.
			let mut first = [0u8; 1];
			let is_tls = match tokio::time::timeout(FIRST_BYTE_TIMEOUT, connection.peek(&mut first)).await {
				Ok(Ok(1)) => first[0] == TLS_HANDSHAKE,
				_ => return,
			};
			if is_tls {
				match tokio_rustls::TlsAcceptor::from(tls).accept(connection).await {
					Ok(stream) => serve_connection(stream, state, allowed, peer, Scheme::Https).await,
					// Usually a browser that does not trust the certificate yet.
					Err(e) => tracing::debug!("Control panel TLS handshake with {peer} failed: {e}"),
				}
			} else if peer.remote.ip().is_loopback() {
				serve_connection(connection, state, allowed, peer, Scheme::Http).await;
			} else {
				serve_connection(connection, state, allowed, peer, Scheme::RedirectToHttps).await;
			}
		});
	}
}

/// Both ends of a connection to the control panel.
#[derive(Clone, Copy, Debug)]
struct Peer {
	remote: SocketAddr,
	/// The address of this computer that the connection came to.
	local: SocketAddr,
}

impl std::fmt::Display for Peer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.remote.fmt(f)
	}
}

async fn serve_connection<B: Backend, I>(io: I, state: Arc<State<B>>, allowed: bool, peer: Peer, scheme: Scheme)
where
	I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
	let service = service_fn(move |request| {
		let state = state.clone();
		async move { Ok::<_, Infallible>(handle(state, request, allowed, peer, scheme).await) }
	});
	let _ = hyper::server::conn::http1::Builder::new()
		.serve_connection(TokioIo::new(io), service)
		.with_upgrades()
		.await;
}

async fn handle<B: Backend>(
	state: Arc<State<B>>,
	mut request: Request<Incoming>,
	allowed: bool,
	peer: Peer,
	scheme: Scheme,
) -> Response<Full<Bytes>> {
	let host = header_str(&request, header::HOST).unwrap_or_default().to_string();
	if !allowed {
		return text(
			StatusCode::FORBIDDEN,
			"This address cannot open the Moonshine control panel. To allow it, add its address \
			 range to `allowed_networks` in the [ui] table of config.toml, then restart Moonshine.",
		);
	}
	if !access::host_allowed(&host, &state.options.host_names) {
		return text(
			StatusCode::FORBIDDEN,
			"Open the control panel by this computer's address or name. To use another name, such \
			 as the name of a reverse proxy, add it to `allowed_hosts` in the [ui] table of config.toml, then \
			 restart Moonshine.",
		);
	}

	// A reverse proxy that serves HTTPS itself connects with plain HTTP and says so in
	// `X-Forwarded-Proto`; a redirect would loop. The redirect is only a convenience, so a client
	// that sends the header itself gains nothing.
	let forwarded_https =
		header_str(&request, "x-forwarded-proto").is_some_and(|proto| proto.eq_ignore_ascii_case("https"));
	if scheme == Scheme::RedirectToHttps && !forwarded_https {
		return redirect_to_https(&host, &request);
	}

	// A reverse proxy forwards the page but not the stream, so the browser streams from this
	// computer directly.
	let forwarded = ["x-forwarded-for", "x-forwarded-proto", "forwarded"]
		.iter()
		.any(|name| request.headers().contains_key(*name));
	let stream_host = stream_host(&state.options, forwarded, peer.local);
	let stream_source = state
		.webtransport
		.as_ref()
		.map(|webtransport| stream_source(stream_host.as_deref(), &host, webtransport.port()));

	let path = request.uri().path().to_string();
	let mut response = match (request.method(), path.as_str()) {
		(&Method::GET, "/ws") => {
			let origin_ok = header_str(&request, header::ORIGIN)
				.is_some_and(|origin| access::origin_allowed(origin, &host, &state.options.host_names));
			if !origin_ok {
				return text(
					StatusCode::FORBIDDEN,
					"WebSocket connections must come from the control panel.",
				);
			}
			if !hyper_tungstenite::is_upgrade_request(&request) {
				return text(StatusCode::BAD_REQUEST, "Expected a WebSocket upgrade.");
			}
			match hyper_tungstenite::upgrade(&mut request, None) {
				Ok((response, websocket)) => {
					tokio::spawn(websocket_session(state.clone(), websocket));
					return response;
				},
				Err(e) => return text(StatusCode::BAD_REQUEST, &format!("Failed to open the WebSocket: {e}")),
			}
		},
		(&Method::GET, "/" | "/index.html") => {
			asset(include_bytes!("../assets/index.html"), "text/html; charset=utf-8")
		},
		(&Method::GET, "/app.js") => asset(include_bytes!("../assets/app.js"), "text/javascript; charset=utf-8"),
		(&Method::GET, "/app.css") => asset(include_bytes!("../assets/app.css"), "text/css; charset=utf-8"),
		(&Method::GET, "/stream.js") => asset(include_bytes!("../assets/stream.js"), "text/javascript; charset=utf-8"),
		(&Method::GET, "/play" | "/play/") => asset(include_bytes!("../assets/play.html"), "text/html; charset=utf-8"),
		(&Method::GET, "/play.js") => asset(include_bytes!("../assets/play.js"), "text/javascript; charset=utf-8"),
		(&Method::GET, "/fec.js") => asset(include_bytes!("../assets/fec.js"), "text/javascript; charset=utf-8"),
		(&Method::GET, "/stream-audio.js") => asset(
			include_bytes!("../assets/stream-audio.js"),
			"text/javascript; charset=utf-8",
		),
		(&Method::GET, "/banner.png") => asset(include_bytes!("../../assets/banner-ui.png"), "image/png"),
		(&Method::GET, "/manifest.webmanifest") => asset(
			include_bytes!("../assets/manifest.webmanifest"),
			"application/manifest+json",
		),
		(&Method::GET, "/icon-180.png") => asset(include_bytes!("../assets/icon-180.png"), "image/png"),
		(&Method::GET, "/icon-192.png") => asset(include_bytes!("../assets/icon-192.png"), "image/png"),
		(&Method::GET, "/icon-512.png") => asset(include_bytes!("../assets/icon-512.png"), "image/png"),
		(&Method::GET, "/icon-maskable-192.png") => {
			asset(include_bytes!("../assets/icon-maskable-192.png"), "image/png")
		},
		(&Method::GET, "/icon-maskable-512.png") => {
			asset(include_bytes!("../assets/icon-maskable-512.png"), "image/png")
		},
		(&Method::GET, "/pixelify-sans.woff2") => asset(include_bytes!("../assets/pixelify-sans.woff2"), "font/woff2"),
		(&Method::GET, "/api/snapshot") => {
			let snapshot = state.snapshot.borrow().clone();
			json(StatusCode::OK, snapshot.as_bytes().to_vec())
		},
		(&Method::GET, "/api/report") => {
			let mut response = text(StatusCode::OK, &state.backend.report().await);
			response.headers_mut().insert(
				header::CONTENT_DISPOSITION,
				HeaderValue::from_static("attachment; filename=\"moonshine-report.txt\""),
			);
			response
		},
		(&Method::POST, "/api/action") => action(&state, request).await,
		(&Method::POST, "/api/stream/token") => {
			let client = if forwarded {
				forwarded_client(header_str(&request, "x-forwarded-for"))
			} else {
				Some(peer.remote.ip())
			};
			stream_token(&state, &request, client, stream_host)
		},
		_ => text(StatusCode::NOT_FOUND, "Not found."),
	};

	secure(&mut response, &host, stream_source.as_deref());
	response
}

/// The address or name that the page streams from, or `None` for the one in its own URL.
fn stream_host(options: &Options, forwarded: bool, local: SocketAddr) -> Option<String> {
	if let Some(host) = &options.stream_host {
		return Some(host.clone());
	}
	// A trusted certificate names this computer, not an address, so only `stream_host` helps.
	if !forwarded
		|| matches!(
			options.stream_certificate,
			stream::WebTransportCertificate::Files { .. }
		) {
		return None;
	}
	// The address that the reverse proxy connected to. The browser can usually reach it too.
	let ip = match local.ip() {
		IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
		ip => ip,
	};
	(!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// The `connect-src` source for the stream. CSP cannot name an IPv6 address, so those get any
/// host on the stream's port.
fn stream_source(stream_host: Option<&str>, page_host: &str, port: u16) -> String {
	let host = stream_host.unwrap_or_else(|| access::host_without_port(page_host));
	if host.is_empty() || host.contains(':') {
		format!("https://*:{port}")
	} else {
		format!("https://{host}:{port}")
	}
}

/// The browser's address from a reverse proxy's `X-Forwarded-For`, if it is there.
fn forwarded_client(forwarded_for: Option<&str>) -> Option<IpAddr> {
	let first = forwarded_for?.split(',').next()?.trim();
	first
		.parse::<IpAddr>()
		.ok()
		.or_else(|| first.parse::<SocketAddr>().ok().map(|address| address.ip()))
}

async fn action<B: Backend>(state: &State<B>, request: Request<Incoming>) -> Response<Full<Bytes>> {
	if header_str(&request, "x-moonshine-ui") != Some("1") {
		return text(
			StatusCode::FORBIDDEN,
			"Requests that change something must come from the control panel.",
		);
	}
	let is_json = header_str(&request, header::CONTENT_TYPE).is_some_and(|value| value.starts_with("application/json"));
	if !is_json {
		return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Expected JSON.");
	}
	let body = match Limited::new(request.into_body(), MAX_BODY).collect().await {
		Ok(body) => body.to_bytes(),
		Err(e) => return text(StatusCode::BAD_REQUEST, &format!("Failed to read the request: {e}")),
	};
	let action: Action = match serde_json::from_slice(&body) {
		Ok(action) => action,
		Err(e) => return text(StatusCode::BAD_REQUEST, &format!("The request is not valid: {e}")),
	};

	let result = state.backend.perform(action).await;
	state.poke.notify_one();
	match result {
		Ok(message) => json(
			StatusCode::OK,
			serde_json::json!({ "message": message }).to_string().into_bytes(),
		),
		Err(error) => json(
			StatusCode::UNPROCESSABLE_ENTITY,
			serde_json::json!({ "error": error }).to_string().into_bytes(),
		),
	}
}

/// A token for one WebTransport session, for the browser at `client` (`None`: not known).
fn stream_token<B: Backend>(
	state: &State<B>,
	request: &Request<Incoming>,
	client: Option<IpAddr>,
	host: Option<String>,
) -> Response<Full<Bytes>> {
	if header_str(request, "x-moonshine-ui") != Some("1") {
		return text(
			StatusCode::FORBIDDEN,
			"Requests for a stream must come from the control panel.",
		);
	}
	if !state.options.streaming {
		return json(
			StatusCode::FORBIDDEN,
			serde_json::json!({ "error": "Streaming in the browser is off. To turn it on, set `streaming = true` in the [ui] table of config.toml, then restart Moonshine." })
				.to_string()
				.into_bytes(),
		);
	}
	let Some(webtransport) = &state.webtransport else {
		return json(
			StatusCode::SERVICE_UNAVAILABLE,
			serde_json::json!({ "error": "Streaming to the browser is not available. The log of Moonshine says why." })
				.to_string()
				.into_bytes(),
		);
	};
	match webtransport.issue_token(client, host) {
		Ok(token) => json(StatusCode::OK, serde_json::to_vec(&token).unwrap_or_default()),
		Err(error) => json(
			StatusCode::INTERNAL_SERVER_ERROR,
			serde_json::json!({ "error": error }).to_string().into_bytes(),
		),
	}
}

/// Send the page everything it shows, then every change, until the page closes.
async fn websocket_session<B: Backend>(state: Arc<State<B>>, websocket: HyperWebsocket) {
	let mut websocket = match websocket.await {
		Ok(websocket) => websocket,
		Err(e) => {
			tracing::debug!("Control panel WebSocket failed to open: {e}");
			return;
		},
	};

	let mut snapshots = state.snapshot.clone();
	let (stream, buckets, mut stats) = state.stats.join();
	let mut logs = state.logs.subscribe();
	let tail = state.logs.tail(LOG_TAIL);

	let hello = [
		format!(
			r#"{{"type":"snapshot","data":{}}}"#,
			snapshots.borrow_and_update().as_str()
		),
		serde_json::json!({ "type": "stats_history", "stream": stream, "buckets": buckets }).to_string(),
		serde_json::json!({ "type": "logs", "lines": tail }).to_string(),
	];
	for message in hello {
		if websocket.send(Message::text(message)).await.is_err() {
			return;
		}
	}

	loop {
		let message = tokio::select! {
			changed = snapshots.changed() => match changed {
				Ok(()) => format!(r#"{{"type":"snapshot","data":{}}}"#, snapshots.borrow_and_update().as_str()),
				Err(_) => break,
			},
			message = stats.recv() => match message {
				Ok(message) => serde_json::to_string(&message).unwrap_or_default(),
				Err(broadcast::error::RecvError::Lagged(_)) => continue,
				Err(broadcast::error::RecvError::Closed) => break,
			},
			line = logs.recv() => {
				let mut lines = match line {
					Ok(line) => vec![line],
					Err(broadcast::error::RecvError::Lagged(_)) => Vec::new(),
					Err(broadcast::error::RecvError::Closed) => break,
				};
				while let Ok(line) = logs.try_recv() {
					lines.push(line);
				}
				serde_json::json!({ "type": "logs", "lines": lines }).to_string()
			},
			incoming = websocket.next() => match incoming {
				Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
				Some(Ok(_)) => continue,
			},
		};
		if websocket.send(Message::text(message)).await.is_err() {
			break;
		}
	}
}

/// Send a plain HTTP request to the same address over HTTPS. Browsers allow streaming (WebCodecs,
/// WebTransport, the Gamepad API) only on secure pages.
fn redirect_to_https<B>(host: &str, request: &Request<B>) -> Response<Full<Bytes>> {
	let target = request.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
	let location = format!("https://{host}{target}");
	let mut response = text(
		StatusCode::PERMANENT_REDIRECT,
		&format!("The control panel is at {location}."),
	);
	match HeaderValue::from_str(&location) {
		Ok(location) => {
			response.headers_mut().insert(header::LOCATION, location);
		},
		Err(_) => *response.status_mut() = StatusCode::BAD_REQUEST,
	}
	response
}

fn header_str<B>(request: &Request<B>, name: impl header::AsHeaderName) -> Option<&str> {
	request.headers().get(name).and_then(|value| value.to_str().ok())
}

fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
	let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
	*response.status_mut() = status;
	response.headers_mut().insert(
		header::CONTENT_TYPE,
		HeaderValue::from_static("text/plain; charset=utf-8"),
	);
	response
		.headers_mut()
		.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	response
}

fn json(status: StatusCode, body: Vec<u8>) -> Response<Full<Bytes>> {
	let mut response = Response::new(Full::new(Bytes::from(body)));
	*response.status_mut() = status;
	response
		.headers_mut()
		.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
	response
		.headers_mut()
		.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	response
}

fn asset(body: &'static [u8], content_type: &'static str) -> Response<Full<Bytes>> {
	let mut response = Response::new(Full::new(Bytes::from_static(body)));
	response
		.headers_mut()
		.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
	response
		.headers_mut()
		.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
	response
}

/// Add the headers that keep other pages from framing, sniffing or scripting the control panel.
fn secure(response: &mut Response<Full<Bytes>>, host: &str, stream_source: Option<&str>) {
	let stream_source = stream_source.unwrap_or_default();
	let csp = format!(
		"default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; \
		 font-src 'self'; connect-src 'self' ws://{host} wss://{host} {stream_source}; frame-ancestors 'none'; \
		 base-uri 'none'; form-action 'none'"
	);
	let headers = response.headers_mut();
	if let Ok(csp) = HeaderValue::from_str(&csp) {
		headers.insert(header::CONTENT_SECURITY_POLICY, csp);
	}
	headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
	headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
	headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}

#[cfg(test)]
mod tests {
	use super::*;

	fn options(stream_host: Option<&str>, trusted: bool) -> Options {
		Options {
			listen: Vec::new(),
			allowed_networks: Vec::new(),
			host_names: Vec::new(),
			tls: None,
			stream_certificate: if trusted {
				stream::WebTransportCertificate::Files {
					certificate: "cert.pem".into(),
					private_key: "key.pem".into(),
				}
			} else {
				stream::WebTransportCertificate::SelfSigned { names: Vec::new() }
			},
			stream_host: stream_host.map(str::to_string),
			streaming: true,
		}
	}

	#[test]
	fn a_proxied_page_streams_from_the_address_the_proxy_used() {
		let tailscale: SocketAddr = "100.95.89.123:47990".parse().unwrap();
		assert_eq!(stream_host(&options(None, false), false, tailscale), None);
		assert_eq!(
			stream_host(&options(None, false), true, tailscale).as_deref(),
			Some("100.95.89.123")
		);
		let mapped: SocketAddr = "[::ffff:100.95.89.123]:47990".parse().unwrap();
		assert_eq!(
			stream_host(&options(None, false), true, mapped).as_deref(),
			Some("100.95.89.123")
		);
		// A proxy on this computer connects over loopback, which the browser cannot use.
		let loopback: SocketAddr = "127.0.0.1:47990".parse().unwrap();
		assert_eq!(stream_host(&options(None, false), true, loopback), None);
		// A trusted certificate does not name the address.
		assert_eq!(stream_host(&options(None, true), true, tailscale), None);
		// The setting always wins.
		assert_eq!(
			stream_host(&options(Some("desktop.ts.net"), true), false, loopback).as_deref(),
			Some("desktop.ts.net")
		);
	}

	#[test]
	fn the_csp_allows_the_stream_host_and_port() {
		assert_eq!(
			stream_source(Some("100.95.89.123"), "moonshine.example.org", 47990),
			"https://100.95.89.123:47990"
		);
		assert_eq!(
			stream_source(None, "desktop.local:47990", 47990),
			"https://desktop.local:47990"
		);
		assert_eq!(stream_source(None, "[fd7a::1]:47990", 47990), "https://*:47990");
		assert_eq!(stream_source(Some("fd7a::1"), "x", 47990), "https://*:47990");
	}

	#[test]
	fn the_forwarded_client_is_the_first_address() {
		assert_eq!(
			forwarded_client(Some("100.64.0.9, 100.127.201.61")),
			Some("100.64.0.9".parse().unwrap())
		);
		assert_eq!(
			forwarded_client(Some("[fd7a::9]:1234")),
			Some("fd7a::9".parse().unwrap())
		);
		assert_eq!(forwarded_client(Some("unknown")), None);
		assert_eq!(forwarded_client(None), None);
	}

	#[test]
	fn a_proxy_name_is_an_allowed_origin_for_the_stream() {
		let names = vec!["moonshine.example.org".to_string()];
		assert!(access::origin_allowed(
			"https://moonshine.example.org",
			"100.95.89.123:47990",
			&names
		));
		assert!(!access::origin_allowed(
			"https://evil.example",
			"100.95.89.123:47990",
			&names
		));
	}
}
