use async_shutdown::ShutdownManager;
use clients::ClientManager;
use config::Config;
use openssl::pkey::PKey;
use session::SessionManager;
use webserver::Webserver;

pub mod clients;
pub mod config;
pub mod cuda;
pub mod session;
pub mod publisher;
pub mod util;
pub mod webserver;

pub struct Moonshine {
	webserver: Webserver,
}

impl Moonshine {
	pub fn new(
		config: Config,
		shutdown: ShutdownManager<i32>,
	) -> Result<Self, ()> {
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

		// Create a handler for the webserver.
		let webserver = Webserver::new(
			config,
			server_certs,
			client_manager,
			session_manager,
			shutdown,
		)?;

		Ok(Self { webserver })
	}

	pub async fn stop(&self) -> Result<(), ()> {
		self.webserver.stop().await
	}
}