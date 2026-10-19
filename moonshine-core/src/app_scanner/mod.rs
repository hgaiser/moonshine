use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use bottles::BottlesApplicationScannerConfig;
use desktop::DesktopApplicationScannerConfig;
use heroic::HeroicApplicationScannerConfig;
use lutris::LutrisApplicationScannerConfig;
use steam::SteamApplicationScannerConfig;

pub mod bottles;
pub mod desktop;
pub mod heroic;
pub mod lutris;
pub mod steam;

use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ApplicationScannerConfig {
	/// Scans a 'libraryfolders.vdf' file from a Steam library directory.
	Steam(SteamApplicationScannerConfig),

	/// Scans directories containing freedesktop .desktop launchers.
	Desktop(DesktopApplicationScannerConfig),

	/// Scans the Lutris game database.
	Lutris(LutrisApplicationScannerConfig),

	/// Scans the Heroic Games Launcher library caches.
	Heroic(HeroicApplicationScannerConfig),

	/// Scans the Bottles library.
	Bottles(BottlesApplicationScannerConfig),
}

/// Fingerprint of the files a scanner reads: their paths and modified timestamps.
///
/// Comparing fingerprints detects files that were added, changed or deleted. The
/// default fingerprint stands for "no source files".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceFingerprint(u64);

/// An application scanner that tracks the state of its source data.
#[derive(Clone, Debug)]
pub struct ApplicationScanner {
	config: ApplicationScannerConfig,

	/// Fingerprint of the source data, captured just before the last scan.
	last_scan_fingerprint: Option<SourceFingerprint>,
}

impl ApplicationScanner {
	pub fn new(config: ApplicationScannerConfig) -> Self {
		Self {
			config,
			last_scan_fingerprint: None,
		}
	}

	/// Cheaply check whether the source data changed since the last scan.
	///
	/// Compares a fingerprint of the files the scanner reads against the one
	/// recorded during the last scan, so scans can be skipped entirely while
	/// nothing changed.
	pub fn needs_reload(&self) -> bool {
		Some(self.source_fingerprint()) != self.last_scan_fingerprint
	}

	/// Fingerprint of the source data this scanner reads.
	fn source_fingerprint(&self) -> SourceFingerprint {
		match &self.config {
			ApplicationScannerConfig::Steam(config) => steam::source_fingerprint(config),
			ApplicationScannerConfig::Desktop(config) => desktop::source_fingerprint(config),
			ApplicationScannerConfig::Lutris(config) => lutris::source_fingerprint(config),
			ApplicationScannerConfig::Heroic(config) => heroic::source_fingerprint(config),
			ApplicationScannerConfig::Bottles(config) => bottles::source_fingerprint(config),
		}
	}
}

/// Fingerprint of the matching files under `roots`.
///
/// Directories are searched recursively, up to `max_depth` (1 = direct
/// children only). Scanners match the files they read with `matches`, so the
/// walk never has to open a file to decide whether a reload is needed. The
/// fingerprint covers every matching path, not just the newest timestamp, so
/// deleting a file changes it too.
pub(crate) fn source_fingerprint(
	roots: &[PathBuf],
	max_depth: usize,
	matches: impl Fn(&Path) -> bool,
) -> SourceFingerprint {
	let mut files: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();

	for root in roots {
		for entry in WalkDir::new(root)
			.follow_links(true)
			.max_depth(max_depth)
			.into_iter()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_type().is_file())
		{
			if matches(entry.path()) {
				let modified = entry.metadata().ok().and_then(|metadata| metadata.modified().ok());
				files.push((entry.into_path(), modified));
			}
		}
	}

	if files.is_empty() {
		return SourceFingerprint::default();
	}

	// Walk order is not stable across directories, so sort before hashing.
	files.sort();
	let mut hasher = DefaultHasher::new();
	files.hash(&mut hasher);
	SourceFingerprint(hasher.finish())
}

/// Scan all configured application scanners.
///
/// Scanned applications whose title matches one of the `configured` applications, or one
/// scanned earlier, get a numeric suffix (`Title (2)`). The client-visible application id
/// is a hash of the title alone, so a repeated title would make the later application
/// unlaunchable. Configured applications keep their titles.
pub fn scan_applications(
	application_scanners: &mut [ApplicationScanner],
	configured: &[ApplicationConfig],
) -> Vec<ApplicationConfig> {
	let mut applications = Vec::new();
	let mut dedupe_keys = HashSet::new();
	let mut used_titles: HashSet<String> = configured.iter().map(|app| title_key(&app.title)).collect();

	for application_scanner in application_scanners {
		// Capture the fingerprint before scanning: if the source data changes
		// while scanning, the next check still reports that a reload is needed.
		let source_fingerprint = application_scanner.source_fingerprint();

		let scanned_applications = match &application_scanner.config {
			ApplicationScannerConfig::Steam(config) => match steam::scan_steam_applications(config) {
				Ok(steam_applications) => steam_applications,
				Err(()) => continue,
			},
			ApplicationScannerConfig::Desktop(config) => match desktop::scan_desktop_applications(config) {
				Ok(desktop_applications) => desktop_applications,
				Err(()) => continue,
			},
			ApplicationScannerConfig::Lutris(config) => match lutris::scan_lutris_applications(config) {
				Ok(lutris_applications) => lutris_applications,
				Err(()) => continue,
			},
			ApplicationScannerConfig::Heroic(config) => match heroic::scan_heroic_applications(config) {
				Ok(heroic_applications) => heroic_applications,
				Err(()) => continue,
			},
			ApplicationScannerConfig::Bottles(config) => match bottles::scan_bottles_applications(config) {
				Ok(bottles_applications) => bottles_applications,
				Err(()) => continue,
			},
		};

		application_scanner.last_scan_fingerprint = Some(source_fingerprint);

		for mut application in scanned_applications {
			let dedupe_key = (title_key(&application.title), application.command.join("\u{1f}"));

			// The same application found twice (same title and command) is dropped; a different
			// application that merely shares a title is kept under a unique title.
			if !dedupe_keys.insert(dedupe_key) {
				continue;
			}

			application.title = unique_title(&application.title, &mut used_titles);
			applications.push(application);
		}
	}

	applications
}

fn title_key(title: &str) -> String {
	title.trim().to_ascii_lowercase()
}

/// Return `title`, or `title (N)` with the lowest `N >= 2` that is not in `used`, and record it.
fn unique_title(title: &str, used: &mut HashSet<String>) -> String {
	let mut candidate = title.to_string();
	let mut n = 2;
	while !used.insert(title_key(&candidate)) {
		candidate = format!("{title} ({n})");
		n += 1;
	}
	candidate
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unique_title_suffixes_repeats() {
		let mut used = HashSet::new();
		assert_eq!(unique_title("Hades", &mut used), "Hades");
		assert_eq!(unique_title("hades", &mut used), "hades (2)");
		assert_eq!(unique_title("Hades", &mut used), "Hades (3)");
	}

	#[test]
	fn unique_title_avoids_titles_that_already_look_suffixed() {
		let mut used = HashSet::from([title_key("Hades"), title_key("Hades (2)")]);
		assert_eq!(unique_title("Hades", &mut used), "Hades (3)");
	}
}

/// Combine the configured applications with freshly scanned ones and resolve missing boxart.
pub fn load_applications(
	configured_applications: &[ApplicationConfig],
	application_scanners: &mut [ApplicationScanner],
) -> Vec<ApplicationConfig> {
	let mut applications = configured_applications.to_vec();
	let scanned_applications = scan_applications(application_scanners, configured_applications);
	tracing::debug!("Adding scanned applications:\n{:#?}", scanned_applications);
	applications.extend(scanned_applications);
	resolve_missing_boxart(&mut applications);
	applications
}

/// Resolve missing boxart for applications by searching for icons matching the application title.
pub fn resolve_missing_boxart(applications: &mut [ApplicationConfig]) {
	let resolver = desktop::IconResolver::new(true);
	for app in applications.iter_mut() {
		if app.boxart.is_none() {
			app.boxart = resolver.find_icon_by_name(&app.title.to_ascii_lowercase());
		}
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
	use desktop::DesktopApplicationScannerConfig;
	use lutris::LutrisApplicationScannerConfig;

	fn desktop_scanner(directory: PathBuf) -> ApplicationScanner {
		ApplicationScanner::new(ApplicationScannerConfig::Desktop(DesktopApplicationScannerConfig {
			directories: vec![directory],
			include_terminal: false,
			resolve_icons: false,
			pre_command: Vec::new(),
			post_command: Vec::new(),
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		}))
	}

	fn lutris_scanner(pga_db: PathBuf) -> ApplicationScanner {
		ApplicationScanner::new(ApplicationScannerConfig::Lutris(LutrisApplicationScannerConfig {
			pga_db,
			command: vec!["/usr/bin/lutris".to_string(), "lutris://rungameid/{slug}".to_string()],
			pre_command: Vec::new(),
			post_command: Vec::new(),
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		}))
	}

	fn write_desktop_entry(directory: &Path, filename: &str, exec: &str) {
		fs::write(
			directory.join(filename),
			format!("[Desktop Entry]\nType=Application\nName=Game\nExec={exec}\n"),
		)
		.unwrap();
	}

	#[test]
	fn a_scanner_needs_a_reload_until_it_has_scanned() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "game.desktop", "game");

		let mut scanners = vec![desktop_scanner(tempdir.path().to_path_buf())];
		assert!(scanners[0].needs_reload());

		assert_eq!(scan_applications(&mut scanners).len(), 1);
		assert!(!scanners[0].needs_reload());
	}

	#[test]
	fn a_scanner_needs_a_reload_when_its_source_data_changed() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "game.desktop", "game");

		let mut scanners = vec![desktop_scanner(tempdir.path().to_path_buf())];
		assert_eq!(scan_applications(&mut scanners).len(), 1);
		assert!(!scanners[0].needs_reload());

		sleep(Duration::from_millis(10));
		write_desktop_entry(tempdir.path(), "other.desktop", "other");

		assert!(scanners[0].needs_reload());
		assert_eq!(scan_applications(&mut scanners).len(), 2);
		assert!(!scanners[0].needs_reload());
	}

	#[test]
	fn a_scanner_needs_a_reload_when_a_source_file_was_deleted() {
		let tempdir = tempdir().unwrap();
		write_desktop_entry(tempdir.path(), "old.desktop", "old");
		sleep(Duration::from_millis(10));
		write_desktop_entry(tempdir.path(), "new.desktop", "new");

		let mut scanners = vec![desktop_scanner(tempdir.path().to_path_buf())];
		assert_eq!(scan_applications(&mut scanners).len(), 2);
		assert!(!scanners[0].needs_reload());

		// The newest file stays, so the latest timestamp alone would not notice.
		fs::remove_file(tempdir.path().join("old.desktop")).unwrap();

		assert!(scanners[0].needs_reload());
		assert_eq!(scan_applications(&mut scanners).len(), 1);
	}

	#[test]
	fn a_failed_scan_keeps_the_scanner_stale() {
		let tempdir = tempdir().unwrap();
		let db = tempdir.path().join("pga.db");
		fs::write(&db, b"not a database").unwrap();

		let mut scanners = vec![lutris_scanner(db)];
		assert!(scan_applications(&mut scanners).is_empty());
		assert!(scanners[0].needs_reload());
	}
}
