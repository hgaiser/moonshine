//! The web control panel of Moonshine.
//!
//! This crate serves the page, its JSON API and a WebSocket with live updates. It knows nothing
//! about Moonshine itself: a [`Backend`] gives it the data and performs the actions, so the
//! `moonshine` crate can depend on this one.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
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
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;

mod access;
mod logs;
pub mod protocol;
mod stats;

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
}

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
	snapshot: watch::Receiver<Arc<String>>,
	poke: Arc<Notify>,
	stats: StatsHistory,
	logs: LogBuffer,
}

/// Start the control panel. Fails when it cannot listen on any address.
pub async fn serve<B: Backend>(backend: Arc<B>, options: Options, logs: LogBuffer) -> Result<UiServer, String> {
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

	let state = Arc::new(State {
		backend,
		options,
		snapshot: snapshot_rx,
		poke,
		stats,
		logs,
	});

	for listener in listeners {
		if let Ok(address) = listener.local_addr() {
			tracing::info!("Control panel listening on http://{address}");
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

		let state = state.clone();
		tokio::spawn(async move {
			let service = service_fn(move |request| {
				let state = state.clone();
				async move { Ok::<_, Infallible>(handle(state, request, allowed).await) }
			});
			let _ = hyper::server::conn::http1::Builder::new()
				.serve_connection(TokioIo::new(connection), service)
				.with_upgrades()
				.await;
		});
	}
}

async fn handle<B: Backend>(
	state: Arc<State<B>>,
	mut request: Request<Incoming>,
	allowed: bool,
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
		(&Method::GET, "/banner.png") => asset(include_bytes!("../../assets/banner-ui.png"), "image/png"),
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
		_ => text(StatusCode::NOT_FOUND, "Not found."),
	};

	secure(&mut response, &host);
	response
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
fn secure(response: &mut Response<Full<Bytes>>, host: &str) {
	let csp = format!(
		"default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; \
		 font-src 'self'; connect-src 'self' ws://{host} wss://{host}; frame-ancestors 'none'; \
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
