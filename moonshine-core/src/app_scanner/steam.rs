use std::{
	path::{Path, PathBuf},
	str::FromStr,
	time::SystemTime,
};

use serde::{Deserialize, Serialize};
use steamlocate::SteamDir;
use walkdir::WalkDir;

use super::latest_modified;
use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteamApplicationScannerConfig {
	/// Path to a Steam library (ie. `~/.local/share/Steam`).
	pub library: PathBuf,

	/// The command to run.
	pub command: Vec<String>,

	/// Commands to run before launching each scanned application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after each scanned application's session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// systemd StandardOutput value for launched applications.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<String>,

	/// systemd StandardError value for launched applications.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<String>,

	/// Seconds to wait for each scanned application to reach an active state after launch.
	#[serde(default = "crate::session::application::default_launch_timeout")]
	pub launch_timeout_secs: u64,
}

pub(crate) fn scan_steam_applications(config: &SteamApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	// Expand the library path.
	let library_str = config.library.to_string_lossy();
	let library = shellexpand::full(&library_str)
		.map_err(|e| tracing::warn!("Failed to expand library path {:?}: {e}", config.library))?;

	// Create SteamDir from the expanded path.
	let steam_dir = SteamDir::from_dir(Path::new(&*library))
		.map_err(|e| tracing::warn!("Failed to locate Steam directory at {:?}: {e}", library))?;

	// Iterate over all libraries.
	let mut applications = Vec::new();
	for library_result in steam_dir
		.libraries()
		.map_err(|e| tracing::warn!("Failed to list Steam libraries: {e}"))?
	{
		let library = match library_result {
			Ok(lib) => lib,
			Err(e) => {
				tracing::warn!("Failed to read library: {e}");
				continue;
			},
		};

		// Iterate over all installed apps in this library.
		for app_result in library.apps() {
			let app = match app_result {
				Ok(app) => app,
				Err(e) => {
					tracing::warn!("Failed to read app manifest: {e}");
					continue;
				},
			};

			// Skip apps without a name.
			let title = match &app.name {
				Some(name) => name.clone(),
				None => {
					tracing::debug!("Skipping app {} without a name.", app.app_id);
					continue;
				},
			};

			// Skip Proton, Steam Linux Runtime, and Steamworks Common Redistributables.
			if title.starts_with("Proton")
				|| title.starts_with("Steam Linux Runtime")
				|| title.starts_with("Steamworks Common Redistributables")
			{
				continue;
			}

			// Build the ApplicationConfig.
			let mut application = ApplicationConfig {
				title,
				pre_command: config.pre_command.clone(),
				post_command: config.post_command.clone(),
				command: config
					.command
					.iter()
					.map(|cmd| cmd.replace("{game_id}", &app.app_id.to_string()))
					.collect(),
				boxart: None,
				stdout: config.stdout.clone(),
				stderr: config.stderr.clone(),
				launch_timeout_secs: config.launch_timeout_secs,
			};

			// Search for boxart.
			let game_dir = config
				.library
				.join("appcache/librarycache")
				.join(app.app_id.to_string());
			if let Some(boxart) =
				search_file(&game_dir, "library_600x900.jpg").or_else(|| search_file(&game_dir, "library_capsule.jpg"))
			{
				if boxart.exists() {
					application.boxart = Some(boxart);
				} else {
					tracing::warn!("No boxart for game '{}' at '{}'.", application.title, boxart.display());
				}
			} else {
				tracing::debug!(
					"No boxart found for game '{}' in directory '{}'.",
					application.title,
					game_dir.display()
				);
			}

			applications.push(application);
		}
	}

	Ok(applications)
}

/// Modified timestamp of the most recently changed file the scan reads.
///
/// That is the library list (`steamapps/libraryfolders.vdf`) and every game
/// manifest (`steamapps/appmanifest_*.acf`) in each library. The steamapps
/// directories are only searched one level deep, so game data and Proton
/// prefixes are never walked.
pub(crate) fn source_modified(config: &SteamApplicationScannerConfig) -> Option<SystemTime> {
	// Expand the library path.
	let library_str = config.library.to_string_lossy();
	let Ok(library) = shellexpand::full(&library_str) else {
		return None;
	};
	let Ok(steam_dir) = SteamDir::from_dir(Path::new(&*library)) else {
		return None;
	};

	let mut steamapps_directories = vec![steam_dir.path().join("steamapps")];
	if let Ok(libraries) = steam_dir.libraries() {
		steamapps_directories.extend(libraries.flatten().map(|library| library.path().join("steamapps")));
	}

	latest_modified(&steamapps_directories, 1, |path| {
		let filename = path.file_name().and_then(|filename| filename.to_str());
		filename.is_some_and(|filename| {
			filename == "libraryfolders.vdf" || (filename.starts_with("appmanifest_") && filename.ends_with(".acf"))
		})
	})
}

fn search_file(directory: &Path, filename: &str) -> Option<PathBuf> {
	let binding = directory.to_string_lossy();
	let directory = match shellexpand::full(&binding) {
		Ok(directory) => directory,
		Err(_) => return None,
	};

	let directory = match PathBuf::from_str(&directory) {
		Ok(directory) => directory,
		Err(_) => return None,
	};

	for entry in WalkDir::new(&directory)
		.follow_links(true)
		.into_iter()
		.filter_map(|e| e.ok())
	{
		let entry_filename = entry.file_name().to_string_lossy();

		if entry_filename == filename {
			return Some(entry.into_path());
		}
	}

	None
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::thread::sleep;
	use std::time::Duration;

	use tempfile::tempdir;

	use super::*;

	fn scanner_config(library: PathBuf) -> SteamApplicationScannerConfig {
		SteamApplicationScannerConfig {
			library,
			command: vec!["/usr/bin/steam".to_string(), "steam://rungameid/{game_id}".to_string()],
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		}
	}

	fn write_library_folders(steam_dir: &Path) {
		let steamapps = steam_dir.join("steamapps");
		fs::create_dir_all(&steamapps).unwrap();
		fs::write(
			steamapps.join("libraryfolders.vdf"),
			format!(
				"\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
				steam_dir.display()
			),
		)
		.unwrap();
	}

	#[test]
	fn source_modified_tracks_manifest_changes() {
		let tempdir = tempdir().unwrap();
		let steam_dir = tempdir.path().join("Steam");
		write_library_folders(&steam_dir);

		let config = scanner_config(steam_dir.clone());

		// With only the library list present, that file decides the timestamp.
		let modified = source_modified(&config).unwrap();

		fs::write(steam_dir.join("steamapps").join("appmanifest_400.acf"), "manifest").unwrap();
		assert!(source_modified(&config).unwrap() > modified);

		sleep(Duration::from_millis(10));
		fs::write(steam_dir.join("steamapps").join("appmanifest_400.acf"), "changed").unwrap();
		assert!(source_modified(&config).unwrap() > modified);
	}

	#[test]
	fn source_modified_is_none_without_a_steam_directory() {
		let tempdir = tempdir().unwrap();
		let config = scanner_config(tempdir.path().join("missing"));

		assert_eq!(source_modified(&config), None);
	}
}
