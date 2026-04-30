use serde::{Deserialize, Serialize};
use std::{
	collections::hash_map::DefaultHasher,
	hash::{Hash, Hasher},
	path::{Path, PathBuf},
};

fn default_true() -> bool {
	true
}

fn default_false() -> bool {
	false
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
	/// Name of the Moonshine host.
	pub name: String,

	/// Address to bind to.
	pub address: String,

	/// Configuration for the webserver.
	pub webserver: WebserverConfig,

	/// Configuration for the streams.
	pub stream: StreamConfig,

	/// List of applications to expose to clients.
	#[serde(rename = "application")]
	pub applications: Vec<ApplicationConfig>,

	/// List of scanners that dynamically adds applications when started.
	#[serde(rename = "application_scanner")]
	#[serde(skip_serializing_if = "Vec::is_empty", default)]
	pub application_scanners: Vec<ApplicationScannerConfig>,

	/// Path to the DRM render node to use (e.g. /dev/dri/renderD128).
	pub gpu: Option<String>,

	/// Whether to advertise HDR support to clients.
	///
	/// When true (the default), the server tells clients that HDR is available.
	/// At session time the compositor still verifies that an HDR-capable render
	/// format is available and silently falls back to SDR if not. Set this to
	/// false to prevent clients from attempting HDR sessions on hardware that
	/// cannot deliver them.
	#[serde(default = "default_true")]
	pub hdr_support: bool,

	/// Time in seconds since last ping after which the stream closes.
	pub stream_timeout: u64,

	/// Optional OpenTelemetry exporter configuration. When `otlp_endpoint`
	/// is unset (default), telemetry is fully disabled — no spans, no
	/// metrics, no overhead. When set, moonshine ships per-frame traces
	/// and aggregated histograms/counters/gauges to the configured OTLP
	/// gRPC endpoint, which the user runs (Tempo, Jaeger, SigNoz, an
	/// otelcol passthrough, whatever).
	#[serde(default)]
	pub telemetry: TelemetryConfigToml,
}

/// TOML mirror of `crate::telemetry::TelemetryConfig`. Kept separate so
/// the telemetry module can stay free of serde / TOML knowledge.
///
/// Example:
///
/// ```toml
/// [telemetry]
/// otlp_endpoint = "http://localhost:4317"
/// trace_mode = "outliers"           # one of: "none", "outliers", "static"
/// trace_sample_rate = 0.05          # only used when trace_mode = "static"
/// metric_export_interval_ms = 10000
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TelemetryConfigToml {
	#[serde(default)]
	pub otlp_endpoint: Option<String>,
	#[serde(default)]
	pub service_name: Option<String>,
	/// `"none"`, `"outliers"`, or `"static"`. Default: `"outliers"`.
	#[serde(default)]
	pub trace_mode: Option<String>,
	/// Trace sampling rate (0.0–1.0). Only consulted when
	/// `trace_mode = "static"`.
	#[serde(default)]
	pub trace_sample_rate: Option<f64>,
	/// Metric export interval in milliseconds.
	#[serde(default)]
	pub metric_export_interval_ms: Option<u64>,
}

impl Config {
	#[allow(clippy::result_unit_err)]
	pub fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
		let config =
			std::fs::read_to_string(file).map_err(|e| tracing::warn!("Failed to open configuration file: {e}"))?;
		let config: Config =
			toml::from_str(&config).map_err(|e| tracing::warn!("Failed to parse configuration file: {e}"))?;

		Ok(config)
	}
}

impl Default for Config {
	fn default() -> Self {
		Self {
			name: "Moonshine".to_string(),
			address: "0.0.0.0".to_string(),
			webserver: Default::default(),
			stream: Default::default(),
			applications: vec![ApplicationConfig {
				title: "Steam".to_string(),
				command: vec!["/usr/bin/steam".to_string(), "steam://open/bigpicture".to_string()],
				boxart: None,
			}],
			application_scanners: vec![ApplicationScannerConfig::Steam(SteamApplicationScannerConfig {
				library: "$HOME/.local/share/Steam".into(),
				command: vec![
					"/usr/bin/steam".to_string(),
					"-bigpicture".to_string(),
					"steam://rungameid/{game_id}".to_string(),
				],
			})],
			gpu: None,
			hdr_support: true,
			telemetry: TelemetryConfigToml::default(),
			stream_timeout: 60,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebserverConfig {
	/// Port of the webserver.
	pub port: u16,

	/// Port of the HTTPS webserver.
	pub port_https: u16,

	/// Whether to allow new clients to pair.
	#[serde(default = "default_true")]
	pub enable_pairing: bool,

	/// Path to the certificate for SSL encryption.
	pub certificate: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

impl Default for WebserverConfig {
	fn default() -> Self {
		Self {
			port: 47989,
			port_https: 47984,
			enable_pairing: true,
			certificate: "$HOME/.config/moonshine/cert.pem".into(),
			private_key: "$HOME/.config/moonshine/key.pem".into(),
		}
	}
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ApplicationConfig {
	/// Title of the application.
	pub title: String,

	/// Path to a boxart image.
	pub boxart: Option<PathBuf>,

	/// The command to run.
	pub command: Vec<String>,
}

impl ApplicationConfig {
	pub fn id(&self) -> i32 {
		let mut hasher = DefaultHasher::new();
		self.title.hash(&mut hasher);
		hasher.finish() as i32
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ApplicationScannerConfig {
	/// Scans a 'libraryfolders.vdf' file from a Steam library directory.
	Steam(SteamApplicationScannerConfig),

	/// Scans directories containing freedesktop .desktop launchers.
	Desktop(DesktopApplicationScannerConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteamApplicationScannerConfig {
	/// Path to a Steam library (ie. `~/.local/share/Steam`).
	pub library: PathBuf,

	/// The command to run.
	pub command: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopApplicationScannerConfig {
	/// Directories to scan recursively for `.desktop` files.
	pub directories: Vec<PathBuf>,

	/// Whether terminal-based entries should be included.
	#[serde(default = "default_false")]
	pub include_terminal: bool,

	/// Whether to resolve desktop entry icons into Moonshine boxart paths.
	#[serde(default = "default_true")]
	pub resolve_icons: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamConfig {
	/// Port to bind the RTSP server to.
	pub port: u16,

	/// Configuration for the video stream.
	pub video: VideoStreamConfig,

	/// Configuration for the audio stream.
	pub audio: AudioStreamConfig,

	/// Configuration for the control stream.
	pub control: ControlStreamConfig,
}

impl Default for StreamConfig {
	fn default() -> Self {
		Self {
			port: 48010,
			video: Default::default(),
			audio: Default::default(),
			control: Default::default(),
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoStreamConfig {
	/// Port to use for streaming video data.
	pub port: u16,

	/// What percentage of data packets should be parity packets.
	pub fec_percentage: u8,

	/// Whether to enable video stream encryption (AES-128-GCM).
	///
	/// When enabled, the server advertises video encryption support to clients.
	/// The client must also support and enable video encryption for it to be active.
	/// Disabled by default for compatibility with older clients.
	#[serde(default = "default_false")]
	pub encrypt: bool,

	/// Whether to emit a WARN log when a single frame takes longer to encode and
	/// send than the target frame interval (i.e. a latency spike).
	///
	/// Disabled by default because spikes are common during normal operation and
	/// the messages can be very noisy.
	#[serde(default = "default_false")]
	pub log_frame_spikes: bool,
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			fec_percentage: 20,
			encrypt: false,
			log_frame_spikes: false,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStreamConfig {
	/// Port to use for streaming audio data.
	pub port: u16,
}

impl Default for AudioStreamConfig {
	fn default() -> Self {
		Self { port: 48000 }
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlStreamConfig {
	/// Port to use for streaming control data.
	pub port: u16,
}

impl Default for ControlStreamConfig {
	fn default() -> Self {
		Self { port: 47999 }
	}
}
