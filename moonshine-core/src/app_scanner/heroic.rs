use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeroicApplicationScannerConfig {
	/// Path to the Heroic configuration directory (ie. `~/.config/heroic`).
	#[serde(default = "default_config_dir")]
	pub config_dir: PathBuf,

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

/// Directory Heroic writes its per store library caches into.
const STORE_CACHE_DIR: &str = "store_cache";

/// Suffix Heroic gives a library cache inside [`STORE_CACHE_DIR`].
const LIBRARY_SUFFIX: &str = "_library.json";

/// Heroic's manually added games, kept outside the store cache.
const SIDELOAD_LIBRARY: &str = "sideload_apps/library.json";

/// Collect every library cache Heroic has written.
///
/// The store caches are discovered rather than listed, so a store backend Heroic
/// adds later is picked up without a change here. Heroic keeps plenty besides
/// libraries in that directory (install info, achievements, cached API
/// responses), hence matching only on the library suffix.
fn find_libraries(config_dir: &Path) -> Vec<PathBuf> {
	let store_cache = config_dir.join(STORE_CACHE_DIR);

	let mut libraries = match std::fs::read_dir(&store_cache) {
		Ok(entries) => entries
			.flatten()
			.map(|entry| entry.path())
			.filter(|path| {
				path.file_name()
					.and_then(|name| name.to_str())
					.is_some_and(|name| name.ends_with(LIBRARY_SUFFIX))
			})
			.collect(),
		Err(e) => {
			tracing::warn!("Failed to read Heroic store cache {:?}: {e}", store_cache);
			Vec::new()
		},
	};

	// Directory order is arbitrary, so sort for a stable application list.
	libraries.sort();

	let sideload = config_dir.join(SIDELOAD_LIBRARY);
	if sideload.exists() {
		libraries.push(sideload);
	}

	libraries
}

/// Path of the manifest recording which of a store's games are installed.
///
/// Most stores record that in the library cache itself. GOG does not: Heroic
/// merges install state into a copy of each game and deliberately keeps it out
/// of the cache it writes, so every GOG entry on disk claims to be uninstalled.
/// Reading the manifest covers that, and is the fresher source regardless, since
/// Heroic rewrites it on every install and uninstall rather than only on a full
/// library refresh.
///
/// Stores that do record install state simply have no manifest here, which reads
/// as an empty one.
fn installed_manifest(config_dir: &Path, runner: &str) -> PathBuf {
	config_dir.join(format!("{runner}_store")).join("installed.json")
}

fn flatpak_config_dir() -> Option<PathBuf> {
	Some(
		dirs::home_dir()?
			.join(".var/app/com.heroicgameslauncher.hgl/config")
			.join("heroic"),
	)
}

fn native_config_dir() -> PathBuf {
	dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("heroic")
}

/// Prefer a native install, fall back to Flatpak if only that one is present.
fn default_config_dir() -> PathBuf {
	let native = native_config_dir();
	if native.exists() {
		return native;
	}

	match flatpak_config_dir() {
		Some(flatpak) if flatpak.exists() => flatpak,
		_ => native,
	}
}

/// The contents of a Heroic library cache.
///
/// Every backend serializes the same game shape, but they disagree on the key
/// the array lives under, so both spellings are accepted.
#[derive(Debug, Deserialize)]
struct HeroicLibrary {
	#[serde(alias = "games", default)]
	library: Vec<HeroicGame>,
}

/// A single entry of a Heroic library cache.
///
/// Heroic stores far more than this per game; only the fields Moonshine needs
/// are declared, the rest is ignored.
#[derive(Debug, Deserialize)]
struct HeroicGame {
	app_name: String,
	title: String,

	#[serde(default)]
	runner: Option<String>,

	#[serde(default)]
	is_installed: bool,

	#[serde(default)]
	install: HeroicInstall,

	/// Portrait cover art, the better fit for Moonlight's box art area.
	#[serde(default)]
	art_square: Option<String>,

	/// Landscape cover art, used when there is no portrait art.
	#[serde(default)]
	art_cover: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HeroicInstall {
	/// Heroic lists installed DLC alongside the games they belong to, each as
	/// its own library entry.
	#[serde(default)]
	is_dlc: bool,
}

/// The contents of a store's installed games manifest.
#[derive(Debug, Deserialize)]
struct HeroicInstalledManifest {
	#[serde(default)]
	installed: Vec<HeroicInstalledGame>,
}

/// A single entry of an installed games manifest.
#[derive(Debug, Deserialize)]
struct HeroicInstalledGame {
	/// Matches the `app_name` of the game's library entry.
	///
	/// Heroic tolerates entries without one and skips them, so this is optional.
	#[serde(default, rename = "appName")]
	app_name: Option<String>,

	#[serde(default)]
	is_dlc: bool,
}

pub(crate) fn scan_heroic_applications(config: &HeroicApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	let config_dir = &config.config_dir;

	let binding = config_dir.to_string_lossy();
	let expanded = shellexpand::full(&binding)
		.map_err(|e| tracing::warn!("Failed to expand Heroic configuration path {:?}: {e}", config_dir))?;

	let config_dir = PathBuf::from(expanded.as_ref());

	if !config_dir.exists() {
		tracing::warn!(
			"Heroic configuration directory not found at {:?}, no Heroic games will be scanned.",
			config_dir
		);
		return Ok(Vec::new());
	}

	let mut applications = Vec::new();
	let mut skipped = 0;
	let mut dlc = 0;

	// Manifests are read once per store, the first time one of its games is seen.
	let mut installed_manifests: HashMap<String, HashMap<String, bool>> = HashMap::new();

	for library_path in find_libraries(&config_dir) {
		let games = match read_library(&library_path) {
			Ok(games) => games,
			Err(()) => continue,
		};

		for game in games {
			// Sideloaded entries have no runner of their own.
			let runner = game.runner.as_deref().unwrap_or("sideload");

			if !installed_manifests.contains_key(runner) {
				let manifest = read_installed(&installed_manifest(&config_dir, runner));
				installed_manifests.insert(runner.to_string(), manifest);
			}
			let installed_entry = installed_manifests[runner].get(&game.app_name).copied();

			if !game.is_installed && installed_entry.is_none() {
				skipped += 1;
				continue;
			}

			// DLC is launched through its base game, never on its own. This also
			// drops Heroic's synthetic "Galaxy Common Redistributables" entry,
			// the one GOG entry that is always marked installed and which it
			// flags as DLC.
			if game.install.is_dlc || installed_entry.unwrap_or(false) {
				dlc += 1;
				continue;
			}

			let boxart = find_boxart(&config_dir, &game);

			let application = ApplicationConfig {
				title: game.title,
				pre_command: config.pre_command.clone(),
				post_command: config.post_command.clone(),
				command: config
					.command
					.iter()
					.map(|cmd| cmd.replace("{app_name}", &game.app_name).replace("{runner}", runner))
					.collect(),
				boxart,
				stdout: config.stdout.clone(),
				stderr: config.stderr.clone(),
				launch_timeout_secs: config.launch_timeout_secs,
			};

			applications.push(application);
		}
	}

	tracing::debug!(
		"Scanned {} Heroic games ({} not installed, {} DLC).",
		applications.len(),
		skipped,
		dlc
	);

	Ok(applications)
}

fn read_library(path: &Path) -> Result<Vec<HeroicGame>, ()> {
	let contents =
		std::fs::read_to_string(path).map_err(|e| tracing::warn!("Failed to read Heroic library {:?}: {e}", path))?;

	let library: HeroicLibrary = serde_json::from_str(&contents)
		.map_err(|e| tracing::warn!("Failed to parse Heroic library {:?}: {e}", path))?;

	Ok(library.library)
}

/// Read a store's installed games, mapping each app name to whether it is DLC.
///
/// A manifest that is missing or unreadable means nothing from that store is
/// treated as installed, which is the same outcome as before it was consulted.
fn read_installed(path: &Path) -> HashMap<String, bool> {
	if !path.exists() {
		tracing::debug!("Heroic installed games manifest {:?} not found, skipping.", path);
		return HashMap::new();
	}

	let Ok(contents) = std::fs::read_to_string(path)
		.map_err(|e| tracing::warn!("Failed to read Heroic installed games manifest {:?}: {e}", path))
	else {
		return HashMap::new();
	};

	let Ok(manifest) = serde_json::from_str::<HeroicInstalledManifest>(&contents)
		.map_err(|e| tracing::warn!("Failed to parse Heroic installed games manifest {:?}: {e}", path))
	else {
		return HashMap::new();
	};

	manifest
		.installed
		.into_iter()
		.filter_map(|game| Some((game.app_name?, game.is_dlc)))
		.collect()
}

/// Heroic requests Epic art through a resizing CDN and caches it under whichever
/// URL it actually requested, so the unmodified URL is usually a miss.
const ART_URL_VARIANTS: [&str; 3] = ["?h=800&resize=1&w=600", "?h=400&resize=1&w=300", ""];

/// Guard against an app name from the library cache escaping the icons directory.
fn is_valid_app_name(app_name: &str) -> bool {
	!app_name.is_empty()
		&& app_name
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Find the most box art like image Heroic has already downloaded.
///
/// Heroic never fetches art up front, so what is on disk depends on what its UI
/// has displayed. Portrait art is preferred over the landscape banner, which
/// only gets used when there is nothing better.
fn find_boxart(config_dir: &Path, game: &HeroicGame) -> Option<PathBuf> {
	// Full resolution portrait art, saved when Heroic downloads a game icon.
	//
	// Heroic always writes that art as a JPG. It only writes a PNG when it has
	// substituted a store's own icon for the art, to keep the transparency, and
	// that icon is a small square logo rather than cover art. Matching on the
	// extension alone keeps any store Heroic does that for out of the box art.
	if is_valid_app_name(&game.app_name) {
		let icon = config_dir.join("icons").join(format!("{}.jpg", game.app_name));
		if icon.exists() {
			return Some(icon);
		}
	}

	// Otherwise fall back to Heroic's image cache, which keys files by the
	// SHA256 of their source URL and stores them without an extension.
	let images_cache = config_dir.join("images-cache");
	[game.art_square.as_deref(), game.art_cover.as_deref()]
		.into_iter()
		.flatten()
		.filter(|url| !url.is_empty())
		.flat_map(|url| ART_URL_VARIANTS.map(|variant| format!("{url}{variant}")))
		.map(|url| images_cache.join(hex::encode(Sha256::digest(url.as_bytes()))))
		.find(|path| path.exists())
}

#[cfg(test)]
mod tests {
	use std::fs;

	use tempfile::tempdir;

	use super::*;

	fn scanner_config(config_dir: PathBuf) -> HeroicApplicationScannerConfig {
		HeroicApplicationScannerConfig {
			config_dir,
			command: vec![
				"/usr/bin/heroic".to_string(),
				"heroic://launch?appName={app_name}&runner={runner}".to_string(),
			],
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		}
	}

	fn write_library(config_dir: &Path, library_file: &str, contents: &str) {
		let path = config_dir.join(library_file);
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, contents).unwrap();
	}

	#[test]
	fn scans_installed_games_across_runners() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_library(
			config_dir,
			"store_cache/legendary_library.json",
			r#"{"library": [
				{"app_name": "epic-id", "title": "Epic Game", "runner": "legendary", "is_installed": true},
				{"app_name": "not-installed", "title": "Uninstalled", "runner": "legendary", "is_installed": false}
			]}"#,
		);
		// GOG entries are always written as uninstalled, so the manifest decides.
		write_library(
			config_dir,
			"store_cache/gog_library.json",
			r#"{"games": [
				{"app_name": "gog-id", "title": "GOG Game", "runner": "gog", "is_installed": false},
				{"app_name": "gog-unowned", "title": "GOG Unowned", "runner": "gog", "is_installed": false}
			]}"#,
		);
		write_library(
			config_dir,
			"gog_store/installed.json",
			r#"{"installed": [{"appName": "gog-id", "is_dlc": false}]}"#,
		);
		write_library(
			config_dir,
			"store_cache/nile_library.json",
			r#"{"library": [
				{"app_name": "amazon-id", "title": "Amazon Game", "runner": "nile", "is_installed": true}
			]}"#,
		);
		// Not a store Moonshine knows about by name, but it follows the same
		// layout, so discovery picks it up.
		write_library(
			config_dir,
			"store_cache/zoom_library.json",
			r#"{"games": [
				{"app_name": "zoom-id", "title": "Zoom Game", "runner": "zoom", "is_installed": true}
			]}"#,
		);
		write_library(
			config_dir,
			"sideload_apps/library.json",
			r#"{"games": [{"app_name": "side-id", "title": "Sideloaded", "is_installed": true}]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();

		// Store caches are scanned in name order, with sideloaded games last.
		let titles: Vec<&str> = applications.iter().map(|app| app.title.as_str()).collect();
		assert_eq!(
			titles,
			vec!["GOG Game", "Epic Game", "Amazon Game", "Zoom Game", "Sideloaded"]
		);

		assert_eq!(
			applications[0].command,
			vec!["/usr/bin/heroic", "heroic://launch?appName=gog-id&runner=gog"]
		);
		assert_eq!(
			applications[1].command,
			vec!["/usr/bin/heroic", "heroic://launch?appName=epic-id&runner=legendary"]
		);
		assert_eq!(
			applications[3].command,
			vec!["/usr/bin/heroic", "heroic://launch?appName=zoom-id&runner=zoom"]
		);
	}

	#[test]
	fn skips_installed_dlc() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_library(
			config_dir,
			"store_cache/legendary_library.json",
			r#"{"library": [
				{"app_name": "Eider", "title": "HITMAN 3", "runner": "legendary", "is_installed": true,
				 "install": {"is_dlc": false}},
				{"app_name": "EiderBonusSuitPack", "title": "HITMAN WOA - Trinity Pack", "runner": "legendary",
				 "is_installed": true, "install": {"is_dlc": true}},
				{"app_name": "no-install-block", "title": "No Install Block", "runner": "legendary", "is_installed": true}
			]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();

		let titles: Vec<&str> = applications.iter().map(|app| app.title.as_str()).collect();
		assert_eq!(titles, vec!["HITMAN 3", "No Install Block"]);
	}

	#[test]
	fn skips_gog_games_when_the_installed_manifest_is_missing() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_library(
			config_dir,
			"store_cache/gog_library.json",
			r#"{"games": [
				{"app_name": "gog-id", "title": "GOG Game", "runner": "gog", "is_installed": false}
			]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn skips_gog_dlc_recorded_in_the_installed_manifest() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		// Heroic always prepends this synthetic entry to the GOG cache, and it is
		// the only one it marks as installed.
		write_library(
			config_dir,
			"store_cache/gog_library.json",
			r#"{"games": [
				{"app_name": "gog-redist", "title": "Galaxy Common Redistributables", "runner": "gog",
				 "is_installed": true, "install": {"is_dlc": true}},
				{"app_name": "gog-id", "title": "GOG Game", "runner": "gog", "is_installed": false},
				{"app_name": "gog-dlc", "title": "GOG DLC", "runner": "gog", "is_installed": false}
			]}"#,
		);
		write_library(
			config_dir,
			"gog_store/installed.json",
			r#"{"installed": [
				{"appName": "gog-id", "is_dlc": false},
				{"appName": "gog-dlc", "is_dlc": true},
				{"install_path": "/games/orphan"}
			]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();

		let titles: Vec<&str> = applications.iter().map(|app| app.title.as_str()).collect();
		assert_eq!(titles, vec!["GOG Game"]);
	}

	#[test]
	fn defaults_missing_runner_to_sideload() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_library(
			config_dir,
			"sideload_apps/library.json",
			r#"{"games": [{"app_name": "side-id", "title": "Sideloaded", "is_installed": true}]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();

		assert_eq!(applications.len(), 1);
		assert_eq!(
			applications[0].command,
			vec!["/usr/bin/heroic", "heroic://launch?appName=side-id&runner=sideload"]
		);
	}

	const SQUARE_URL: &str = "https://example.com/square.jpg";
	const COVER_URL: &str = "https://example.com/cover.jpg";

	fn write_art_library(config_dir: &Path) {
		write_library(
			config_dir,
			"store_cache/legendary_library.json",
			&format!(
				r#"{{"library": [{{
					"app_name": "epic-id",
					"title": "Epic Game",
					"runner": "legendary",
					"is_installed": true,
					"art_square": "{SQUARE_URL}",
					"art_cover": "{COVER_URL}"
				}}]}}"#
			),
		);
	}

	fn cache_art(config_dir: &Path, url: &str) -> PathBuf {
		let images_cache = config_dir.join("images-cache");
		fs::create_dir_all(&images_cache).unwrap();
		let path = images_cache.join(hex::encode(Sha256::digest(url.as_bytes())));
		fs::write(&path, b"image").unwrap();
		path
	}

	fn scan_one_boxart(config_dir: &Path) -> Option<PathBuf> {
		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();
		assert_eq!(applications.len(), 1);
		applications[0].boxart.clone()
	}

	#[test]
	fn falls_back_to_the_banner_when_it_is_the_only_cached_art() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_art_library(config_dir);
		let cover_path = cache_art(config_dir, COVER_URL);

		assert_eq!(scan_one_boxart(config_dir), Some(cover_path));
	}

	#[test]
	fn prefers_portrait_art_cached_under_a_resized_url() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_art_library(config_dir);
		cache_art(config_dir, COVER_URL);
		// Heroic requests Epic art through a resizing CDN, so this is how the
		// portrait art actually lands in the cache.
		let square_path = cache_art(config_dir, &format!("{SQUARE_URL}?h=400&resize=1&w=300"));

		assert_eq!(scan_one_boxart(config_dir), Some(square_path));
	}

	#[test]
	fn prefers_the_full_resolution_icon_over_the_image_cache() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_art_library(config_dir);
		cache_art(config_dir, COVER_URL);
		cache_art(config_dir, &format!("{SQUARE_URL}?h=400&resize=1&w=300"));

		let icon = write_icon(config_dir, "epic-id.jpg");

		assert_eq!(scan_one_boxart(config_dir), Some(icon));
	}

	fn write_gog_art_library(config_dir: &Path) {
		write_library(
			config_dir,
			"store_cache/gog_library.json",
			&format!(
				r#"{{"games": [{{
					"app_name": "gog-id",
					"title": "GOG Game",
					"runner": "gog",
					"is_installed": false,
					"art_square": "{SQUARE_URL}",
					"art_cover": "{COVER_URL}"
				}}]}}"#
			),
		);
		write_library(
			config_dir,
			"gog_store/installed.json",
			r#"{"installed": [{"appName": "gog-id", "is_dlc": false}]}"#,
		);
	}

	fn write_icon(config_dir: &Path, name: &str) -> PathBuf {
		let icons = config_dir.join("icons");
		fs::create_dir_all(&icons).unwrap();
		let icon = icons.join(name);
		fs::write(&icon, b"image").unwrap();
		icon
	}

	#[test]
	fn passes_over_a_substituted_store_icon_for_cover_art() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_gog_art_library(config_dir);

		// Heroic requests GOG art without the resizing parameters it adds for Epic.
		let square_path = cache_art(config_dir, SQUARE_URL);

		// A PNG under icons/ is the store's own logo, not cover art.
		write_icon(config_dir, "gog-id.png");

		assert_eq!(scan_one_boxart(config_dir), Some(square_path));
	}

	#[test]
	fn still_uses_the_icons_entry_of_a_store_that_substitutes_icons() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_gog_art_library(config_dir);
		cache_art(config_dir, SQUARE_URL);

		// Heroic only writes a JPG here when it had no icon to substitute, in
		// which case it is the portrait art at full resolution.
		let icon = write_icon(config_dir, "gog-id.jpg");

		assert_eq!(scan_one_boxart(config_dir), Some(icon));
	}

	#[test]
	fn ignores_an_app_name_that_would_escape_the_icons_directory() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_library(
			config_dir,
			"store_cache/legendary_library.json",
			r#"{"library": [
				{"app_name": "../../etc/passwd", "title": "Traversal", "runner": "legendary", "is_installed": true}
			]}"#,
		);

		assert_eq!(scan_one_boxart(config_dir), None);
	}

	#[test]
	fn returns_nothing_when_heroic_is_not_installed() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path().join("missing");

		let applications = scan_heroic_applications(&scanner_config(config_dir)).unwrap();
		assert!(applications.is_empty());
	}
}
