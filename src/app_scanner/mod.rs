use std::collections::HashSet;

use crate::config::{ApplicationConfig, ApplicationScannerConfig};

mod desktop;
mod steam;

pub fn scan_applications(application_scanners: &Vec<ApplicationScannerConfig>) -> Vec<ApplicationConfig> {
	let mut applications = Vec::new();
	let mut dedupe_keys = HashSet::new();

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
		};

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

/// Resolve missing boxart for applications by searching for icons matching the application title.
pub fn resolve_missing_boxart(applications: &mut [ApplicationConfig]) {
	let resolver = desktop::IconResolver::new(true);
	for app in applications.iter_mut() {
		if app.boxart.is_none() {
			app.boxart = resolver.find_icon_by_name(&app.title.to_ascii_lowercase());
		}
	}
}
