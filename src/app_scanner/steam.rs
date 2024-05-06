use std::{path::{Path, PathBuf}, str::FromStr};

use crate::config::{SteamApplicationScannerConfig, ApplicationConfig};

pub fn scan_steam_applications(config: &SteamApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	let library_path = config.library.join("steamapps").join("libraryfolders.vdf").to_string_lossy().to_string();
	let library_path = shellexpand::full(&library_path)
		.map_err(|e| tracing::error!("Failed to expand {library_path:?}: {e}"))?;
	let library = std::fs::read_to_string(library_path.as_ref())
		.map_err(|e| tracing::warn!("Failed to open library: {e}"))?;

	// Poor man's library parsing.
	let start_apps = library.find("apps")
		.ok_or_else(|| tracing::warn!("Failed to find 'apps' key in {library_path:?}."))?;
	let library = &library[start_apps..];
	let stop_apps = library.find('}')
		.ok_or_else(|| tracing::warn!("Failed to find end of 'apps' section."))?;
	let library = &library[..stop_apps];

	let mut applications = Vec::new();
	for line in library.lines().skip(2) {
		let mut application = ApplicationConfig::default();

		if line.trim().is_empty() {
			continue;
		}

		let game_id = match line.split('\"').nth(1) {
			Some(game_id) => game_id,
			None => {
				tracing::warn!("Failed to parse library entry: '{line}'");
				continue;
			},
		};

		let game_id: u32 = match game_id.parse() {
			Ok(game_id) => game_id,
			Err(e) => {
				tracing::warn!("Failed to parse game id: {e}");
				continue;
			},
		};

		application.title = match get_game_name(game_id, library_path.as_ref()) {
			Ok(title) => title,
			Err(()) => continue,
		};

		// Skip things that aren't really games.
		if application.title.starts_with("Proton")
			|| application.title.starts_with("Steam Linux Runtime")
			|| application.title.starts_with("Steamworks Common Redistributables") {
			continue;
		}

		if let Some(run_before) = &config.run_before {
			application.run_before = Some(
				run_before
					.clone()
					.iter_mut()
					.map(|c| {
						c
							.iter_mut()
							.map(|a| a.replace("{game_id}", &game_id.to_string()))
							.collect()
					})
					.collect()
			);
		}

		if let Some(run_after) = &config.run_after {
			application.run_after = Some(
				run_after
					.clone()
					.iter_mut()
					.map(|c| {
						c
							.iter_mut()
							.map(|a| a.replace("{game_id}", &game_id.to_string()))
							.collect()
					})
					.collect()
			);
		}

		let boxart = config.library.join(format!("appcache/librarycache/{game_id}_library_600x900.jpg"));
		if let Ok(boxart) = shellexpand::full(&boxart.to_string_lossy()) {
			match PathBuf::from_str(&boxart) {
				Ok(path) => {
					if path.exists() {
						application.boxart = Some(path);
					} else {
						tracing::warn!("No boxart for game '{}' at '{boxart}", application.title);
					}
				},
				Err(e) => {
					tracing::warn!("Failed to parse boxart path: {e}");
				}
			}
		}

		applications.push(application);
	}

	Ok(applications)
}

fn get_game_name<P: AsRef<Path>>(game_id: u32, library: P) -> Result<String, ()> {
	let manifest_path = library
		.as_ref()
		.parent()
		.ok_or_else(|| eprintln!("Expected '{:?}' to have a parent, but couldn't find one.", library.as_ref()))?
		.join(format!("appmanifest_{}.acf", game_id));
	let manifest = std::fs::read_to_string(&manifest_path)
		.map_err(|e| eprintln!("Failed to open Steam game manifest ({manifest_path:?}): {e}"))?;
	let name_line = manifest
		.lines()
		.find(|l| l.contains("\"name\""));

	match name_line {
		Some(line) => {
			line
				.split('\"')
				.nth(3)
				.ok_or_else(|| eprintln!("Line '{}' doesn't match expected format (expected: \"name\" \"<NAME>\").", line))
				.map(|l| l.to_string())
		},
		None => {
			eprintln!("Couldn't find name for game with ID '{game_id}'.");
			Err(())
		},
	}
}
