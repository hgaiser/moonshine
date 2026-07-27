use is_terminal::IsTerminal;
use std::path::PathBuf;

use async_shutdown::ShutdownManager;
use clap::{Parser, Subcommand};
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use moonshine_core::clients::ClientManager;
use moonshine_core::config::Config;
use moonshine_core::discovery::MdnsDiscovery;
use moonshine_core::healthcheck::{self, CheckOutcome, HealthReport};
use moonshine_core::rtsp::RtspServer;
use moonshine_core::session::manager::SessionManager;
use moonshine_core::webserver::Webserver;
pub use moonshine_core::ShutdownReason;

#[derive(Parser, Debug)]
#[clap(version)]
#[command(subcommand_negates_reqs = true)]
struct Args {
	/// Path to the configuration file.
	#[arg(required = true)]
	config: PathBuf,

	/// Skip health checks on startup.
	#[arg(long)]
	no_health_check: bool,

	#[command(subcommand)]
	command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Run health checks and report results, then exit.
	Healthcheck {
		/// Path to configuration file (enables port availability and GPU preference checks).
		#[arg(short, long)]
		config: Option<PathBuf>,
	},
}

fn init_tracing() {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer().with_ansi(std::io::stdout().is_terminal()))
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("error")))
		.init();
}

/// Invoke `f` for every check with its outcome, name and message.
fn iter_checks(report: &HealthReport, mut f: impl FnMut(CheckOutcome, &str, &str)) {
	for check in &report.checks {
		f(check.outcome, check.name, &check.message);
	}
}

/// Return `(all_fatal_passed, fatal_count, warning_count)`.
fn health_summary(report: &HealthReport) -> (bool, usize, usize) {
	let fatal = report
		.checks
		.iter()
		.filter(|c| c.outcome == CheckOutcome::Failed)
		.count();
	let warn = report
		.checks
		.iter()
		.filter(|c| c.outcome == CheckOutcome::Warning)
		.count();
	(report.all_fatal_passed, fatal, warn)
}

fn log_health_report(report: &HealthReport) {
	iter_checks(report, |outcome, name, msg| match outcome {
		CheckOutcome::Passed => {
			tracing::debug!(target: "health", "{:>15}  OK   {msg}", name);
		},
		CheckOutcome::Failed => {
			tracing::error!(target: "health", "{:>15}  FAIL\n{msg}", name);
		},
		CheckOutcome::Warning => {
			tracing::warn!(target: "health", "{:>15}  WARN\n{msg}", name);
		},
	});

	let (passed, fatal, warn) = health_summary(report);
	if passed {
		if warn > 0 {
			tracing::info!(
				"Health checks passed in {}ms ({} warnings).",
				report.duration.as_millis(),
				warn
			);
		} else {
			tracing::info!("Health checks passed in {}ms.", report.duration.as_millis());
		}
	} else {
		tracing::error!(
			"Health checks FAILED in {}ms ({} errors, {} warnings). Fix issues above or use --no-health-check.",
			report.duration.as_millis(),
			fatal,
			warn,
		);
	}
}

fn print_health_report(report: &HealthReport) {
	let tty = std::io::stdout().is_terminal();
	let (red, green, yellow, reset) = if tty {
		("\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[m")
	} else {
		("", "", "", "")
	};

	iter_checks(report, |outcome, name, msg| match outcome {
		CheckOutcome::Passed => {
			println!("  {green}OK{reset}    {:>15}  {msg}", name);
		},
		CheckOutcome::Failed => {
			println!("  {red}FAIL{reset}  {:>15}", name);
			for line in msg.lines() {
				println!("        {line}");
			}
		},
		CheckOutcome::Warning => {
			println!("  {yellow}WARN{reset}  {:>15}", name);
			for line in msg.lines() {
				println!("        {line}");
			}
		},
	});

	let (passed, fatal, warn) = health_summary(report);
	println!();
	if passed {
		if warn > 0 {
			println!(
				"{green}Health checks passed{reset} in {}ms ({} warnings).",
				report.duration.as_millis(),
				warn
			);
		} else {
			println!(
				"{green}All health checks passed{reset} in {}ms.",
				report.duration.as_millis()
			);
		}
	} else {
		println!(
			"{red}Health checks FAILED{reset} in {}ms ({} errors, {} warnings).",
			report.duration.as_millis(),
			fatal,
			warn,
		);
	}
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	let args = Args::parse();

	// Standalone healthcheck subcommand — run checks and exit.
	if let Some(Command::Healthcheck { config: hc_config }) = args.command {
		init_tracing();
		let config = match hc_config.as_ref().map(Config::read_from_file) {
			Some(Ok(c)) => Some(c),
			Some(Err(())) => {
				tracing::warn!("Failed to load config, running checks without config.");
				None
			},
			None => None,
		};
		let report = tokio::task::spawn_blocking(move || healthcheck::run_healthcheck(config.as_ref()))
			.await
			.map_err(|_| ())?;
		print_health_report(&report);
		std::process::exit(if report.all_fatal_passed { 0 } else { 1 });
	}

	init_tracing();

	let config_path = &args.config;
	let mut config = Config::load_or_create(config_path)?;
	tracing::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = moonshine_core::app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);
	moonshine_core::app_scanner::resolve_missing_boxart(&mut config.applications);

	// GPU capability probes (codecs + HDR + DMA-BUF) always run so the server
	// advertises real support. DMA-BUF in particular is required for the video
	// pipeline, so its absence gates startup even when the full health check is
	// skipped. The full health check (ports, dependencies) only runs when not
	// explicitly skipped with --no-health-check.
	let (supported_codecs, hdr_supported, dma_buf_supported) = if args.no_health_check {
		tracing::info!("Health checks disabled (--no-health-check); probing GPU capabilities only.");
		let caps = tokio::task::spawn_blocking({
			let cfg = config.clone();
			move || healthcheck::probe_capabilities(Some(&cfg))
		})
		.await
		.map_err(|_| ())?;
		(caps.supported_codecs, caps.hdr_supported, caps.dma_buf_supported)
	} else {
		tracing::info!("Running health checks...");
		let report = tokio::task::spawn_blocking({
			let cfg = config.clone();
			move || healthcheck::run_healthcheck(Some(&cfg))
		})
		.await
		.map_err(|_| ())?;
		log_health_report(&report);
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

	// HDR is only advertised when both the probe detects HDR-capable formats
	// and the user enabled it in the configuration.
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
