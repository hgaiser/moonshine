use std::collections::HashSet;

use serde::{Deserialize, Serialize};

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

/// Scan all configured application scanners.
///
/// Scanned applications whose title matches one of the `configured` applications, or one
/// scanned earlier, get a numeric suffix (`Title (2)`). The client-visible application id
/// is a hash of the title alone, so a repeated title would make the later application
/// unlaunchable. Configured applications keep their titles.
pub fn scan_applications(
	application_scanners: &Vec<ApplicationScannerConfig>,
	configured: &[ApplicationConfig],
) -> Vec<ApplicationConfig> {
	let mut applications = Vec::new();
	let mut dedupe_keys = HashSet::new();
	let mut used_titles: HashSet<String> = configured.iter().map(|app| title_key(&app.title)).collect();

	for application_scanner in application_scanners {
		let scanned_applications = match application_scanner {
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

/// Resolve missing boxart for applications by searching for icons matching the application title.
pub fn resolve_missing_boxart(applications: &mut [ApplicationConfig]) {
	let resolver = desktop::IconResolver::new(true);
	for app in applications.iter_mut() {
		if app.boxart.is_none() {
			app.boxart = resolver.find_icon_by_name(&app.title.to_ascii_lowercase());
		}
	}
}
