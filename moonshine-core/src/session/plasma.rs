//! Managed, isolated KDE Plasma 6 session for headless streaming.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Clone, Debug)]
pub struct PlasmaSessionOptions {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub scale: f32,
	pub inner: bool,
}

/// Run nested KWin and Plasma on Pyroshine's Wayland output.
///
/// The outer invocation creates a private XDG runtime/config overlay and D-Bus
/// session, then re-executes Pyroshine in `inner` mode. The overlay snapshots the
/// user's top-level Plasma configuration and links its application profile
/// directories, while directing Plasma session writes and the classic-startup
/// override to private storage. The inner invocation starts KWin first.
/// `startplasma-wayland` performs KDE's supported environment initialization,
/// then its classic `plasma_session` path sees
/// `org.kde.KWinWrapper` already registered and uses that compositor instead of
/// launching another.
pub async fn run(options: PlasmaSessionOptions) -> Result<(), String> {
	if options.inner {
		return run_inner(options).await;
	}
	validate_options(&options)?;
	for executable in [
		"dbus-run-session",
		"kwin_wayland_wrapper",
		"startplasma-wayland",
		"plasma_session",
		"Xwayland",
	] {
		which::which(executable).map_err(|_| format!("Plasma desktop requires `{executable}` in PATH"))?;
	}

	let outer_runtime = std::env::var_os("XDG_RUNTIME_DIR")
		.map(PathBuf::from)
		.ok_or_else(|| "XDG_RUNTIME_DIR is not set for the Pyroshine session".to_string())?;
	let outer_display = std::env::var_os("WAYLAND_DISPLAY")
		.ok_or_else(|| "WAYLAND_DISPLAY is not set for the Pyroshine compositor".to_string())?;
	let outer_socket = if Path::new(&outer_display).is_absolute() {
		PathBuf::from(outer_display)
	} else {
		outer_runtime.join(outer_display)
	};
	if !outer_socket.exists() {
		return Err(format!(
			"Pyroshine Wayland socket does not exist: {}",
			outer_socket.display()
		));
	}

	let isolation = tempfile::Builder::new()
		.prefix("moonshine-plasma-")
		.tempdir_in(&outer_runtime)
		.map_err(|e| format!("creating isolated Plasma runtime: {e}"))?;
	std::fs::set_permissions(isolation.path(), std::fs::Permissions::from_mode(0o700))
		.map_err(|e| format!("securing isolated Plasma runtime: {e}"))?;
	let config = isolation.path().join("config");
	let cache = isolation.path().join("cache");
	for directory in [&config, &cache] {
		std::fs::create_dir(directory).map_err(|e| format!("creating {}: {e}", directory.display()))?;
	}
	let profile_config = profile_config_directory(
		std::env::var_os("XDG_CONFIG_HOME").as_deref(),
		std::env::var_os("HOME").as_deref(),
	)?;
	populate_config_overlay(&profile_config, &config)?;
	// The systemd boot mode would use the host user manager. Classic mode is
	// Plasma's supported process startup path and stays on our private bus. This
	// file intentionally replaces any value copied from the user's profile.
	std::fs::write(config.join("startkderc"), b"[General]\nsystemdBoot=false\n")
		.map_err(|e| format!("writing isolated Plasma startup config: {e}"))?;

	let executable = std::env::current_exe().map_err(|e| format!("locating Pyroshine executable: {e}"))?;
	tracing::info!(
		width = options.width,
		height = options.height,
		refresh_rate = options.refresh_rate,
		scale = options.scale,
		outer_wayland = %outer_socket.display(),
		profile_config = %profile_config.display(),
		"Starting isolated nested Plasma 6 session with user profile"
	);
	let mut command = Command::new("dbus-run-session");
	command
		.arg("--")
		.arg(executable)
		.arg("plasma-session")
		.arg("--inner")
		.arg("--width")
		.arg(options.width.to_string())
		.arg("--height")
		.arg(options.height.to_string())
		.arg("--refresh-rate")
		.arg(options.refresh_rate.to_string())
		.arg("--scale")
		.arg(options.scale.to_string())
		.env("XDG_RUNTIME_DIR", isolation.path())
		.env("XDG_CONFIG_HOME", &config)
		.env("XDG_CACHE_HOME", &cache)
		.env("WAYLAND_DISPLAY", &outer_socket)
		.env_remove("DISPLAY")
		.stdin(Stdio::null())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		.kill_on_drop(true);
	let child = command
		.spawn()
		.map_err(|e| format!("starting private Plasma D-Bus session: {e}"))?;
	let (status, shutdown_requested) = wait_for_child(child, "private Plasma D-Bus session").await?;
	if shutdown_requested {
		return Ok(());
	}
	if status.success() {
		Ok(())
	} else {
		Err(format!("Plasma session exited with {status}"))
	}
}

fn profile_config_directory(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf, String> {
	if let Some(value) = xdg_config_home.filter(|value| !value.is_empty()) {
		let path = PathBuf::from(value);
		if path.is_absolute() {
			return Ok(path);
		}
		tracing::warn!(
			xdg_config_home = %path.display(),
			"Ignoring relative XDG_CONFIG_HOME while preparing Plasma profile"
		);
	}
	let home = home
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.ok_or_else(|| "HOME is not set; cannot locate the user's Plasma configuration".to_string())?;
	if !home.is_absolute() {
		return Err("HOME must be an absolute path to locate the user's Plasma configuration".to_string());
	}
	Ok(home.join(".config"))
}

fn populate_config_overlay(profile: &Path, overlay: &Path) -> Result<(), String> {
	if !profile.exists() {
		return Ok(());
	}
	let entries = std::fs::read_dir(profile)
		.map_err(|e| format!("reading Plasma profile directory {}: {e}", profile.display()))?;
	for entry in entries {
		let entry = entry.map_err(|e| format!("reading an entry in {}: {e}", profile.display()))?;
		if entry.file_name() == OsStr::new("startkderc") {
			continue;
		}
		let source = entry.path();
		let destination = overlay.join(entry.file_name());
		let file_type = entry
			.file_type()
			.map_err(|e| format!("inspecting Plasma profile entry {}: {e}", source.display()))?;
		if file_type.is_file() {
			std::fs::copy(&source, &destination)
				.map_err(|e| format!("copying Plasma profile file {}: {e}", source.display()))?;
		} else if file_type.is_dir() || file_type.is_symlink() {
			// Profile directories can be very large (browser profiles commonly live
			// here), so link rather than recursively copying them for every stream.
			symlink(&source, &destination)
				.map_err(|e| format!("linking Plasma profile entry {}: {e}", source.display()))?;
		} else {
			tracing::debug!(path = %source.display(), "Skipping special file in Plasma profile");
		}
	}
	Ok(())
}

async fn run_inner(options: PlasmaSessionOptions) -> Result<(), String> {
	validate_options(&options)?;
	if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
		return Err("isolated Plasma process has no private D-Bus session".to_string());
	}
	let outer_socket = std::env::var_os("WAYLAND_DISPLAY")
		.ok_or_else(|| "isolated Plasma process has no outer WAYLAND_DISPLAY".to_string())?;
	let socket_name = format!("moonshine-plasma-{}", std::process::id());

	// KWin is paced by frame callbacks from Pyroshine's client-mode output;
	// that outer output already carries the client-selected refresh rate.
	tracing::info!(
		width = options.width,
		height = options.height,
		refresh_rate = options.refresh_rate,
		scale = options.scale,
		dbus = "private",
		"Launching KWin and Plasma workspace"
	);
	let mut command = Command::new("kwin_wayland_wrapper");
	command
		.arg("--xwayland")
		.arg("--socket")
		.arg(&socket_name)
		.arg("--wayland-display")
		.arg(outer_socket)
		.arg("--width")
		.arg(options.width.to_string())
		.arg("--height")
		.arg(options.height.to_string())
		.arg("--scale")
		.arg(options.scale.to_string())
		.arg("--fullscreen")
		.arg("true")
		.arg("--no-lockscreen")
		.arg("--exit-with-session")
		.arg("startplasma-wayland")
		.env("XDG_SESSION_TYPE", "wayland")
		.env("XDG_SESSION_DESKTOP", "KDE")
		.env("XDG_CURRENT_DESKTOP", "KDE")
		.env("KDE_FULL_SESSION", "true")
		.env("KDE_SESSION_VERSION", "6")
		.stdin(Stdio::null())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		.kill_on_drop(true);
	let child = command.spawn().map_err(|e| format!("starting nested KWin: {e}"))?;
	let (status, shutdown_requested) = wait_for_child(child, "nested KWin/Plasma").await?;
	if shutdown_requested {
		return Ok(());
	}
	if status.success() {
		Ok(())
	} else {
		Err(format!("nested KWin/Plasma exited with {status}"))
	}
}

/// Wait for a session process while owning SIGINT/SIGTERM. On service stop,
/// forward SIGTERM and allow KDE/D-Bus to clean up before escalating. Keeping
/// the outer process alive until this completes also lets `TempDir` remove the
/// private runtime tree deterministically.
async fn wait_for_child(mut child: Child, label: &str) -> Result<(ExitStatus, bool), String> {
	let mut terminate = signal(SignalKind::terminate()).map_err(|e| format!("installing SIGTERM handler: {e}"))?;
	let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| format!("installing SIGINT handler: {e}"))?;
	tokio::select! {
		status = child.wait() => status
			.map(|status| (status, false))
			.map_err(|e| format!("waiting for {label}: {e}")),
		_ = terminate.recv() => terminate_child(child, label).await,
		_ = interrupt.recv() => terminate_child(child, label).await,
	}
}

async fn terminate_child(mut child: Child, label: &str) -> Result<(ExitStatus, bool), String> {
	tracing::info!(process = label, "Stopping managed Plasma process");
	if let Some(pid) = child.id() {
		// SAFETY: `pid` belongs to the live child owned above. SIGTERM does not
		// transfer ownership and failure is handled by the subsequent wait.
		unsafe {
			libc::kill(pid as libc::pid_t, libc::SIGTERM);
		}
	}
	match tokio::time::timeout(std::time::Duration::from_secs(15), child.wait()).await {
		Ok(status) => status
			.map(|status| (status, true))
			.map_err(|e| format!("waiting for {label} shutdown: {e}")),
		Err(_) => {
			tracing::warn!(process = label, "Plasma process did not stop after SIGTERM; killing it");
			child.start_kill().map_err(|e| format!("killing {label}: {e}"))?;
			let status = child.wait().await.map_err(|e| format!("reaping {label}: {e}"))?;
			Ok((status, true))
		},
	}
}

fn validate_options(options: &PlasmaSessionOptions) -> Result<(), String> {
	if options.width == 0 || options.height == 0 || options.refresh_rate == 0 {
		return Err("Plasma width, height, and refresh rate must be non-zero".to_string());
	}
	if !options.scale.is_finite() || !(0.5..=4.0).contains(&options.scale) {
		return Err("Plasma scale must be finite and between 0.5 and 4.0".to_string());
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn validates_client_mode() {
		let valid = PlasmaSessionOptions {
			width: 3840,
			height: 2160,
			refresh_rate: 120,
			scale: 2.0,
			inner: false,
		};
		assert!(validate_options(&valid).is_ok());
		assert!(validate_options(&PlasmaSessionOptions { scale: 0.0, ..valid }).is_err());
	}

	#[test]
	fn resolves_profile_config_from_xdg_or_home() {
		assert_eq!(
			profile_config_directory(Some(OsStr::new("/custom/config")), Some(OsStr::new("/home/test"))).unwrap(),
			PathBuf::from("/custom/config")
		);
		assert_eq!(
			profile_config_directory(Some(OsStr::new("relative")), Some(OsStr::new("/home/test"))).unwrap(),
			PathBuf::from("/home/test/.config")
		);
		assert!(profile_config_directory(None, None).is_err());
		assert!(profile_config_directory(None, Some(OsStr::new("relative"))).is_err());
	}

	#[test]
	fn materializes_profile_without_copying_large_directories() {
		let profile = tempfile::tempdir().unwrap();
		let overlay = tempfile::tempdir().unwrap();
		std::fs::write(
			profile.path().join("kdeglobals"),
			b"[General]\nColorScheme=BreezeDark\n",
		)
		.unwrap();
		std::fs::write(profile.path().join("startkderc"), b"[General]\nsystemdBoot=force\n").unwrap();
		std::fs::create_dir(profile.path().join("plasma-workspace")).unwrap();
		std::fs::write(profile.path().join("plasma-workspace/env.sh"), b"export TEST=1\n").unwrap();

		populate_config_overlay(profile.path(), overlay.path()).unwrap();

		assert_eq!(
			std::fs::read_to_string(overlay.path().join("kdeglobals")).unwrap(),
			"[General]\nColorScheme=BreezeDark\n"
		);
		assert!(!overlay.path().join("startkderc").exists());
		assert!(
			std::fs::symlink_metadata(overlay.path().join("plasma-workspace"))
				.unwrap()
				.file_type()
				.is_symlink()
		);
		assert_eq!(
			std::fs::read_to_string(overlay.path().join("plasma-workspace/env.sh")).unwrap(),
			"export TEST=1\n"
		);

		std::fs::write(overlay.path().join("kdeglobals"), b"changed").unwrap();
		assert_ne!(std::fs::read(profile.path().join("kdeglobals")).unwrap(), b"changed");
	}
}
