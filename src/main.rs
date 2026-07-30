use moonshine_core::app_scanner;
use moonshine_core::healthcheck;
use std::io::IsTerminal;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub use moonshine_core::ShutdownReason;
use moonshine_core::clients::ClientManager;
use moonshine_core::config::Config;
use moonshine_core::discovery::MdnsDiscovery;
use moonshine_core::rtsp::RtspServer;
use moonshine_core::session::manager::SessionManager;
use moonshine_core::webserver::Webserver;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to the configuration file.
	config: Option<PathBuf>,

	/// Skip health checks on startup.
	#[arg(long)]
	no_health_check: bool,

	#[command(subcommand)]
	command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Run health checks and report results, then exit.
	Healthcheck,
}

fn default_config_path() -> PathBuf {
	match std::env::var("XDG_CONFIG_HOME") {
		Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("moonshine/config.toml"),
		_ => {
			let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
			PathBuf::from(home).join(".config/moonshine/config.toml")
		},
	}
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	let args = Args::parse();

	init_tracing();

	let config_path = args.config.unwrap_or_else(default_config_path);
	let mut config = Config::load_or_create(&config_path)?;

	// Standalone healthcheck subcommand — run checks and exit.
	if let Some(Command::Healthcheck) = args.command {
		let report = tokio::task::spawn_blocking(move || healthcheck::run_healthcheck(Some(&config)))
			.await
			.map_err(|e| tracing::error!("Failed to run health check task: {e}"))?;
		healthcheck::print_health_report(&report);
		std::process::exit(if report.all_fatal_passed { 0 } else { 1 });
	}

	tracing::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);
	app_scanner::resolve_missing_boxart(&mut config.applications);

	tracing::debug!("Waiting for D-Bus session bus...");
	wait_for_dbus().await?;
	tracing::debug!("D-Bus session bus available.");

	// Run health checks unless the user explicitly disabled them.
	// If health checks are disabled, we still probe the GPU for supported codecs and HDR support.
	let (supported_codecs, hdr_supported, dma_buf_supported) = if args.no_health_check {
		tracing::info!("Health checks disabled (--no-health-check); probing GPU capabilities only.");
		let capabilities = tokio::task::spawn_blocking({
			let cfg = config.clone();
			move || healthcheck::probe_capabilities(Some(&cfg))
		})
		.await
		.map_err(|e| tracing::error!("Failed to run health check task: {e}"))?;

		(
			capabilities.supported_codecs,
			capabilities.hdr_supported,
			capabilities.dma_buf_supported,
		)
	} else {
		tracing::debug!("Running health checks...");
		let report = tokio::task::spawn_blocking({
			let cfg = config.clone();
			move || healthcheck::run_healthcheck(Some(&cfg))
		})
		.await
		.map_err(|e| tracing::error!("Failed to run health check task: {e}"))?;

		healthcheck::log_health_report(&report);
		if !report.all_fatal_passed {
			return Err(());
		}

		(report.supported_codecs, report.hdr_supported, report.dma_buf_supported)
	};

	if !dma_buf_supported {
		tracing::error!(
			"DMA-BUF import is not supported by the GPU/Vulkan driver. \
			 This is required for video encoding; refusing to start. \
			 Update GPU drivers to the latest version."
		);
		return Err(());
	}

	// HDR is only advertised when both the probe detects HDR-capable formats and the user enabled it in the configuration.
	let hdr_supported = hdr_supported && config.compositor.hdr;

	let shutdown = ShutdownManager::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			let mut terminate = signal(SignalKind::terminate()).unwrap();
			tokio::select! {
				_ = tokio::signal::ctrl_c() => {
					tracing::info!("Received SIGINT, shutting down...");
				},
				_ = terminate.recv() => {
					tracing::info!("Received SIGTERM, shutting down...");
				}
			}
			shutdown.trigger_shutdown(ShutdownReason::AppQuit).ok();
		}
	});

	let moonshine = Moonshine::new(config, supported_codecs, hdr_supported, shutdown.clone())?;
	tracing::info!("Moonshine is ready and waiting for connections.");

	shutdown.wait_shutdown_triggered().await;
	drop(moonshine);

	let exit_code = shutdown.wait_shutdown_complete().await;
	tracing::debug!("Successfully waited for shutdown to complete.");
	std::process::exit(exit_code as i32);
}

fn init_tracing() {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer().with_ansi(std::io::stdout().is_terminal()))
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("error")))
		.init();
}

async fn wait_for_dbus() -> Result<(), ()> {
	let mut terminate =
		signal(SignalKind::terminate()).map_err(|e| tracing::error!("Failed to bind to SIGTERM signal: {e}"))?;
	let mut interrupt =
		signal(SignalKind::interrupt()).map_err(|e| tracing::error!("Failed to bind to SIGINT signal: {e}"))?;

	loop {
		tokio::select! {
			_ = terminate.recv() => {
				tracing::info!("Received SIGTERM while waiting for D-Bus, exiting.");
				return Err(());
			},
			_ = interrupt.recv() => {
				tracing::info!("Received SIGINT while waiting for D-Bus, exiting.");
				return Err(());
			},
			res = zbus::Connection::session() => {
				match res {
					Ok(_conn) => {
						return Ok(())
					},
					Err(e) => {
						tracing::warn!("Failed to connect to D-Bus session bus: {e}. Retrying in 20 seconds...");
						tokio::time::sleep(std::time::Duration::from_secs(20)).await;
					},
				}
			}
		}
	}
}

pub struct Moonshine {
	_rtsp_server: RtspServer,
	_session_manager: SessionManager,
	_client_manager: ClientManager,
	_webserver: Webserver,
	_discovery: MdnsDiscovery,
}

impl Moonshine {
	#[allow(clippy::result_unit_err)]
	pub fn new(
		config: Config,
		supported_codecs: u32,
		hdr_supported: bool,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		let (cert, pkey) = moonshine_core::tls::load_or_create_certificate(&config)?;

		let session_manager = SessionManager::new(
			config.compositor.clone(),
			config.stream.video.clone(),
			config.stream.audio.clone(),
			config.stream.control.clone(),
			config.address.clone(),
			config.stream.timeout,
			config.inhibit_sleep,
			shutdown.clone(),
		)?;
		let client_manager = ClientManager::new(cert.clone(), pkey.clone())?;

		Ok(Self {
			_rtsp_server: RtspServer::new(
				config.address.clone(),
				config.stream.port,
				config.stream.video.clone(),
				config.stream.audio.clone(),
				config.stream.control.clone(),
				session_manager.clone(),
				shutdown.clone(),
			),
			_session_manager: session_manager.clone(),
			_client_manager: client_manager.clone(),
			_webserver: Webserver::new(
				config.name.clone(),
				config.address.clone(),
				config.stream.port,
				config.webserver.clone(),
				config.applications.clone(),
				supported_codecs,
				hdr_supported,
				client_manager.persistent_state().get_uuid()?.to_string(),
				cert,
				client_manager,
				session_manager,
				shutdown.clone(),
			)?,
			_discovery: MdnsDiscovery::spawn(&config.address, config.webserver.port, &config.name),
		})
	}
}
