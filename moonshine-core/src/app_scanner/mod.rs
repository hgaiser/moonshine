use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use desktop::DesktopApplicationScannerConfig;
use heroic::HeroicApplicationScannerConfig;
use lutris::LutrisApplicationScannerConfig;
use steam::SteamApplicationScannerConfig;

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
}

/// An application scanner that tracks the state of its source data.
#[derive(Clone, Debug)]
pub struct ApplicationScanner {
	config: ApplicationScannerConfig,

	/// Modified timestamp of the source data, captured just before the last scan.
	last_scan_modified: Option<SystemTime>,
}

impl ApplicationScanner {
	pub fn new(config: ApplicationScannerConfig) -> Self {
		Self {
			config,
			last_scan_modified: None,
		}
	}

	/// Cheaply check whether the source data changed since the last scan.
	///
	/// Compares the modified timestamps of the files the scanner reads against
	/// those recorded during the last scan, so scans can be skipped entirely
	/// while nothing changed.
	pub fn needs_reload(&self) -> bool {
		self.source_modified() != self.last_scan_modified
	}

	/// Modified timestamp of the source data this scanner reads.
	///
	/// Returns `None` when there is no source data (yet).
	fn source_modified(&self) -> Option<SystemTime> {
		match &self.config {
			ApplicationScannerConfig::Steam(config) => steam::source_modified(config),
			ApplicationScannerConfig::Desktop(config) => desktop::source_modified(config),
			ApplicationScannerConfig::Lutris(config) => lutris::source_modified(config),
			ApplicationScannerConfig::Heroic(config) => heroic::source_modified(config),
		}
	}
}

/// Modified timestamp of a file, or `None` when it cannot be read.
fn modified_time(path: &Path) -> Option<SystemTime> {
	path.metadata().and_then(|metadata| metadata.modified()).ok()
}

/// Latest modified timestamp of the matching files under `roots`.
///
/// Directories are searched recursively, up to `max_depth` (1 = direct
/// children only). Scanners match the files they read with `matches`, so the
/// walk never has to open a file to decide whether a reload is needed.
pub(crate) fn latest_modified(
	roots: &[PathBuf],
	max_depth: usize,
	matches: impl Fn(&Path) -> bool,
) -> Option<SystemTime> {
	let mut latest = None;

	for root in roots {
		for entry in WalkDir::new(root)
			.follow_links(true)
			.max_depth(max_depth)
			.into_iter()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_type().is_file())
		{
			if !matches(entry.path()) {
				continue;
			}

			if let Some(modified) = modified_time(entry.path()) {
				latest = latest.max(Some(modified));
			}
		}
	}

	latest
}

pub fn scan_applications(application_scanners: &mut [ApplicationScanner]) -> Vec<ApplicationConfig> {
	let mut applications = Vec::new();
	let mut dedupe_keys = HashSet::new();

	for application_scanner in application_scanners {
		// Capture the timestamp before scanning: if the source data changes
		// while scanning, the next check still reports that a reload is needed.
		let source_modified = application_scanner.source_modified();

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
		};

		application_scanner.last_scan_modified = source_modified;

		for application in scanned_applications {
			let dedupe_key = (
				application.title.trim().to_ascii_lowercase(),
				application.command.join("\u{1f}"),
			);

			if dedupe_keys.insert(dedupe_key) {
				applications.push(application);
			}
		}
	}

	applications
}

/// Combine the configured applications with freshly scanned ones and resolve missing boxart.
pub fn load_applications(
	configured_applications: &[ApplicationConfig],
	application_scanners: &mut [ApplicationScanner],
) -> Vec<ApplicationConfig> {
	let mut applications = configured_applications.to_vec();
	let scanned_applications = scan_applications(application_scanners);
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
	fn a_failed_scan_keeps_the_scanner_stale() {
		let tempdir = tempdir().unwrap();
		let db = tempdir.path().join("pga.db");
		fs::write(&db, b"not a database").unwrap();

		let mut scanners = vec![lutris_scanner(db)];
		assert!(scan_applications(&mut scanners).is_empty());
		assert!(scanners[0].needs_reload());
	}
}
