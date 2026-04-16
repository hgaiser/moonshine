use std::{
	path::{Path, PathBuf},
	str::FromStr,
};

use walkdir::WalkDir;

use crate::config::{ApplicationConfig, SteamApplicationScannerConfig};

struct LibraryFolder {
	path: PathBuf,
	apps: Vec<u32>,
}

fn parse_vdf(vdf_content: &str) -> Vec<LibraryFolder> {
	let library_folder_depth = 2;
	let apps_depth = 3;
	let path_pattern = "\"path\"";
	let mut library_folders = Vec::new();
	let mut current_path: Option<PathBuf> = None;
	let mut current_apps: Vec<u32> = Vec::new();
	let mut depth = 0;

	for line in vdf_content.lines().map(|l| l.trim()) {
		if line == "{" {
			depth += 1;
			continue;
		}
		if line == "}" {
			depth -= 1;

			// if we go back to depth 1, it means we finished parsing lib folder block
			if depth == 1 {
				if let Some(path) = current_path.take() {
					// adding folder to list
					library_folders.push(LibraryFolder {
						path,
						apps: std::mem::take(&mut current_apps),
					});
				}
			}
			continue;
		}

		// library_folder depth
		// this block contains folder path
		if depth == library_folder_depth && line.starts_with(path_pattern) {
			if let Some(start_quote) = &line[path_pattern.len()..].find('"') {
				let path_start = path_pattern.len() + start_quote + 1;

				if let Some(end_quote) = line[path_start..].find('"') {
					let path_str = &line[path_start..path_start + end_quote];
					let parsed_path = PathBuf::from(path_str);

					current_path = Some(parsed_path);
				}
			}

		// apps list depth
		} else if depth == apps_depth && line.starts_with('"') {
			// game line: "game_id" "_"
			if let Some(id_end) = &line[1..].find('"') {
				let id_str = &line[1..1 + id_end];
				let parsed_id: u32 = match id_str.parse() {
					Ok(id) => id,
					Err(e) => {
						tracing::warn!("Failed to parse game id, {e}");
						continue;
					},
				};
				current_apps.push(parsed_id);
			}
		}
	}

	library_folders
}

pub fn scan_steam_applications(config: &SteamApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	let library_path = config
		.library
		.join("steamapps")
		.join("libraryfolders.vdf")
		.to_string_lossy()
		.to_string();
	let library_path =
		shellexpand::full(&library_path).map_err(|e| tracing::warn!("Failed to expand {library_path:?}: {e}"))?;
	let library =
		std::fs::read_to_string(library_path.as_ref()).map_err(|e| tracing::warn!("Failed to open library: {e}"))?;

	let library_folders = parse_vdf(&library);

	// Poor man's library parsing.
	let applications: Vec<ApplicationConfig> = library_folders
		.into_iter()
		.flat_map(|folder| {
			let folder_path = folder.path.clone();

			folder.apps.into_iter().filter_map(move |app_id| {
				let mut application = ApplicationConfig { ..Default::default() };

				let steamapps_dir = folder_path.join("steamapps");
				if let Ok(title) = get_game_name(app_id, &steamapps_dir) {
					application.title = title;
				} else {
					tracing::warn!("Could not get name of application '{app_id}'");
					return None;
				}

				// skip things that aren't really games.
				if application.title.starts_with("Proton")
					|| application.title.starts_with("Steam Linux Runtime")
					|| application.title.starts_with("Steamworks Common Redistributables")
				{
					return None;
				}

				application.command = config
					.command
					.clone()
					.iter()
					.map(|a| a.replace("{game_id}", &app_id.to_string()))
					.collect();

				let game_dir = config.library.join(format!("appcache/librarycache/{app_id}/"));
				if let Some(boxart) = search_file(&game_dir, "library_600x900.jpg")
					.or_else(|| search_file(&game_dir, "library_capsule.jpg"))
				{
					if boxart.exists() {
						application.boxart = Some(boxart);
					} else {
						tracing::warn!("No boxart for game '{}' at '{}.", application.title, boxart.display());
					}
				} else {
					tracing::debug!(
						"No boxart found for game '{}' in directory '{}'.",
						application.title,
						game_dir.display()
					);
				}

				Some(application)
			})
		})
		.collect();

	Ok(applications)
}

fn get_game_name<P: AsRef<Path>>(game_id: u32, steamapps_dir: P) -> Result<String, ()> {
	let manifest_path = steamapps_dir.as_ref().join(format!("appmanifest_{}.acf", game_id));
	let manifest = std::fs::read_to_string(&manifest_path)
		.map_err(|e| eprintln!("Failed to open Steam game manifest ({manifest_path:?}): {e}"))?;
	let name_line = manifest.lines().find(|l| l.contains("\"name\""));

	match name_line {
		Some(line) => line
			.split('\"')
			.nth(3)
			.ok_or_else(|| {
				eprintln!(
					"Line '{}' doesn't match expected format (expected: \"name\" \"<NAME>\").",
					line
				)
			})
			.map(|l| l.to_string()),
		None => {
			eprintln!("Couldn't find name for game with ID '{game_id}'.");
			Err(())
		},
	}
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
