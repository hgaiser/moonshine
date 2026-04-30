use std::io::Write;
use std::path::PathBuf;

use crate::clients::ClientManager;
use crate::config::Config;
use crate::crypto::create_certificate;
use crate::rtsp::RtspServer;
use crate::session::SessionManager;
use crate::state::State;
use crate::webserver::Webserver;
use async_shutdown::ShutdownManager;
use clap::{Parser, Subcommand};
use tokio::signal::unix::{signal, SignalKind};

mod app_scanner;
mod bench;
mod clients;
mod config;
mod crypto;
mod gpu_stats;
mod publisher;
mod rtsp;
mod session;
mod state;
mod telemetry;
mod webserver;

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to configuration file.
	config: PathBuf,

	/// Override the OTLP exporter endpoint from the config (e.g.
	/// `http://localhost:4317`). Useful for ad-hoc profiling without
	/// editing config.toml. Empty string disables telemetry even if the
	/// config enables it.
	#[arg(long, global = true)]
	otlp_endpoint: Option<String>,

	/// Override per-frame trace emission mode: `none`, `outliers`, or
	/// `static`. Use with `--trace-sample-rate` for `static`.
	#[arg(long, global = true, value_parser = parse_trace_mode_cli)]
	trace_mode: Option<String>,

	/// Static-mode trace sampling rate (0.0–1.0). Only consulted when
	/// `--trace-mode static`.
	#[arg(long, global = true)]
	trace_sample_rate: Option<f64>,

	#[command(subcommand)]
	command: Option<Command>,
}

fn parse_trace_mode_cli(s: &str) -> Result<String, String> {
	match s {
		"none" | "outliers" | "static" => Ok(s.to_string()),
		other => Err(format!("expected one of: none, outliers, static (got '{other}')")),
	}
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Run the full pipeline (compositor + capture + convert + encode) without
	/// a Moonlight client. Encoded packets are dropped; per-frame latency is
	/// reported when the run ends.
	Bench(bench::BenchArgs),
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), ()> {
	let args = Args::parse();

	// Config load runs before telemetry init so `[telemetry]` settings
	// take effect. tracing! calls during config load won't appear because
	// no subscriber is installed yet — pre-init failures fall back to
	// eprintln! below.

	// Ensure rustls has a single crypto provider selected at process start.
	// When multiple crypto backends (ring, aws-lc-rs) are present the crate
	// requires an explicit choice. Install the default provider now.
	// Prefer the `ring` provider explicitly to avoid runtime ambiguity
	// when multiple crypto backends are present in the dependency graph.
	// Construct the provider from the `ring` module and install it.
	let provider = rustls::crypto::ring::default_provider();
	let _ = provider.install_default();

	let mut config;
	if args.config.exists() {
		config = Config::read_from_file(args.config).unwrap_or_else(|()| std::process::exit(1));
	} else {
		tracing::info!(
			"No config file found at {}, creating a default config file.",
			args.config.display()
		);
		config = Config::default();

		let serialized_config =
			toml::to_string_pretty(&config).map_err(|e| tracing::error!("Failed to serialize config: {e}"))?;

		let config_dir = args
			.config
			.parent()
			.ok_or_else(|| tracing::error!("Failed to get parent directory of config file."))?;
		std::fs::create_dir_all(config_dir).map_err(|e| tracing::error!("Failed to create config directory: {e}"))?;
		std::fs::write(args.config, serialized_config)
			.map_err(|e| tracing::error!("Failed to save config file: {e}"))?;
	}

	// Resolve these paths so that the rest of the code doesn't need to.
	let cert_path = config.webserver.certificate.to_string_lossy().to_string();
	let cert_path =
		shellexpand::full(&cert_path).map_err(|e| tracing::error!("Failed to expand certificate path: {e}"))?;
	config.webserver.certificate = cert_path.to_string().into();

	let private_key_path = config.webserver.private_key.to_string_lossy().to_string();
	let private_key_path =
		shellexpand::full(&private_key_path).map_err(|e| tracing::error!("Failed to expand private key path: {e}"))?;
	config.webserver.private_key = private_key_path.to_string().into();

	// Install the real subscriber + (optional) OTel pipelines. CLI override
	// wins over config; empty-string CLI override disables.
	let telemetry_cfg = telemetry::TelemetryConfig {
		otlp_endpoint: args
			.otlp_endpoint
			.clone()
			.filter(|s| !s.is_empty())
			.or_else(|| config.telemetry.otlp_endpoint.clone()),
		service_name: config.telemetry.service_name.clone(),
		// Trace mode resolution priority:
		//   1) --trace-mode CLI (with --trace-sample-rate for static)
		//   2) [telemetry] trace_mode in config (with trace_sample_rate)
		//   3) Default: bench → Static(1.0) (full fidelity), else Outliers
		trace_mode: {
			let mode_str = args.trace_mode.clone().or_else(|| config.telemetry.trace_mode.clone());
			let cli_rate = args.trace_sample_rate.or(config.telemetry.trace_sample_rate);
			match mode_str.as_deref() {
				Some("none") => telemetry::TraceMode::None,
				Some("outliers") => telemetry::TraceMode::Outliers,
				Some("static") => telemetry::TraceMode::Static(cli_rate.unwrap_or(0.05)),
				Some(_) | None => {
					if matches!(args.command, Some(Command::Bench(_))) {
						// Bench is short and we want everything.
						telemetry::TraceMode::Static(1.0)
					} else {
						telemetry::TraceMode::Outliers
					}
				},
			}
		},
		metric_export_interval: config
			.telemetry
			.metric_export_interval_ms
			.map(std::time::Duration::from_millis)
			.unwrap_or(std::time::Duration::from_secs(10)),
	};
	let _telemetry = telemetry::init(&telemetry_cfg).map_err(|e| tracing::error!("telemetry init: {e}"))?;

	tracing::debug!("Using configuration:\n{:#?}", config);

	let scanned_applications = app_scanner::scan_applications(&config.application_scanners);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	config.applications.extend(scanned_applications);

	app_scanner::resolve_missing_boxart(&mut config.applications);

	// Spawn a task to wait for CTRL+C and trigger a shutdown.
	let shutdown = ShutdownManager::new();
	tokio::spawn({
		let shutdown = shutdown.clone();
		async move {
			let mut terminate = signal(SignalKind::terminate()).unwrap();

			tokio::select! {
				_ = tokio::signal::ctrl_c() => {
					tracing::info!("Received CTRL+C, shutting down...");
				},
				_ = terminate.recv() => {
					tracing::info!("Received SIGTERM, shutting down...");
				}
			}

			shutdown.trigger_shutdown(1).ok();
		}
	});

	match args.command {
		Some(Command::Bench(bench_args)) => {
			let result = bench::run(config, bench_args, shutdown.clone()).await;
			// Drain pending OTel exports synchronously before exit. Bench
			// runs are short enough that the BatchSpanProcessor's scheduled
			// flush can lose the trailing window otherwise.
			_telemetry.force_flush();
			let _ = shutdown.trigger_shutdown(result.map(|_| 0).unwrap_or(1));
			let exit_code = shutdown.wait_shutdown_complete().await;
			std::process::exit(exit_code);
		},
		None => {
			// Create the main application.
			let moonshine = Moonshine::new(config, shutdown.clone()).await?;

			tracing::info!("Moonshine is ready and waiting for connections.");

			// Wait until something causes a shutdown trigger.
			shutdown.wait_shutdown_triggered().await;

			// Drop the main moonshine object, triggering other systems to shutdown too.
			drop(moonshine);

			// Wait until everything was shutdown.
			let exit_code = shutdown.wait_shutdown_complete().await;
			tracing::debug!("Successfully waited for shutdown to complete.");
			std::process::exit(exit_code);
		},
	}
}

pub struct Moonshine {
	_rtsp_server: RtspServer,
	_session_manager: SessionManager,
	_client_manager: ClientManager,
	_webserver: Webserver,
}

impl Moonshine {
	pub async fn new(config: Config, shutdown: ShutdownManager<i32>) -> Result<Self, ()> {
		let state = State::new().await?;

		let (cert, pkey) = if !config.webserver.certificate.exists() && !config.webserver.private_key.exists() {
			tracing::info!("No certificate found, creating a new one.");

			let (cert, pkey) =
				create_certificate().map_err(|e| tracing::error!("Failed to create certificate: {e}"))?;

			// Write certificate to file.
			let cert_dir = config
				.webserver
				.certificate
				.parent()
				.ok_or_else(|| tracing::error!("Failed to find parent directory for certificate file."))?;
			std::fs::create_dir_all(cert_dir)
				.map_err(|e| tracing::error!("Failed to create certificate directory: {e}"))?;
			let mut certfile = std::fs::File::create(&config.webserver.certificate).unwrap();
			certfile
				.write(cert.as_bytes())
				.map_err(|e| tracing::error!("Failed to write PEM to file: {e}"))?;

			// Write private key to file.
			let private_key_dir = config
				.webserver
				.private_key
				.parent()
				.ok_or_else(|| tracing::error!("Failed to find parent directory for private key file."))?;
			std::fs::create_dir_all(private_key_dir)
				.map_err(|e| tracing::error!("Failed to create private key directory: {e}"))?;
			let mut keyfile = std::fs::File::create(&config.webserver.private_key).unwrap();
			keyfile
				.write(pkey.as_bytes())
				.map_err(|e| tracing::error!("Failed to write private key to file: {e}"))?;

			tracing::debug!("Saved private key to {}", config.webserver.certificate.display());
			tracing::debug!("Saved certificate to {}", config.webserver.private_key.display());

			(cert, pkey)
		} else {
			let cert = std::fs::read_to_string(&config.webserver.certificate)
				.map_err(|e| tracing::error!("Failed to read server certificate: {e}"))?;

			let pkey = std::fs::read_to_string(&config.webserver.private_key)
				.map_err(|e| tracing::error!("Failed to read private key: {e}"))?;

			(cert, pkey)
		};

		// Create a manager for interacting with sessions.
		let session_manager = SessionManager::new(config.clone(), shutdown.clone())?;

		// Create a manager for saving and loading client state.
		let client_manager = ClientManager::new(state.clone(), cert.clone(), pkey, shutdown.trigger_shutdown_token(3));

		// Run the RTSP server.
		let rtsp_server = RtspServer::new(config.clone(), session_manager.clone(), shutdown.clone());

		// Publish the Moonshine service using zeroconf.
		publisher::spawn(config.webserver.port, config.name.clone());

		// Create a handler for the webserver.
		let webserver = Webserver::new(
			config,
			state.get_uuid().await?,
			cert,
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
