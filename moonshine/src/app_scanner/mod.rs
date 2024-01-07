use crate::config::{ApplicationScannerConfig, ApplicationConfig};

mod steam;

pub fn scan_applications(application_scanners: &Vec<ApplicationScannerConfig>) -> Vec<ApplicationConfig> {
	let mut applications = Vec::new();

	for application_scanner in application_scanners {
		match application_scanner {
			ApplicationScannerConfig::Steam(config) => {
				match steam::scan_steam_applications(config) {
					Ok(steam_applications) => applications.extend(steam_applications),
					Err(()) => continue,
				}
			},
		}
	}

	applications
}
