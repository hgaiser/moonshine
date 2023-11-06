use std::path::Path;

use async_shutdown::ShutdownManager;
use clients::ClientManager;
use config::Config;
use openssl::pkey::PKey;
use rtsp::RtspServer;
use serde::{Deserialize, Serialize};
use session::SessionManager;
use webserver::Webserver;

pub mod clients;
pub mod config;
pub mod crypto;
pub mod cuda;
pub mod rtsp;
pub mod session;
pub mod publisher;
pub mod util;
pub mod webserver;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
	unique_id: String,
}

impl State {
	fn new() -> Self {
		Self { unique_id: uuid::Uuid::new_v4().to_string() }
	}

	fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Self, ()> {
		let state = std::fs::read_to_string(file)
			.map_err(|e| log::error!("Failed to open state file: {e}"))?;
		let state: State = toml::from_str(&state)
			.map_err(|e| log::error!("Failed to parse state file: {e}"))?;

		Ok(state)
	}

	fn save<P: AsRef<Path>>(&self, file: P) -> Result<(), ()> {
		let parent_dir = file.as_ref().parent().ok_or_else(|| log::error!("Failed to get state dir for file {:?}", file.as_ref()))?;
		std::fs::create_dir_all(parent_dir)
			.map_err(|e| log::error!("Failed to create state dir: {e}"))?;

		std::fs::write(file, toml::to_string_pretty(self).map_err(|e| log::error!("Failed to serialize state: {e}"))?)
			.map_err(|e| log::error!("Failed to save state file: {e}"))
	}
}

pub struct Moonshine {
	_rtsp_server: RtspServer,
	_session_manager: SessionManager,
	_client_manager: ClientManager,
	_webserver: Webserver,
}

impl Moonshine {
	pub fn new(
		config: Config,
		shutdown: ShutdownManager<i32>,
	) -> Result<Self, ()> {
		let state_path = dirs::data_dir()
			.ok_or_else(|| log::error!("Failed to get state dir."))?
			.join("moonshine")
			.join("state.toml");

		let state;
		if state_path.exists() {
			state = State::read_from_file(state_path)?;
		} else {
			state = State::new();
			state.save(state_path)?;
		}

		let server_certs = std::fs::read(&config.webserver.certificate_chain)
			.map_err(|e| log::error!("Failed to read server certificate: {e}"))?;
		let server_certs = openssl::x509::X509::from_pem(&server_certs)
			.map_err(|e| log::error!("Failed to parse server certificate: {e}"))?;
		let server_pkey = PKey::private_key_from_pem(&std::fs::read(&config.webserver.private_key)
			.map_err(|e| log::error!("Failed to read private key: {e}"))?)
			.map_err(|e| log::error!("Failed to parse private key: {e}"))?;

		// Create a manager for interacting with sessions.
		let session_manager = SessionManager::new(config.clone(), shutdown.trigger_shutdown_token(2))?;

		// Create a manager for saving and loading client state.
		let client_manager = ClientManager::new(server_certs.clone(), server_pkey, shutdown.trigger_shutdown_token(3))?;

		// Run the RTSP server.
		let rtsp_server = RtspServer::new(config.clone(), session_manager.clone(), shutdown.clone());

		// Publish the Moonshine service using zeroconf.
		publisher::spawn(config.webserver.port, config.name.clone(), shutdown.clone());

		// Create a handler for the webserver.
		let webserver = Webserver::new(
			config,
			&state.unique_id,
			server_certs,
			client_manager.clone(),
			session_manager.clone(),
			shutdown,
		)?;

		Ok(Self {
			_rtsp_server: rtsp_server,
			_session_manager: session_manager,
			_client_manager: client_manager,
			_webserver: webserver,
		})
	}
}