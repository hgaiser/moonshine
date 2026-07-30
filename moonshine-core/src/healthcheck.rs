use std::ffi::CStr;
use std::io::IsTerminal;
use std::io::Write;
use std::os::linux::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pixelforge::{Codec, VideoContext, VideoContextBuilder};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
	Passed,
	Warning,
	Failed,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
	pub name: &'static str,
	pub outcome: CheckOutcome,
	pub message: String,
	pub duration_ms: u64,
}

#[derive(Debug)]
pub struct HealthReport {
	pub checks: Vec<CheckResult>,
	pub all_fatal_passed: bool,
	pub supported_codecs: u32,
	pub hdr_supported: bool,
	pub dma_buf_supported: bool,
	pub gpu_name: String,
	pub duration: Duration,
}

/// GPU capability results produced by [`probe_capabilities`].
///
/// Unlike [`run_healthcheck`], this only runs the GPU/codec/HDR/DMA-BUF probes
/// and does not gate startup on the full set of checks, so it can be invoked
/// even when the full health check is skipped. DMA-BUF support is still
/// captured because the video pipeline cannot encode without it.
pub struct Capabilities {
	pub supported_codecs: u32,
	pub hdr_supported: bool,
	pub dma_buf_supported: bool,
	pub gpu_name: String,
}

// Codec mode support bitmask values — single source of truth, also used by the
// webserver to build the advertised `ServerCodecModeSupport` bitmask from the
// probed `report.supported_codecs`.
pub const CODEC_H264: u32 = 0x00000001;
pub const CODEC_HEVC: u32 = 0x00000100;
pub const CODEC_HEVC_MAIN10: u32 = 0x00000200;
pub const CODEC_AV1_MAIN8: u32 = 0x00010000;
pub const CODEC_AV1_MAIN10: u32 = 0x00020000;
pub const CODEC_H264_HIGH_8444: u32 = 0x00040000;
pub const CODEC_HEVC_REXT_8444: u32 = 0x00080000;
pub const CODEC_HEVC_REXT_10444: u32 = 0x00100000;
pub const CODEC_AV1_HIGH_8444: u32 = 0x00200000;
pub const CODEC_AV1_HIGH_10444: u32 = 0x00400000;

impl HealthReport {
	fn add(&mut self, name: &'static str, outcome: CheckOutcome, message: String, duration_ms: u64) {
		if outcome == CheckOutcome::Failed {
			self.all_fatal_passed = false;
		}
		self.checks.push(CheckResult {
			name,
			outcome,
			message,
			duration_ms,
		});
	}

	fn add_passed(&mut self, name: &'static str, message: String, duration_ms: u64) {
		self.add(name, CheckOutcome::Passed, message, duration_ms);
	}

	fn add_failed(&mut self, name: &'static str, message: String, duration_ms: u64) {
		self.add(name, CheckOutcome::Failed, message, duration_ms);
	}

	fn add_warn(&mut self, name: &'static str, message: String, duration_ms: u64) {
		self.add(name, CheckOutcome::Warning, message, duration_ms);
	}
}

/// Return `(all_fatal_passed, fatal_count, warning_count)`.
pub fn health_summary(report: &HealthReport) -> (bool, usize, usize) {
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

pub fn log_health_report(report: &HealthReport) {
	iter_checks(report, |outcome, name, msg| match outcome {
		CheckOutcome::Passed => {
			tracing::debug!("{:>15}  OK   {msg}", name);
		},
		CheckOutcome::Failed => {
			tracing::error!("{:>15}  FAIL\n{msg}", name);
		},
		CheckOutcome::Warning => {
			tracing::warn!("{:>15}  WARN\n{msg}", name);
		},
	});

	let (passed, fatal, warn) = health_summary(report);
	if passed {
		if warn > 0 {
			tracing::debug!(
				"Health checks passed in {}ms ({} warnings).",
				report.duration.as_millis(),
				warn
			);
		} else {
			tracing::debug!("Health checks passed in {}ms.", report.duration.as_millis());
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

/// Invoke `f` for every check with its outcome, name and message.
fn iter_checks(report: &HealthReport, mut f: impl FnMut(CheckOutcome, &str, &str)) {
	for check in &report.checks {
		f(check.outcome, check.name, &check.message);
	}
}

/// Run the health check and log the results to the console.
pub fn print_health_report(report: &HealthReport) {
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

/// Probe GPU capabilities and HDR support without the full health check.
///
/// Runs the render-node, EGL, Vulkan, codec and DMA-BUF probes and records
/// their results. This is the value-producing part of [`run_healthcheck`] and
/// is invoked unconditionally so the server always advertises real capabilities.
pub fn probe_capabilities(config: Option<&Config>) -> Capabilities {
	let gpu_config = config.map(|c| c.compositor.gpu.clone()).unwrap_or(None);

	let mut report = HealthReport {
		checks: Vec::new(),
		all_fatal_passed: true,
		supported_codecs: 0,
		hdr_supported: false,
		dma_buf_supported: false,
		gpu_name: String::new(),
		duration: Duration::ZERO,
	};

	run_gpu_checks(&mut report, &gpu_config);

	Capabilities {
		supported_codecs: report.supported_codecs,
		hdr_supported: report.hdr_supported,
		dma_buf_supported: report.dma_buf_supported,
		gpu_name: report.gpu_name,
	}
}

/// Run the GPU/codec/HDR probes and record their results into `report`.
fn run_gpu_checks(report: &mut HealthReport, gpu_config: &Option<String>) {
	let render_node = check_render_nodes(report, gpu_config);
	let render_node_open = check_render_node_open(report, gpu_config, render_node.as_ref());
	// Prefer the configured/accessible node over the first discovered one, so
	// the EGL probe (HDR detection) runs on the GPU the compositor will use.
	let egl_result = check_egl(report, render_node_open.as_ref().or(render_node.as_ref()));
	let vk_context = check_vulkan(report);
	check_codecs(report, vk_context.as_ref());
	report.dma_buf_supported = check_dmabuf(report, vk_context.as_ref());
	check_wsi_layer(report);

	if let Some(ref ctx) = vk_context {
		let props = ctx.device_properties();
		let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
			.to_string_lossy()
			.to_string();
		report.gpu_name = name;
	} else if let Some((ref name, _, _)) = egl_result {
		report.gpu_name = name.clone();
	}

	if let Some((_, hdr, _)) = &egl_result {
		report.hdr_supported = *hdr;
	}
}

pub fn run_healthcheck(config: Option<&Config>) -> HealthReport {
	let started = Instant::now();

	let gpu_config = config.map(|c| c.compositor.gpu.clone()).unwrap_or(None);

	let mut report = HealthReport {
		checks: Vec::new(),
		all_fatal_passed: true,
		supported_codecs: 0,
		hdr_supported: false,
		dma_buf_supported: false,
		gpu_name: String::new(),
		duration: Duration::ZERO,
	};

	// --- GPU checks ---

	run_gpu_checks(&mut report, &gpu_config);

	// --- External dependencies ---

	check_xwayland(&mut report);
	check_xdg_runtime(&mut report);
	check_inhibit(&mut report);
	check_kcmp(&mut report);

	// --- Ports (requires config) ---

	if let Some(config) = config {
		check_ports(&mut report, config);
	}

	check_uinput(&mut report);
	check_uhid(&mut report);

	if let Some(config) = config {
		check_dirs(&mut report, config);
	}

	report.duration = started.elapsed();
	report
}

// ---------------------------------------------------------------------------
// GPU helpers
// ---------------------------------------------------------------------------

fn group_name_of(path: &Path) -> String {
	let Ok(meta) = std::fs::metadata(path) else {
		return "unknown".into();
	};
	let gid = meta.st_gid();
	let grp = unsafe { libc::getgrgid(gid) };
	if grp.is_null() {
		format!("gid {gid}")
	} else {
		unsafe { CStr::from_ptr((*grp).gr_name) }.to_string_lossy().into_owned()
	}
}

fn check_render_nodes(report: &mut HealthReport, _gpu_config: &Option<String>) -> Option<PathBuf> {
	let start = Instant::now();
	let dri = Path::new("/dev/dri");
	if !dri.exists() {
		report.add_failed(
			"Render nodes",
			"  No /dev/dri directory found.\n  Ensure the GPU kernel DRM driver is loaded and /dev/dri exists.\n  Ubuntu: `sudo ubuntu-drivers autoinstall` (or install matching kernel/firmware)\n  Fedora: `sudo dnf install mesa-dri-drivers`\n  Arch: `sudo pacman -S mesa`".into(),
			start.elapsed().as_millis() as u64,
		);
		return None;
	}

	let mut entries: Vec<_> = match std::fs::read_dir(dri) {
		Ok(e) => e
			.filter_map(|e| e.ok())
			.filter(|e| {
				e.file_name()
					.to_str()
					.map(|n| n.starts_with("renderD"))
					.unwrap_or(false)
			})
			.collect(),
		Err(e) => {
			report.add_failed(
				"Render nodes",
				format!("  Cannot read /dev/dri: {e}\n  Check directory permissions."),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	entries.sort_by_key(|e| e.file_name());

	if entries.is_empty() {
		report.add_failed(
			"Render nodes",
			"  No render nodes found in /dev/dri.\n  Install GPU drivers with render node support.\n  Ubuntu: `sudo ubuntu-drivers autoinstall`\n  Fedora: `sudo dnf install mesa-dri-drivers`\n  Arch: `sudo pacman -S mesa`".into(),
			start.elapsed().as_millis() as u64,
		);
		return None;
	}

	let names: Vec<String> = entries
		.iter()
		.map(|e| e.file_name().to_string_lossy().into_owned())
		.collect();
	report.add_passed("Render nodes", names.join(", "), start.elapsed().as_millis() as u64);

	Some(entries[0].path())
}

fn check_render_node_open(
	report: &mut HealthReport,
	gpu_config: &Option<String>,
	fallback_node: Option<&PathBuf>,
) -> Option<PathBuf> {
	let start = Instant::now();
	let node = match find_render_node(gpu_config) {
		Ok(n) => n,
		Err(e) => {
			// When a GPU was explicitly configured but could not be resolved, this
			// is a real misconfiguration: the compositor will fail to start with it
			// even if the probe below passes on a fallback node. Record it as a
			// failure so startup gating matches runtime behavior.
			if gpu_config.is_some() {
				let cfg = gpu_config.as_deref().unwrap_or("<unknown>");
				match fallback_node {
					Some(fb) => {
						report.add_failed(
							"GPU config",
							format!(
								"  Configured GPU `{}` could not be resolved: {}\n  Falling back to {} for the remaining checks, but the compositor will fail to start with this configuration.",
								cfg,
								e,
								fb.display()
							),
							start.elapsed().as_millis() as u64,
						);
					},
					None => {
						report.add_failed(
							"GPU config",
							format!(
								"  Configured GPU `{}` could not be resolved: {}\n  No usable render node found.",
								cfg, e
							),
							start.elapsed().as_millis() as u64,
						);
						return None;
					},
				}
			}
			// No explicit config (or config invalid but a fallback exists): proceed
			// with the fallback node.
			fallback_node?.clone()
		},
	};

	match std::fs::OpenOptions::new().read(true).write(true).open(&node) {
		Ok(_fd) => {
			let group = group_name_of(&node);
			report.add_passed(
				"Render access",
				format!("{} (owned by group '{group}')", node.display()),
				start.elapsed().as_millis() as u64,
			);
			Some(node)
		},
		Err(e) => {
			let group = group_name_of(&node);
			report.add_failed(
				"Render access",
				format!(
					"  Cannot open {}: {e}\n  The node is owned by group '{group}'.\n  Add your user to this group: `sudo usermod -aG {group} $USER`\n  Then log out and back in, or use systemd service with SupplementaryGroups={group}.",
					node.display(),
				),
				start.elapsed().as_millis() as u64,
			);
			None
		},
	}
}

fn check_egl(report: &mut HealthReport, node: Option<&PathBuf>) -> Option<(String, bool, Vec<String>)> {
	let start = Instant::now();
	let node = node?;

	let render_fd = match std::fs::OpenOptions::new().read(true).write(true).open(node) {
		Ok(fd) => fd,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!("  Cannot open render node for EGL probe: {e}"),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let gbm_device = match smithay::backend::allocator::gbm::GbmDevice::new(render_fd) {
		Ok(d) => d,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!(
					"  Failed to create GBM device: {e}\n  libgbm is required for GPU rendering.\n  Ubuntu: `sudo apt install libgbm1`\n  Fedora: `sudo dnf install mesa-libgbm`\n  Arch: `sudo pacman -S mesa`"
				),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let egl_display = match unsafe { smithay::backend::egl::EGLDisplay::new(gbm_device) } {
		Ok(d) => d,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!(
					"  Failed to create EGL display: {e}\n  GPU drivers may be missing or incomplete.\n  Ubuntu: `sudo ubuntu-drivers autoinstall`\n  Fedora: `sudo dnf install mesa-libEGL`\n  Arch: `sudo pacman -S mesa`"
				),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let egl_context = match smithay::backend::egl::EGLContext::new(&egl_display) {
		Ok(c) => c,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!("  Failed to create EGL context: {e}\n  GPU does not support the required OpenGL ES features."),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let render_formats = egl_context.dmabuf_render_formats();
	let mut format_names: Vec<String> = render_formats.iter().map(|f| format!("{:?}", f.code)).collect();
	format_names.sort();
	format_names.dedup();
	let hdr_supported = render_formats.iter().any(|f| {
		matches!(
			f.code,
			smithay::backend::allocator::Fourcc::Abgr2101010 | smithay::backend::allocator::Fourcc::Abgr16161616f
		)
	});

	// Try to derive a GPU name from the render node's uevent DRIVER= field,
	// falling back to the render node path when unavailable.
	let gpu_name = node
		.file_name()
		.and_then(|n| n.to_str())
		.map(|name| Path::new("/sys/class/drm").join(name).join("device/uevent"))
		.and_then(|uevent| std::fs::read_to_string(&uevent).ok())
		.and_then(|uevent| {
			uevent
				.lines()
				.find_map(|line| line.strip_prefix("DRIVER=").map(|d| d.to_string()))
		})
		.unwrap_or_else(|| node.display().to_string());

	let msg = if hdr_supported {
		format!("{gpu_name} (HDR-capable, formats: {})", format_names.join(", "))
	} else {
		format!("{gpu_name} (SDR-only, formats: {})", format_names.join(", "))
	};

	report.add_passed("EGL/GLES", msg, start.elapsed().as_millis() as u64);
	Some((gpu_name, hdr_supported, format_names))
}

fn check_vulkan(report: &mut HealthReport) -> Option<VideoContext> {
	let start = Instant::now();
	match VideoContextBuilder::new().app_name("Moonshine Health Check").build() {
		Ok(ctx) => {
			let props = ctx.device_properties();
			let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
				.to_string_lossy()
				.to_string();
			let driver_version = props.driver_version;
			let api_version = props.api_version;
			let msg = format!(
				"{device_name}, driver {}.{}.{}, Vulkan {}.{}.{}",
				vk_version_major(driver_version),
				vk_version_minor(driver_version),
				vk_version_patch(driver_version),
				vk_version_major(api_version),
				vk_version_minor(api_version),
				vk_version_patch(api_version),
			);
			report.add_passed("Vulkan", msg, start.elapsed().as_millis() as u64);
			Some(ctx)
		},
		Err(e) => {
			let msg = format!(
				"  Failed to initialize Vulkan: {e}\n  Ensure Vulkan drivers with video encode support are installed.\n  Requires NVIDIA RTX 20xx+, AMD RDNA2+, or Intel Arc.\n  Ubuntu: `sudo ubuntu-drivers autoinstall`\n  Fedora: `sudo dnf install vulkan-loader mesa-vulkan-drivers`\n  Arch: `sudo pacman -S vulkan-icd-loader vulkan-mesa-layer`"
			);
			report.add_failed("Vulkan", msg, start.elapsed().as_millis() as u64);
			None
		},
	}
}

fn check_codecs(report: &mut HealthReport, context: Option<&VideoContext>) {
	let start = Instant::now();
	let ctx = match context {
		Some(c) => c,
		None => {
			report.add_failed(
				"Codecs",
				"  Skipped (Vulkan check failed).".into(),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let mut supported = 0u32;
	let mut names = Vec::new();

	if ctx.supports_encode(Codec::H264) {
		supported |= CODEC_H264;
		supported |= CODEC_H264_HIGH_8444;
		names.push("H.264");
	}
	if ctx.supports_encode(Codec::H265) {
		supported |= CODEC_HEVC;
		supported |= CODEC_HEVC_MAIN10;
		supported |= CODEC_HEVC_REXT_8444;
		supported |= CODEC_HEVC_REXT_10444;
		names.push("HEVC");
	}
	if ctx.supports_encode(Codec::AV1) {
		supported |= CODEC_AV1_MAIN8;
		supported |= CODEC_AV1_MAIN10;
		supported |= CODEC_AV1_HIGH_8444;
		supported |= CODEC_AV1_HIGH_10444;
		names.push("AV1");
	}

	report.supported_codecs = supported;

	if names.is_empty() {
		report.add_failed(
			"Codecs",
			"  No video encode codec supported (H.264, HEVC, AV1).\n  GPU driver may be outdated or missing Vulkan Video extensions.\n  Update to the latest GPU driver.".into(),
			start.elapsed().as_millis() as u64,
		);
	} else {
		report.add_passed("Codecs", names.join(", "), start.elapsed().as_millis() as u64);
	}
}

fn check_dmabuf(report: &mut HealthReport, context: Option<&VideoContext>) -> bool {
	let start = Instant::now();
	let ctx = match context {
		Some(c) => c,
		None => {
			report.add_failed(
				"DMA-BUF",
				"  Skipped (Vulkan check failed).".into(),
				start.elapsed().as_millis() as u64,
			);
			return false;
		},
	};

	let instance = ctx.instance();
	let physical_device = ctx.physical_device();

	let extensions = match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
		Ok(exts) => exts,
		Err(e) => {
			report.add_failed(
				"DMA-BUF",
				format!("  Failed to enumerate Vulkan device extensions: {e}"),
				start.elapsed().as_millis() as u64,
			);
			return false;
		},
	};

	let has_ext = |name: &CStr| -> bool {
		extensions.iter().any(|ext| {
			let ext_name = unsafe { CStr::from_ptr(ext.extension_name.as_ptr()) };
			ext_name == name
		})
	};

	let has_ext_mem_fd = has_ext(ash::khr::external_memory_fd::NAME);
	let has_drm_mod = has_ext(ash::ext::image_drm_format_modifier::NAME);

	if has_ext_mem_fd && has_drm_mod {
		report.add_passed(
			"DMA-BUF",
			"VK_KHR_external_memory_fd, VK_EXT_image_drm_format_modifier".into(),
			start.elapsed().as_millis() as u64,
		);
		true
	} else {
		let mut missing = Vec::new();
		if !has_ext_mem_fd {
			missing.push("VK_KHR_external_memory_fd");
		}
		if !has_drm_mod {
			missing.push("VK_EXT_image_drm_format_modifier");
		}
		report.add_failed(
			"DMA-BUF",
			format!(
				"  Missing extensions: {}\n  DMA-BUF import is required for zero-copy video encoding.\n  Update GPU drivers to the latest version.",
				missing.join(", ")
			),
			start.elapsed().as_millis() as u64,
		);
		false
	}
}

fn vk_version_major(v: u32) -> u32 {
	v >> 22
}

fn vk_version_minor(v: u32) -> u32 {
	(v >> 12) & 0x3ff
}

fn vk_version_patch(v: u32) -> u32 {
	v & 0xfff
}

// ---------------------------------------------------------------------------
// External dependencies
// ---------------------------------------------------------------------------

fn check_xwayland(report: &mut HealthReport) {
	let start = Instant::now();
	match which::which("Xwayland") {
		Ok(path) => {
			report.add_passed(
				"Xwayland",
				path.display().to_string(),
				start.elapsed().as_millis() as u64,
			);
		},
		Err(_) => {
			report.add_warn(
				"Xwayland",
				"  Xwayland not found in PATH.\n  Ubuntu: `sudo apt install xwayland`\n  Fedora: `sudo dnf install xorg-x11-server-Xwayland`\n  Arch: `sudo pacman -S xorg-x11-server`".into(),
				start.elapsed().as_millis() as u64,
			);
		},
	}
}

fn check_xdg_runtime(report: &mut HealthReport) {
	let start = Instant::now();
	match std::env::var("XDG_RUNTIME_DIR") {
		Ok(dir) => {
			let path = Path::new(&dir);
			if !path.is_dir() {
				report.add_failed(
					"Runtime dir",
					format!(
						"  XDG_RUNTIME_DIR ({dir}) is not a directory.\n  Ensure XDG_RUNTIME_DIR points to a valid runtime directory:\n  export XDG_RUNTIME_DIR=/run/user/$(id -u)"
					),
					start.elapsed().as_millis() as u64,
				);
			} else {
				// Quick writability test: try to create a temp file.
				let test = path.join(".moonshine-healthcheck-test");
				match std::fs::File::create(&test).and_then(|mut f| f.write_all(b"x")) {
					Ok(_) => {
						let _ = std::fs::remove_file(&test);
						report.add_passed("Runtime dir", dir, start.elapsed().as_millis() as u64);
					},
					Err(e) => {
						report.add_failed(
							"Runtime dir",
							format!(
								"  Cannot write to XDG_RUNTIME_DIR ({dir}): {e}\n  Check permissions on the runtime directory."
							),
							start.elapsed().as_millis() as u64,
						);
					},
				}
			}
		},
		Err(_) => {
			report.add_failed(
				"Runtime dir",
				"  XDG_RUNTIME_DIR is not set.\n  Set: export XDG_RUNTIME_DIR=/run/user/$(id -u)".into(),
				start.elapsed().as_millis() as u64,
			);
		},
	}
}

fn check_inhibit(report: &mut HealthReport) {
	let start = Instant::now();

	let conn = match zbus::blocking::Connection::system() {
		Ok(conn) => conn,
		Err(e) => {
			report.add_warn(
				"Sleep inhibit",
				format!("  Cannot connect to system D-Bus: {e}\n  systemd-logind may not be running."),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let msg = match conn.call_method(
		Some("org.freedesktop.login1"),
		"/org/freedesktop/login1",
		Some("org.freedesktop.login1.Manager"),
		"Inhibit",
		&("sleep", "Moonshine", "healthcheck", "block"),
	) {
		Ok(msg) => msg,
		Err(e) => {
			report.add_warn(
				"Sleep inhibit",
				format!("  Cannot acquire sleep inhibit: {e}\n  Add your user to the 'moonshine' group: `sudo usermod -aG moonshine $USER`\n  Then log out and back in."),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let raw = match msg.data().fds().first().map(|fd| fd.as_raw_fd()) {
		Some(fd) => fd,
		None => {
			report.add_warn(
				"Sleep inhibit",
				"  logind returned no file descriptor in inhibit response.".into(),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let duped = unsafe { libc::dup(raw) };
	if duped < 0 {
		report.add_warn(
			"Sleep inhibit",
			"  Failed to duplicate inhibit file descriptor.".into(),
			start.elapsed().as_millis() as u64,
		);
		return;
	}
	let _fd = unsafe { OwnedFd::from_raw_fd(duped) };

	report.add_passed(
		"Sleep inhibit",
		"Can acquire logind sleep inhibit (user in moonshine group, polkit rules ok)".into(),
		start.elapsed().as_millis() as u64,
	);
}

fn check_kcmp(report: &mut HealthReport) {
	let start = Instant::now();

	/// `KCMP_FILE` — not exposed by `libc` or glibc.
	const KCMP_FILE: libc::c_int = 0;

	// SAFETY: uses /dev/null fds; syscall with scalar arguments.
	let result = unsafe {
		let a = std::fs::File::open("/dev/null");
		let b = std::fs::File::open("/dev/null");
		match (a, b) {
			(Ok(a), Ok(b)) => libc::syscall(
				libc::SYS_kcmp,
				libc::getpid(),
				libc::getpid(),
				KCMP_FILE,
				a.as_raw_fd() as libc::c_ulong,
				b.as_raw_fd() as libc::c_ulong,
			),
			(Err(e), _) | (_, Err(e)) => {
				report.add_failed(
					"kcmp(2)",
					format!("  Cannot open /dev/null for kcmp(2) probe: {e}"),
					start.elapsed().as_millis() as u64,
				);
				return;
			},
		}
	};

	if result < 0 {
		let error = std::io::Error::last_os_error();
		let hint = match error.raw_os_error() {
			Some(libc::ENOSYS) => {
				"  Kernel was compiled without CONFIG_CHECKPOINT_RESTORE.\n  \
				kcmp(2) is required for DMA-BUF import cache validation.\n  \
				Rebuild your kernel with CONFIG_CHECKPOINT_RESTORE=y."
			},
			Some(libc::EPERM) => {
				"  kcmp(2) is blocked by a seccomp filter (e.g. Flatpak/container).\n  \
				kcmp(2) is required for DMA-BUF import cache validation.\n  \
				Run outside the sandbox or adjust the seccomp profile."
			},
			_ => "  kcmp(2) is required for DMA-BUF import cache validation.",
		};
		report.add_failed(
			"kcmp(2)",
			format!("  kcmp(2) not available: {error}\n{hint}"),
			start.elapsed().as_millis() as u64,
		);
		return;
	}

	report.add_passed("kcmp(2)", "Available".into(), start.elapsed().as_millis() as u64);
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

fn check_ports(report: &mut HealthReport, config: &Config) {
	let start = Instant::now();

	let ports: &[(u16, &str, bool)] = &[
		(config.webserver.port, "HTTP", true),
		(config.webserver.port_https, "HTTPS", true),
		(config.stream.port, "RTSP", true),
		(config.stream.video.port, "Video UDP", false),
		(config.stream.audio.port, "Audio UDP", false),
		(config.stream.control.port, "Control UDP", false),
	];

	let mut failed = Vec::new();
	let mut ok_list = Vec::new();

	for &(port, name, is_tcp) in ports {
		let result = if is_tcp {
			std::net::TcpListener::bind((config.address.as_str(), port)).map(|_| ())
		} else {
			std::net::UdpSocket::bind((config.address.as_str(), port)).map(|_| ())
		};

		match result {
			Ok(_) => ok_list.push(format!("{name}({port})")),
			Err(e) => failed.push(format!("{name} port {port}: {e}")),
		}
	}

	if failed.is_empty() {
		report.add_passed("Ports", ok_list.join(", "), start.elapsed().as_millis() as u64);
	} else {
		let mut msg = "  Port conflicts detected:\n".to_string();
		for f in &failed {
			msg.push_str(&format!("    {f}\n"));
		}
		msg.push_str(&format!("  Available ports: {}", ok_list.join(", ")));
		report.add_failed("Ports", msg, start.elapsed().as_millis() as u64);
	}
}

// ---------------------------------------------------------------------------
// Warning checks
// ---------------------------------------------------------------------------

fn check_uinput(report: &mut HealthReport) {
	let start = Instant::now();
	match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
		Ok(_fd) => {
			report.add_passed("uinput", "Writable".into(), start.elapsed().as_millis() as u64);
		},
		Err(e) => {
			report.add_warn(
				"uinput",
				format!(
					"  Cannot access /dev/uinput: {e}\n  Gamepad emulation will not work.\n  Load the module: `sudo modprobe uinput`\n  Persistent: `echo uinput | sudo tee /etc/modules-load.d/moonshine.conf`\n  Check udev rules are installed: /usr/lib/udev/rules.d/60-moonshine.rules"
				),
				start.elapsed().as_millis() as u64,
			);
		},
	}
}

fn check_uhid(report: &mut HealthReport) {
	let start = Instant::now();
	match std::fs::OpenOptions::new().write(true).open("/dev/uhid") {
		Ok(_fd) => {
			report.add_passed("uhid", "Writable".into(), start.elapsed().as_millis() as u64);
		},
		Err(e) => {
			report.add_warn(
				"uhid",
				format!(
					"  Cannot access /dev/uhid: {e}\n  Gamepad emulation may not work.\n  Load the module: `sudo modprobe uhid`\n  Persistent: `echo uhid | sudo tee /etc/modules-load.d/moonshine.conf`"
				),
				start.elapsed().as_millis() as u64,
			);
		},
	}
}

fn check_wsi_layer(report: &mut HealthReport) {
	let start = Instant::now();

	let entry = match unsafe { ash::Entry::load() } {
		Ok(e) => e,
		Err(_) => {
			report.add_failed(
				"WSI layer",
				"  Skipped (Vulkan check failed).".into(),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let layers = match unsafe { entry.enumerate_instance_layer_properties() } {
		Ok(l) => l,
		Err(_) => {
			report.add_warn(
				"WSI layer",
				"  Could not enumerate Vulkan instance layers.".into(),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let found = layers.iter().any(|l| {
		l.layer_name_as_c_str()
			.is_ok_and(|name| name.to_bytes() == b"VK_LAYER_MOONSHINE_wsi")
	});

	if found {
		report.add_passed("WSI layer", String::new(), start.elapsed().as_millis() as u64);
	} else {
		report.add_warn(
			"WSI layer",
			"  Not found in Vulkan instance layers.\n  Install the moonshine-wsi Vulkan layer.\n  Games will render through XWayland (higher latency, no direct scanout).".into(),
			start.elapsed().as_millis() as u64,
		);
	}
}

fn check_dirs(report: &mut HealthReport, config: &Config) {
	let start = Instant::now();

	let mut failed = Vec::new();

	// Pure read-only check: verify each required directory either exists and is
	// writable, or its parent is writable so the daemon can create it at startup.
	// We intentionally do NOT create directories here (a healthcheck should be
	// side-effect free and must not mask permission issues).
	let check_dir = |dir: &Path, label: &str, failed: &mut Vec<String>| {
		if dir.exists() {
			let test = dir.join(".moonshine-healthcheck-test");
			if let Err(e) = std::fs::File::create(&test).and_then(|mut f| f.write_all(b"x")) {
				failed.push(format!("{label} {}: {e}", dir.display()));
			} else {
				let _ = std::fs::remove_file(&test);
			}
		} else if let Some(parent) = dir.parent() {
			let test = parent.join(".moonshine-healthcheck-test");
			if let Err(e) = std::fs::File::create(&test).and_then(|mut f| f.write_all(b"x")) {
				failed.push(format!(
					"{label} {} does not exist and cannot be created (parent not writable): {e}",
					dir.display()
				));
			} else {
				let _ = std::fs::remove_file(&test);
			}
		} else {
			failed.push(format!("{label} {} has no parent directory", dir.display()));
		}
	};

	// Check state directory
	let state_dir = dirs::data_dir()
		.map(|d| d.join("moonshine"))
		.unwrap_or_else(|| PathBuf::from("/var/lib/moonshine"));
	check_dir(&state_dir, "State dir", &mut failed);

	// Check cert/key directories
	let cert_dir = config.webserver.certificate.parent();
	let key_dir = config.webserver.private_key.parent();
	for dir in [cert_dir, key_dir].iter().flatten() {
		check_dir(dir, "Config dir", &mut failed);
	}

	if failed.is_empty() {
		report.add_passed(
			"Config dirs",
			format!(
				"{}, {}",
				state_dir.display(),
				cert_dir.map(|d| d.display().to_string()).unwrap_or_default()
			),
			start.elapsed().as_millis() as u64,
		);
	} else {
		report.add_warn(
			"Config dirs",
			format!(
				"  Directory issues:\n    {}\n  Check permissions.",
				failed.join("\n    ")
			),
			start.elapsed().as_millis() as u64,
		);
	}
}

// ---------------------------------------------------------------------------
// Shared utility: find the appropriate DRM render node
// ---------------------------------------------------------------------------

/// Find the appropriate DRM render node, respecting GPU config and env override.
pub(crate) fn find_render_node(gpu_config: &Option<String>) -> Result<PathBuf, String> {
	if let Ok(node) = std::env::var("MOONSHINE_RENDER_NODE") {
		return Ok(PathBuf::from(node));
	}

	let dri_path = Path::new("/dev/dri");
	if !dri_path.exists() {
		return Err("No /dev/dri directory found".to_string());
	}

	let mut entries: Vec<_> = std::fs::read_dir(dri_path)
		.map_err(|e| format!("Failed to read /dev/dri: {e}"))?
		.filter_map(|entry| entry.ok())
		.filter(|entry| {
			entry
				.file_name()
				.to_str()
				.map(|name| name.starts_with("renderD"))
				.unwrap_or(false)
		})
		.collect();

	entries.sort_by_key(|e| e.file_name());

	if entries.is_empty() {
		return Err("No render node found in /dev/dri".to_string());
	}

	let get_device_info = |entry: &std::fs::DirEntry| -> Option<(String, String)> {
		let file_name = entry.file_name();
		let name = file_name.to_str()?;
		let device_path = Path::new("/sys/class/drm").join(name).join("device/uevent");
		let content = std::fs::read_to_string(device_path).ok()?;
		Some((name.to_string(), content))
	};

	if let Some(config_str) = gpu_config {
		let path = Path::new(config_str);
		if path.is_absolute() {
			if path.exists() {
				return Ok(path.to_path_buf());
			}
			return Err(format!("Configured GPU path does not exist: {config_str}"));
		}

		for entry in &entries {
			if let Some((name, uevent)) = get_device_info(entry) {
				if name == *config_str {
					return Ok(entry.path());
				}
				if uevent.to_lowercase().contains(&config_str.to_lowercase()) {
					return Ok(entry.path());
				}
			}
		}

		return Err(format!("No GPU found matching configuration: {config_str}"));
	}

	let mut best_node = None;
	let mut best_score: i32 = -1;

	for entry in &entries {
		let score = if let Some((_, uevent)) = get_device_info(entry) {
			if uevent.contains("PCI_ID=10DE") {
				100
			} else if uevent.contains("PCI_ID=1002") {
				50
			} else {
				0
			}
		} else {
			0
		};

		if score > best_score {
			best_score = score;
			best_node = Some(entry.path());
		}
	}

	Ok(best_node.unwrap_or_else(|| entries[0].path()))
}
