use std::collections::HashSet;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::SystemTime;

use super::{ApplicationScanner, ApplicationScannerConfig, CatalogEntry, load_catalog};
use crate::session::application::ApplicationConfig;

/// Keeps the list of configured and scanned applications up to date.
///
/// The list is rebuilt by [`refresh`](Self::refresh) when a scanner's source data changed, and
/// served from memory otherwise. Cloning is cheap and all clones share the same state.
#[derive(Clone)]
pub struct ApplicationManager {
	inner: Arc<ApplicationManagerInner>,
}

/// What the catalog is built from.
#[derive(Clone)]
struct Sources {
	/// Applications from the configuration file, which scanning never changes.
	configured_applications: Vec<ApplicationConfig>,

	/// Titles of scanned applications that clients don't see.
	hidden: HashSet<String>,
}

struct ApplicationManagerInner {
	sources: RwLock<Sources>,

	/// Scanners that add dynamically discovered applications.
	///
	/// The async lock is held for the whole of a refresh, so concurrent refreshes queue up and
	/// only the first one scans.
	scanners: tokio::sync::Mutex<Vec<ApplicationScanner>>,

	/// Configured and scanned applications as of the most recent scan.
	catalog: RwLock<Vec<CatalogEntry>>,

	/// When the most recent scan finished.
	scanned_at: RwLock<Option<SystemTime>>,
}

impl ApplicationManager {
	/// Create a manager and run the first scan.
	///
	/// Blocks while scanning, so call it before serving requests.
	pub fn new(configured_applications: Vec<ApplicationConfig>, scanners: Vec<ApplicationScannerConfig>) -> Self {
		Self::with_hidden(configured_applications, scanners, Vec::new())
	}

	/// Create a manager whose scanned applications in `hidden` (by title) are hidden from clients,
	/// and run the first scan.
	///
	/// Blocks while scanning, so call it before serving requests.
	pub fn with_hidden(
		configured_applications: Vec<ApplicationConfig>,
		scanners: Vec<ApplicationScannerConfig>,
		hidden: Vec<String>,
	) -> Self {
		let sources = Sources {
			configured_applications,
			hidden: hidden.into_iter().collect(),
		};
		let mut scanners: Vec<ApplicationScanner> = scanners.into_iter().map(ApplicationScanner::new).collect();
		let catalog = load_catalog(&sources.configured_applications, &mut scanners, &sources.hidden);

		Self {
			inner: Arc::new(ApplicationManagerInner {
				sources: RwLock::new(sources),
				scanners: tokio::sync::Mutex::new(scanners),
				catalog: RwLock::new(catalog),
				scanned_at: RwLock::new(Some(SystemTime::now())),
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
		self.rescan(&mut scanners).await;
	}

	/// Replace the configured applications, the scanners and the hidden titles, then scan again.
	pub async fn reload(
		&self,
		configured_applications: Vec<ApplicationConfig>,
		scanners: Vec<ApplicationScannerConfig>,
		hidden: Vec<String>,
	) {
		let mut current = self.inner.scanners.lock().await;
		*current = scanners.into_iter().map(ApplicationScanner::new).collect();
		*self.inner.sources.write().unwrap_or_else(PoisonError::into_inner) = Sources {
			configured_applications,
			hidden: hidden.into_iter().collect(),
		};
		self.rescan(&mut current).await;
	}

	/// Replace the titles of the hidden applications, without scanning again.
	pub fn set_hidden(&self, hidden: Vec<String>) {
		let hidden: HashSet<String> = hidden.into_iter().collect();
		for entry in self
			.inner
			.catalog
			.write()
			.unwrap_or_else(PoisonError::into_inner)
			.iter_mut()
		{
			entry.hidden = entry.scanner.is_some() && hidden.contains(&entry.application.title);
		}
		self.inner
			.sources
			.write()
			.unwrap_or_else(PoisonError::into_inner)
			.hidden = hidden;
	}

	async fn rescan(&self, scanners: &mut Vec<ApplicationScanner>) {
		let sources = self
			.inner
			.sources
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.clone();
		let mut scanned = scanners.clone();
		match tokio::task::spawn_blocking(move || {
			let catalog = load_catalog(&sources.configured_applications, &mut scanned, &sources.hidden);
			(catalog, scanned)
		})
		.await
		{
			Ok((catalog, scanned)) => {
				*scanners = scanned;
				*self.inner.catalog.write().unwrap_or_else(PoisonError::into_inner) = catalog;
				*self.inner.scanned_at.write().unwrap_or_else(PoisonError::into_inner) = Some(SystemTime::now());
			},
			Err(e) => tracing::warn!("Failed to rescan applications: {e}"),
		}
	}

	/// The applications clients see, as of the most recent scan.
	pub fn applications(&self) -> Vec<ApplicationConfig> {
		self.inner
			.catalog
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
			.filter(|entry| !entry.hidden)
			.map(|entry| entry.application.clone())
			.collect()
	}

	/// Every configured and scanned application, hidden ones included, as of the most recent scan.
	pub fn catalog(&self) -> Vec<CatalogEntry> {
		self.inner
			.catalog
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}

	/// When the most recent scan finished.
	pub fn scanned_at(&self) -> Option<SystemTime> {
		*self.inner.scanned_at.read().unwrap_or_else(PoisonError::into_inner)
	}

	/// Find an application that clients can see by ID in the most recent scan.
	pub fn find(&self, application_id: i32) -> Option<ApplicationConfig> {
		self.inner
			.catalog
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
			.find(|entry| !entry.hidden && entry.application.id() == application_id)
			.map(|entry| entry.application.clone())
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
			env: Default::default(),
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
	async fn hidden_applications_stay_in_the_catalog_only() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "a.desktop", "Alpha");
		write_desktop_entry(tempdir.path(), "b.desktop", "Beta");

		let configured = ApplicationConfig {
			title: "Alpha".to_string(),
			..Default::default()
		};
		let manager = ApplicationManager::with_hidden(
			vec![configured],
			vec![desktop_scanner(tempdir.path().to_path_buf())],
			vec!["Alpha".to_string(), "Beta".to_string()],
		);

		// Only scanned applications can be hidden; the configured one keeps its title, so the
		// scanned one becomes "Alpha (2)".
		assert_eq!(titles(&manager), ["Alpha", "Alpha (2)"]);
		let beta = manager
			.catalog()
			.into_iter()
			.find(|entry| entry.application.title == "Beta")
			.unwrap();
		assert!(beta.hidden);
		assert_eq!(beta.scanner, Some(0));
		assert!(manager.find(beta.application.id()).is_none());

		manager.set_hidden(Vec::new());
		assert_eq!(titles(&manager), ["Alpha", "Alpha (2)", "Beta"]);
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
