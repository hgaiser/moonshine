use std::sync::{Arc, PoisonError, RwLock};

use super::{ApplicationScanner, ApplicationScannerConfig, load_applications};
use crate::session::application::ApplicationConfig;

/// Keeps the list of configured and scanned applications up to date.
///
/// The list is rebuilt by [`refresh`](Self::refresh) when a scanner's source data changed, and
/// served from memory otherwise. Cloning is cheap and all clones share the same state.
#[derive(Clone)]
pub struct ApplicationManager {
	inner: Arc<ApplicationManagerInner>,
}

struct ApplicationManagerInner {
	/// Applications from the configuration file, which scanning never changes.
	configured_applications: Vec<ApplicationConfig>,

	/// Scanners that add dynamically discovered applications.
	///
	/// The async lock is held for the whole of a refresh, so concurrent refreshes queue up and
	/// only the first one scans.
	scanners: tokio::sync::Mutex<Vec<ApplicationScanner>>,

	/// Configured and scanned applications as of the most recent scan.
	applications: RwLock<Vec<ApplicationConfig>>,
}

impl ApplicationManager {
	/// Create a manager and run the first scan.
	///
	/// Blocks while scanning, so call it before serving requests.
	pub fn new(configured_applications: Vec<ApplicationConfig>, scanners: Vec<ApplicationScannerConfig>) -> Self {
		let mut scanners: Vec<ApplicationScanner> = scanners.into_iter().map(ApplicationScanner::new).collect();
		let applications = load_applications(&configured_applications, &mut scanners);

		Self {
			inner: Arc::new(ApplicationManagerInner {
				configured_applications,
				scanners: tokio::sync::Mutex::new(scanners),
				applications: RwLock::new(applications),
			}),
		}
	}

	/// Rescan if any scanner's source data changed since its last scan.
	///
	/// Scanning runs on a blocking thread, since it walks directories and resolves icons. When it
	/// fails to run, the previous list stays in place.
	pub async fn refresh(&self) {
		let mut scanners = self.inner.scanners.lock().await;
		if !scanners.iter().any(ApplicationScanner::needs_reload) {
			return;
		}

		let configured_applications = self.inner.configured_applications.clone();
		let mut scanned = scanners.clone();
		match tokio::task::spawn_blocking(move || {
			let applications = load_applications(&configured_applications, &mut scanned);
			(applications, scanned)
		})
		.await
		{
			Ok((applications, scanned)) => {
				*scanners = scanned;
				*self.inner.applications.write().unwrap_or_else(PoisonError::into_inner) = applications;
			},
			Err(e) => tracing::warn!("Failed to rescan applications: {e}"),
		}
	}

	/// The applications as of the most recent scan.
	pub fn applications(&self) -> Vec<ApplicationConfig> {
		self.inner
			.applications
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}

	/// Find an application by ID in the most recent scan.
	pub fn find(&self, application_id: i32) -> Option<ApplicationConfig> {
		self.inner
			.applications
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
			.find(|application| application.id() == application_id)
			.cloned()
	}
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::{Path, PathBuf};
	use std::thread::sleep;
	use std::time::Duration;

	use tempfile::tempdir;

	use super::*;
	use crate::app_scanner::desktop::DesktopApplicationScannerConfig;

	fn desktop_scanner(directory: PathBuf) -> ApplicationScannerConfig {
		ApplicationScannerConfig::Desktop(DesktopApplicationScannerConfig {
			directories: vec![directory],
			include_terminal: false,
			resolve_icons: false,
			pre_command: Vec::new(),
			post_command: Vec::new(),
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		})
	}

	fn write_desktop_entry(directory: &Path, filename: &str, name: &str) {
		fs::write(
			directory.join(filename),
			format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n"),
		)
		.unwrap();
	}

	fn titles(manager: &ApplicationManager) -> Vec<String> {
		let mut titles: Vec<String> = manager.applications().into_iter().map(|app| app.title).collect();
		titles.sort();
		titles
	}

	#[tokio::test]
	async fn refresh_picks_up_added_and_deleted_applications() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "a.desktop", "Alpha");

		let manager = ApplicationManager::new(Vec::new(), vec![desktop_scanner(tempdir.path().to_path_buf())]);
		assert_eq!(titles(&manager), ["Alpha"]);

		sleep(Duration::from_millis(10));
		write_desktop_entry(tempdir.path(), "b.desktop", "Beta");
		manager.refresh().await;
		assert_eq!(titles(&manager), ["Alpha", "Beta"]);

		// Deleting the older entry leaves the newest timestamp untouched.
		fs::remove_file(tempdir.path().join("a.desktop")).unwrap();
		manager.refresh().await;
		assert_eq!(titles(&manager), ["Beta"]);
	}

	#[tokio::test]
	async fn find_looks_applications_up_by_id() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "a.desktop", "Alpha");

		let manager = ApplicationManager::new(Vec::new(), vec![desktop_scanner(tempdir.path().to_path_buf())]);
		let alpha = manager.applications().remove(0);

		assert_eq!(manager.find(alpha.id()).map(|app| app.title), Some("Alpha".to_string()));
		assert!(manager.find(alpha.id().wrapping_add(1)).is_none());
	}

	#[tokio::test]
	async fn configured_applications_survive_a_refresh() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "a.desktop", "Alpha");

		let configured = ApplicationConfig {
			title: "Manual".to_string(),
			..Default::default()
		};
		let manager = ApplicationManager::new(vec![configured], vec![desktop_scanner(tempdir.path().to_path_buf())]);

		sleep(Duration::from_millis(10));
		write_desktop_entry(tempdir.path(), "b.desktop", "Beta");
		manager.refresh().await;

		assert_eq!(titles(&manager), ["Alpha", "Beta", "Manual"]);
	}
}
