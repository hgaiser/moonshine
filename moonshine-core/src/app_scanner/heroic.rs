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

/// Library caches written by Heroic, one per store backend: Epic (legendary),
/// GOG (gog), Amazon (nile) and manually added games.
const LIBRARY_FILES: [&str; 4] = [
	"store_cache/legendary_library.json",
	"store_cache/gog_library.json",
	"store_cache/nile_library.json",
	"sideload_apps/library.json",
];

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
	/// Left untyped so a single malformed game can be dropped without losing
	/// the whole library.
	#[serde(alias = "games", default)]
	library: Vec<serde_json::Value>,
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

pub(crate) fn scan_heroic_applications(config: &HeroicApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	let config_dir = &config.config_dir;

	let binding = config_dir.to_string_lossy();
	let expanded = shellexpand::full(&binding)
		.map_err(|e| tracing::warn!("Failed to expand Heroic configuration path {:?}: {e}", config_dir))?;

	let config_dir = PathBuf::from(expanded.as_ref());

	if !config_dir.exists() {
		tracing::debug!("Heroic configuration directory not found at {:?}.", config_dir);
		return Ok(Vec::new());
	}

	let mut applications = Vec::new();
	let mut skipped = 0;
	let mut dlc = 0;

	for library_file in LIBRARY_FILES {
		let library_path = config_dir.join(library_file);
		if !library_path.exists() {
			tracing::debug!("Heroic library {:?} not found, skipping.", library_path);
			continue;
		}

		let games = match read_library(&library_path) {
			Ok(games) => games,
			Err(()) => continue,
		};

		for game in games {
			if !game.is_installed {
				skipped += 1;
				continue;
			}

			// DLC is launched through its base game, never on its own.
			if game.install.is_dlc {
				dlc += 1;
				continue;
			}

			// Sideloaded entries have no runner of their own.
			let runner = game.runner.as_deref().unwrap_or("sideload");
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

	Ok(library
		.library
		.into_iter()
		.filter_map(|game| match serde_json::from_value::<HeroicGame>(game) {
			Ok(game) => Some(game),
			Err(e) => {
				tracing::debug!("Skipping unparseable entry in Heroic library {:?}: {e}", path);
				None
			},
		})
		.collect())
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
	if is_valid_app_name(&game.app_name) {
		let icons = config_dir.join("icons");
		for extension in ["jpg", "png"] {
			let icon = icons.join(format!("{}.{extension}", game.app_name));
			if icon.exists() {
				return Some(icon);
			}
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
		write_library(
			config_dir,
			"store_cache/gog_library.json",
			r#"{"games": [
				{"app_name": "gog-id", "title": "GOG Game", "runner": "gog", "is_installed": true}
			]}"#,
		);
		write_library(
			config_dir,
			"store_cache/nile_library.json",
			r#"{"library": [
				{"app_name": "amazon-id", "title": "Amazon Game", "runner": "nile", "is_installed": true}
			]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();

		let titles: Vec<&str> = applications.iter().map(|app| app.title.as_str()).collect();
		assert_eq!(titles, vec!["Epic Game", "GOG Game", "Amazon Game"]);

		assert_eq!(
			applications[0].command,
			vec!["/usr/bin/heroic", "heroic://launch?appName=epic-id&runner=legendary"]
		);
		assert_eq!(
			applications[1].command,
			vec!["/usr/bin/heroic", "heroic://launch?appName=gog-id&runner=gog"]
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

		let icons = config_dir.join("icons");
		fs::create_dir_all(&icons).unwrap();
		let icon = icons.join("epic-id.jpg");
		fs::write(&icon, b"image").unwrap();

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
	fn skips_malformed_entries_without_dropping_the_library() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path();

		write_library(
			config_dir,
			"store_cache/legendary_library.json",
			r#"{"library": [
				{"title": "Missing App Name", "runner": "legendary", "is_installed": true},
				{"app_name": "epic-id", "title": "Epic Game", "runner": "legendary", "is_installed": true}
			]}"#,
		);

		let applications = scan_heroic_applications(&scanner_config(config_dir.to_path_buf())).unwrap();

		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Epic Game");
	}

	#[test]
	fn returns_nothing_when_heroic_is_not_installed() {
		let tempdir = tempdir().unwrap();
		let config_dir = tempdir.path().join("missing");

		let applications = scan_heroic_applications(&scanner_config(config_dir)).unwrap();
		assert!(applications.is_empty());
	}
}
