use moonshine::healthcheck;
use std::io::IsTerminal;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use moonshine::ShutdownReason;
use moonshine::app_scanner::ApplicationManager;
use moonshine::clients::ClientManager;
use moonshine::config::Config;
use moonshine::discovery::MdnsDiscovery;
use moonshine::rtsp::RtspServer;
use moonshine::session::manager::SessionManager;
use moonshine::webserver::Webserver;

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

	#[cfg(feature = "ui")]
	let logs = moonshine_ui::LogBuffer::new();
	init_tracing(
		#[cfg(feature = "ui")]
		&logs,
	);

	let config_path = args.config.unwrap_or_else(default_config_path);
	let config = Config::load_or_create(&config_path)?;

	// Standalone healthcheck subcommand — run checks and exit.
	if let Some(Command::Healthcheck) = args.command {
		let report = tokio::task::spawn_blocking(move || healthcheck::run_healthcheck(Some(&config)))
			.await
			.map_err(|e| tracing::error!("Failed to run health check task: {e}"))?;
		healthcheck::print_health_report(&report);
		std::process::exit(if report.all_fatal_passed { 0 } else { 1 });
	}

	tracing::debug!("Using configuration:\n{:#?}", config);

	tracing::debug!("Waiting for D-Bus session bus...");
	wait_for_dbus().await?;
	tracing::debug!("D-Bus session bus available.");

	// Run health checks unless the user explicitly disabled them.
	// If health checks are disabled, we still probe the GPU for supported codecs and HDR support.
	let mut health_report = None;
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

		let capabilities = (report.supported_codecs, report.hdr_supported, report.dma_buf_supported);
		health_report = Some(report);
		capabilities
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

	#[cfg_attr(not(feature = "ui"), allow(unused_mut))]
	let mut moonshine = Moonshine::new(config.clone(), supported_codecs, hdr_supported, shutdown.clone())?;

	#[cfg(feature = "ui")]
	let restart_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
	#[cfg(feature = "ui")]
	{
		moonshine._ui = moonshine::ui::spawn(moonshine::ui::UiContext {
			config,
			config_path,
			session_manager: moonshine._session_manager.clone(),
			client_manager: moonshine._client_manager.clone(),
			application_manager: moonshine._application_manager.clone(),
			webserver: moonshine._webserver.clone(),
			health: health_report,
			supported_codecs,
			hdr_supported,
			logs,
			restart_requested: restart_requested.clone(),
			shutdown: shutdown.clone(),
		})
		.await
		.unwrap_or_else(|()| {
			tracing::warn!("The control panel is not available.");
			None
		});
	}
	#[cfg(not(feature = "ui"))]
	let _ = (health_report, config);

	tracing::info!("Moonshine is ready and waiting for connections.");

	shutdown.wait_shutdown_triggered().await;
	drop(moonshine);

	let exit_code = shutdown.wait_shutdown_complete().await;
	tracing::debug!("Successfully waited for shutdown to complete.");

	#[cfg(feature = "ui")]
	if restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
		restart();
	}

	std::process::exit(exit_code as i32);
}

/// Replace this process with a new Moonshine that has the same arguments.
#[cfg(feature = "ui")]
fn restart() {
	use std::os::unix::process::CommandExt;

	tracing::info!("Restarting Moonshine.");
	let error = match std::env::current_exe() {
		Ok(exe) => std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec(),
		Err(e) => e,
	};
	tracing::error!("Failed to restart Moonshine: {error}");
}

fn init_tracing(#[cfg(feature = "ui")] logs: &moonshine_ui::LogBuffer) {
	use tracing_subscriber::Layer;

	let terminal = tracing_subscriber::fmt::layer()
		.with_ansi(std::io::stdout().is_terminal())
		.with_filter(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("error")));
	let registry = tracing_subscriber::registry().with(terminal);

	// The control panel shows more than the terminal does by default.
	#[cfg(feature = "ui")]
	let registry = registry.with(logs.layer().with_filter(EnvFilter::new("info,moonshine=debug")));

	registry.init();
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
	_application_manager: ApplicationManager,
	_webserver: Webserver,
	_discovery: MdnsDiscovery,
	#[cfg(feature = "ui")]
	_ui: Option<moonshine_ui::UiServer>,
}

impl Moonshine {
	#[allow(clippy::result_unit_err)]
	pub fn new(
		config: Config,
		supported_codecs: u32,
		hdr_supported: bool,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		let (cert, pkey) = moonshine::tls::load_or_create_certificate(&config)?;

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
		if let Some(path) = &config.stats.socket {
			moonshine::stats_socket::spawn(path.clone(), &session_manager, shutdown.clone())?;
		}
		let application_manager = ApplicationManager::with_hidden(
			config.applications.clone(),
			config.application_scanners.clone(),
			config.hidden_applications.clone(),
		);
		let webserver = Webserver::new(
			config.name.clone(),
			config.address.clone(),
			config.stream.port,
			config.webserver.clone(),
			application_manager.clone(),
			supported_codecs,
			hdr_supported,
			client_manager.persistent_state().get_uuid()?.to_string(),
			cert,
			client_manager.clone(),
			session_manager.clone(),
			shutdown.clone(),
		)?;

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
			_session_manager: session_manager,
			_client_manager: client_manager,
			_application_manager: application_manager,
			_webserver: webserver,
			_discovery: MdnsDiscovery::spawn(&config.address, config.webserver.port, &config.name),
			#[cfg(feature = "ui")]
			_ui: None,
		})
	}
}
