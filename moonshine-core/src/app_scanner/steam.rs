use std::{
	path::{Path, PathBuf},
	str::FromStr,
};

use serde::{Deserialize, Serialize};
use steamlocate::SteamDir;
use walkdir::WalkDir;

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
