use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BottlesApplicationScannerConfig {
	/// Path to the Bottles data directory (ie. `~/.local/share/bottles`).
	///
	/// Bottles stores its data in `~/.local/share/bottles` when installed
	/// natively and in `~/.var/app/com.usebottles.bottles/data/bottles` as a
	/// Flatpak.
	#[serde(default = "default_data_dir")]
	pub data_dir: PathBuf,

	/// The command to run for entries stored in a bottle.
	///
	/// `{bottle}` is replaced with the bottle name, `{program_id}` with the
	/// entry's program id, and `{name}` with the entry's display name.
	pub command: Vec<String>,

	/// The command to run for entries backed by Bottles' UMU integration.
	///
	/// `{umu_game}` is replaced with the entry's UMU game id, `{name}` with the
	/// entry's display name. Optional: without it, UMU entries are skipped.
	#[serde(default)]
	pub umu_command: Vec<String>,

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

/// Name of the file Bottles stores its Library in, inside the data directory.
const LIBRARY_YML: &str = "library.yml";

/// Bottles keeps bottle prefixes in a `bottles/` subdirectory of its data directory.
const BOTTLES_DIR: &str = "bottles";

/// Grid art Bottles downloaded for a bottle lives in its `grids/` directory.
const GRIDS_DIR: &str = "grids";

/// Bottles stores UMU games and their covers in these data directory subdirectories.
const UMU_DIR: &str = "umu";
const UMU_GAMES_DIR: &str = "games";
const UMU_COVERS_DIR: &str = "covers";
const UMU_GAME_CONFIG: &str = "game.yml";

fn native_data_dir() -> PathBuf {
	dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("bottles")
}

fn flatpak_data_dir() -> Option<PathBuf> {
	Some(
		dirs::home_dir()?
			.join(".var/app/com.usebottles.bottles/data")
			.join("bottles"),
	)
}

/// Pick the data directory that actually holds a `library.yml`.
///
/// Leftover files from an uninstalled native Bottles can leave an otherwise
/// empty native directory behind after the user moved to the Flatpak, so
/// preferring the native directory purely on its existence would scan nothing.
/// When neither candidate holds a library, fall back to whichever directory
/// exists, and to the native directory when neither does, so the warning names a
/// directory the user actually has.
fn choose_data_dir(native: &Path, flatpak: Option<&Path>) -> PathBuf {
	let mut candidates = vec![native.to_path_buf()];
	candidates.extend(flatpak.map(Path::to_path_buf));

	for candidate in &candidates {
		if candidate.join(LIBRARY_YML).exists() {
			return candidate.clone();
		}
	}

	candidates
		.into_iter()
		.find(|candidate| candidate.exists())
		.unwrap_or_else(|| native.to_path_buf())
}

fn default_data_dir() -> PathBuf {
	let native = native_data_dir();
	let flatpak = flatpak_data_dir();
	choose_data_dir(&native, flatpak.as_deref())
}

/// A single entry of Bottles' `library.yml`, keyed by a uuid Bottles generates.
///
/// Bottles stores far more per entry; only the fields Moonshine needs are
/// declared. Every field is optional so one malformed entry fails only itself.
#[derive(Debug, Deserialize)]
struct BottlesLibraryEntry {
	name: Option<String>,
	id: Option<String>,

	#[serde(default)]
	bottle: Option<BottlesEntryBottle>,

	#[serde(default)]
	icon: Option<String>,

	#[serde(default)]
	thumbnail: Option<String>,

	/// Entries Bottles launches through Steam carry this marker; Bottles
	/// cannot launch them itself.
	#[serde(default)]
	steam: bool,

	/// `umu` for entries backed by Bottles' UMU integration. Older entries
	/// have no `source` at all and are regular bottle entries.
	#[serde(default)]
	source: Option<String>,

	#[serde(default)]
	source_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BottlesEntryBottle {
	#[serde(default)]
	name: Option<String>,

	/// Bottle directory name inside `bottles/`, or an absolute path for
	/// bottles created with a custom path.
	#[serde(default)]
	path: Option<String>,
}

/// The subset of a UMU game's config Moonshine reads: its readiness state
/// (`draft`, `installing`, `ready`, `failed` or `stopped`).
#[derive(Debug, Deserialize)]
struct UmuGameConfig {
	#[serde(default)]
	state: Option<String>,
}

pub(crate) fn scan_bottles_applications(
	config: &BottlesApplicationScannerConfig,
) -> Result<Vec<ApplicationConfig>, ()> {
	let data_dir = &config.data_dir;

	let binding = data_dir.to_string_lossy();
	let expanded = shellexpand::full(&binding)
		.map_err(|e| tracing::warn!("Failed to expand Bottles data directory path {:?}: {e}", data_dir))?;

	let data_dir = PathBuf::from(expanded.as_ref());

	let library_path = data_dir.join(LIBRARY_YML);
	if !library_path.exists() {
		tracing::warn!(
			"Bottles library not found at {:?}, no Bottles games will be scanned.",
			library_path
		);
		return Ok(Vec::new());
	}

	let contents = std::fs::read_to_string(&library_path)
		.map_err(|e| tracing::warn!("Failed to read Bottles library {:?}: {e}", library_path))?;

	// Bottles writes an empty document when its Library is empty, so the parse
	// target is optional: that reads as no entries, not as a failure. A genuine
	// parse failure still aborts the Bottles scan, leaving other scanners alone.
	let library: Option<HashMap<String, yaml_serde::Value>> = yaml_serde::from_str(&contents)
		.map_err(|e| tracing::warn!("Failed to parse Bottles library {:?}: {e}", library_path))?;

	// Deserialize entry by entry: one entry with an unexpected type has to cost
	// only that entry rather than the whole library.
	let mut entries: Vec<BottlesLibraryEntry> = Vec::new();
	for (uuid, value) in library.unwrap_or_default() {
		match yaml_serde::from_value::<BottlesLibraryEntry>(value) {
			Ok(entry) => entries.push(entry),
			Err(e) => tracing::debug!("Skipping malformed Bottles library entry {uuid}: {e}"),
		}
	}

	// Directory and map order are arbitrary, so sort for a stable application
	// list and deterministic title disambiguation.
	entries.sort_by_key(entry_sort_key);

	let mut applications: Vec<(ApplicationConfig, String)> = Vec::new();
	let mut skipped = 0;

	for entry in entries {
		let Some(name) = entry.name.as_deref().map(str::trim).filter(|name| !name.is_empty()) else {
			skipped += 1;
			continue;
		};

		// Bottles drops id-less entries when it loads the library; do the same.
		let Some(id) = entry.id.as_deref().filter(|id| !id.is_empty()) else {
			skipped += 1;
			continue;
		};

		if entry.steam {
			skipped += 1;
			continue;
		}

		if entry.source.as_deref() == Some("umu") {
			if config.umu_command.is_empty() {
				tracing::debug!("Skipping UMU entry '{name}': no umu_command configured.");
				skipped += 1;
				continue;
			}

			let Some(source_id) = entry.source_id.as_deref().filter(|id| is_safe_path_component(id)) else {
				tracing::debug!("Skipping UMU entry '{name}' without a usable source id.");
				skipped += 1;
				continue;
			};

			// Readiness lives in Bottles' internal UMU store, not library.yml.
			// Bottles' own UI refuses to launch anything not `ready`; expose
			// only those, and skip when the state file is absent or unreadable.
			let state_path = data_dir
				.join(UMU_DIR)
				.join(UMU_GAMES_DIR)
				.join(source_id)
				.join(UMU_GAME_CONFIG);
			let ready = read_umu_state(&state_path).is_some_and(|state| state == "ready");
			if !ready {
				tracing::debug!("Skipping UMU entry '{name}' that is not ready ({:?}).", state_path);
				skipped += 1;
				continue;
			}

			let command = substitute(&config.umu_command, &[("{umu_game}", source_id), ("{name}", name)]);

			applications.push((
				build_application(config, name, command, find_boxart(&data_dir, &entry, None)),
				"UMU".to_string(),
			));
		} else {
			let Some(bottle_name) = entry
				.bottle
				.as_ref()
				.and_then(|bottle| bottle.name.as_deref().filter(|n| !n.is_empty()))
			else {
				tracing::debug!("Skipping entry '{name}' without a bottle.");
				skipped += 1;
				continue;
			};

			let Some(bottle_dir) = bottle_dir(&data_dir, &entry.bottle) else {
				skipped += 1;
				continue;
			};

			// Bottles removes entries for deleted bottles only lazily, from its
			// UI; drop them here so they never become dead launch buttons.
			if !bottle_dir.exists() {
				tracing::debug!(
					"Skipping entry '{name}' whose bottle {:?} no longer exists.",
					bottle_dir
				);
				skipped += 1;
				continue;
			}

			let command = substitute(
				&config.command,
				&[("{bottle}", bottle_name), ("{program_id}", id), ("{name}", name)],
			);

			applications.push((
				build_application(config, name, command, find_boxart(&data_dir, &entry, Some(&bottle_dir))),
				bottle_name.to_string(),
			));
		}
	}

	// Moonshine derives an application's client-visible id from its title, so
	// two entries sharing a title would collide. Keep the first (sorted)
	// occurrence plain and suffix the rest, keeping every application
	// launchable.
	let mut used_titles: HashSet<String> = HashSet::new();
	let mut result = Vec::with_capacity(applications.len());
	for (mut application, disambiguator) in applications {
		application.title = unique_title(&application.title, &disambiguator, &mut used_titles);
		result.push(application);
	}

	tracing::debug!("Scanned {} Bottles games ({} skipped).", result.len(), skipped);

	Ok(result)
}

/// Make a title unique among the ones already emitted.
///
/// The first occurrence keeps its title. Later ones gain ` (bottle)`; when that
/// is taken too (three entries sharing a title inside one bottle, or several
/// UMU entries), an occurrence count is appended so titles stay unique.
fn unique_title(base: &str, disambiguator: &str, used: &mut HashSet<String>) -> String {
	let mut candidate = base.to_string();

	if used.contains(&candidate.trim().to_ascii_lowercase()) {
		candidate = format!("{base} ({disambiguator})");

		let mut occurrence = 1;
		while used.contains(&candidate.trim().to_ascii_lowercase()) {
			occurrence += 1;
			candidate = format!("{base} ({disambiguator} {occurrence})");
		}
	}

	used.insert(candidate.trim().to_ascii_lowercase());
	candidate
}

fn entry_sort_key(entry: &BottlesLibraryEntry) -> (String, String, String, String) {
	let bottle = entry
		.bottle
		.as_ref()
		.and_then(|bottle| bottle.name.clone())
		.unwrap_or_default();
	let source_id = entry.source_id.clone().unwrap_or_default();
	// The id breaks ties: two entries sharing a name and a bottle would otherwise
	// compare equal, leaving the library map's arbitrary order to decide which of
	// them keeps the plain title (and with it the client-visible id).
	(
		entry.name.clone().unwrap_or_default(),
		bottle,
		source_id,
		entry.id.clone().unwrap_or_default(),
	)
}

/// Resolve a bottle's prefix directory, handling bottles created with a
/// custom path, whose `path` is absolute.
///
/// A relative path has to stay a plain directory name: it comes from
/// `library.yml`, and joining `..` onto the data directory would point the scan
/// at an unrelated part of the filesystem.
fn bottle_dir(data_dir: &Path, bottle: &Option<BottlesEntryBottle>) -> Option<PathBuf> {
	let path = bottle.as_ref()?.path.as_deref().or(bottle.as_ref()?.name.as_deref())?;
	let path = Path::new(path);

	if path.is_absolute() {
		return Some(path.to_path_buf());
	}

	let name = path.to_str()?;
	if !is_safe_path_component(name) {
		tracing::debug!("Ignoring bottle path {name:?} that is not a plain directory name.");
		return None;
	}

	Some(data_dir.join(BOTTLES_DIR).join(name))
}

/// Read a UMU game's readiness state from Bottles' internal store.
///
/// A missing or unparsable state file reads as `None`, which the caller
/// treats as not ready, so this fails safe on Bottles layout changes.
fn read_umu_state(path: &Path) -> Option<String> {
	let contents = std::fs::read_to_string(path).ok()?;
	let config: UmuGameConfig = yaml_serde::from_str(&contents).ok()?;
	config.state
}

/// Find the most box art like image Bottles has on disk.
///
/// Thumbnails Bottles curated (`grid:` for bottles, `umu-grid:` for UMU
/// games) win; the icon extracted from the program's executable is the
/// fallback. Anything unresolved is left to Moonshine's icon resolution.
fn find_boxart(data_dir: &Path, entry: &BottlesLibraryEntry, bottle_dir: Option<&Path>) -> Option<PathBuf> {
	if let Some(thumbnail) = entry.thumbnail.as_deref() {
		if let Some(file) = thumbnail.strip_prefix("grid:")
			&& let Some(bottle_dir) = bottle_dir
			&& is_safe_path_component(file)
		{
			let candidate = bottle_dir.join(GRIDS_DIR).join(file);
			if candidate.is_file() {
				return Some(candidate);
			}
		} else if let Some(file) = thumbnail.strip_prefix("umu-grid:")
			&& is_safe_path_component(file)
		{
			let candidate = data_dir.join(UMU_DIR).join(UMU_COVERS_DIR).join(file);
			if candidate.is_file() {
				return Some(candidate);
			}
		}
	}

	let icon = entry.icon.as_deref().map(str::trim).filter(|icon| !icon.is_empty())?;
	let icon = PathBuf::from(icon);
	// Bottles writes absolute paths. A relative one belongs to the data
	// directory, not to whatever directory Moonshine happened to start in.
	let icon = if icon.is_absolute() { icon } else { data_dir.join(icon) };
	icon.is_file().then_some(icon)
}

fn substitute(template: &[String], replacements: &[(&str, &str)]) -> Vec<String> {
	template
		.iter()
		.map(|token| substitute_token(token, replacements))
		.collect()
}

/// Expand the `{token}` placeholders of one argv element in a single pass.
///
/// One pass matters: replacing token by token over the whole string would expand
/// text that an earlier replacement inserted, so a bottle named `{name}` would
/// pull the entry's name into the command.
fn substitute_token(token: &str, replacements: &[(&str, &str)]) -> String {
	let mut expanded = String::with_capacity(token.len());
	let mut rest = token;

	while let Some(start) = rest.find('{') {
		expanded.push_str(&rest[..start]);
		let placeholder = &rest[start..];

		let Some(end) = placeholder.find('}') else {
			expanded.push_str(placeholder);
			return expanded;
		};

		// Keep the braces: callers pass the token exactly as it appears in the
		// template, so `{bottle}` matches `{bottle}`.
		let token = &placeholder[..=end];
		match replacements.iter().find(|(name, _)| *name == token) {
			Some((_, value)) => expanded.push_str(value),
			None => expanded.push_str(token),
		}

		rest = &placeholder[end + 1..];
	}

	expanded.push_str(rest);
	expanded
}

fn build_application(
	config: &BottlesApplicationScannerConfig,
	title: &str,
	command: Vec<String>,
	boxart: Option<PathBuf>,
) -> ApplicationConfig {
	ApplicationConfig {
		title: title.to_string(),
		pre_command: config.pre_command.clone(),
		post_command: config.post_command.clone(),
		command,
		boxart,
		stdout: config.stdout.clone(),
		stderr: config.stderr.clone(),
		launch_timeout_secs: config.launch_timeout_secs,
	}
}

/// Guard against a value from a Bottles file escaping the directory it is
/// joined into (thumbnail filenames, UMU game ids, relative bottle paths).
fn is_safe_path_component(value: &str) -> bool {
	!value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\'])
}

#[cfg(test)]
mod tests {
	use std::fs;

	use tempfile::tempdir;

	use super::*;

	const LIBRARY_ENTRY: &str = r#"
89ebedb3-0be5-453b-a39c-623c1c8bbed0:
  bottle:
    name: Gaming
    path: Gaming
  icon: /icons/hades.png
  id: abc-123
  name: Hades
  thumbnail: grid:cover.jpg
"#;

	fn scanner_config(data_dir: PathBuf) -> BottlesApplicationScannerConfig {
		BottlesApplicationScannerConfig {
			data_dir,
			command: vec![
				"bottles-cli".to_string(),
				"run".into(),
				"-b".into(),
				"{bottle}".into(),
				"--program-id".into(),
				"{program_id}".into(),
			],
			umu_command: vec![
				"bottles-cli".to_string(),
				"umu".into(),
				"run".into(),
				"--game".into(),
				"{umu_game}".into(),
			],
			pre_command: Vec::new(),
			post_command: Vec::new(),
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		}
	}

	fn write_library(data_dir: &Path, contents: &str) {
		fs::create_dir_all(data_dir).unwrap();
		fs::write(data_dir.join(LIBRARY_YML), contents).unwrap();
	}

	fn make_bottle(data_dir: &Path, path: &str) -> PathBuf {
		let bottle_dir = data_dir.join(BOTTLES_DIR).join(path);
		fs::create_dir_all(&bottle_dir).unwrap();
		bottle_dir
	}

	fn write_grid(bottle_dir: &Path, name: &str) -> PathBuf {
		let grids = bottle_dir.join(GRIDS_DIR);
		fs::create_dir_all(&grids).unwrap();
		let grid = grids.join(name);
		fs::write(&grid, b"image").unwrap();
		grid
	}

	fn write_icon(data_dir: &Path, name: &str) -> PathBuf {
		let icon = data_dir.join(name);
		fs::write(&icon, b"image").unwrap();
		icon
	}

	fn write_umu_game(data_dir: &Path, source_id: &str, state: &str) {
		let dir = data_dir.join(UMU_DIR).join(UMU_GAMES_DIR).join(source_id);
		fs::create_dir_all(&dir).unwrap();
		fs::write(dir.join(UMU_GAME_CONFIG), format!("state: {state}\n")).unwrap();
	}

	#[test]
	fn scans_bottle_entry_with_grid_boxart() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(data_dir, LIBRARY_ENTRY);
		let bottle_dir = make_bottle(data_dir, "Gaming");
		let grid = write_grid(&bottle_dir, "cover.jpg");

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Hades");
		assert_eq!(
			applications[0].command,
			vec!["bottles-cli", "run", "-b", "Gaming", "--program-id", "abc-123"]
		);
		assert_eq!(applications[0].boxart, Some(grid));
	}

	#[test]
	fn returns_nothing_when_bottles_is_not_installed() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("missing");

		let applications = scan_bottles_applications(&scanner_config(data_dir)).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn tolerates_empty_and_null_libraries() {
		for contents in ["", "{}\n", "null\n"] {
			let tempdir = tempdir().unwrap();
			let data_dir = tempdir.path();
			write_library(data_dir, contents);

			let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();
			assert!(applications.is_empty(), "library {contents:?}");
		}
	}

	#[test]
	fn skips_entries_without_identity() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
no-id:
  name: No Id
no-name:
  id: abc
"#,
		);
		make_bottle(data_dir, "Gaming");

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn skips_steam_shortcut_entries() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
steam-entry:
  bottle:
    name: "123456"
    path: "123456"
  id: steam-id
  name: Steam Game
  steam: true
"#,
		);

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn skips_entries_whose_bottle_is_missing() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(data_dir, LIBRARY_ENTRY);

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn exposes_ready_umu_entries() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
umu-entry:
  id: umu:def-456
  name: UMU Game
  source: umu
  source_id: def-456
  thumbnail: umu-grid:umu-cover.jpg
"#,
		);
		write_umu_game(data_dir, "def-456", "ready");
		let covers = data_dir.join(UMU_DIR).join(UMU_COVERS_DIR);
		fs::create_dir_all(&covers).unwrap();
		let cover = covers.join("umu-cover.jpg");
		fs::write(&cover, b"image").unwrap();

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "UMU Game");
		assert_eq!(
			applications[0].command,
			vec!["bottles-cli", "umu", "run", "--game", "def-456"]
		);
		assert_eq!(applications[0].boxart, Some(cover));
	}

	#[test]
	fn skips_umu_entries_that_are_not_ready() {
		for state in ["draft", "installing", "failed", "stopped"] {
			let tempdir = tempdir().unwrap();
			let data_dir = tempdir.path();
			write_library(
				data_dir,
				r#"
umu-entry:
  id: umu:def-456
  name: UMU Game
  source: umu
  source_id: def-456
"#,
			);
			write_umu_game(data_dir, "def-456", state);

			let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();
			assert!(applications.is_empty(), "state {state}");
		}
	}

	#[test]
	fn skips_umu_entries_without_a_state_file() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
umu-entry:
  id: umu:def-456
  name: UMU Game
  source: umu
  source_id: def-456
"#,
		);

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn disambiguates_duplicate_titles_with_the_bottle_name() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
first:
  bottle:
    name: Gaming
    path: Gaming
  id: one
  name: Hades
second:
  bottle:
    name: Work
    path: Work
  id: two
  name: Hades
"#,
		);
		make_bottle(data_dir, "Gaming");
		make_bottle(data_dir, "Work");

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		let titles: Vec<&str> = applications.iter().map(|app| app.title.as_str()).collect();
		assert_eq!(titles, vec!["Hades", "Hades (Work)"]);
	}

	#[test]
	fn falls_back_to_the_entry_icon_for_boxart() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		let icon = write_icon(data_dir, "hades.png");
		write_library(
			data_dir,
			&format!(
				r#"
entry:
  bottle:
    name: Gaming
    path: Gaming
  icon: {}
  id: abc-123
  name: Hades
"#,
				icon.display()
			),
		);
		make_bottle(data_dir, "Gaming");

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert_eq!(applications[0].boxart, Some(icon));
	}

	#[test]
	fn rejects_a_thumbnail_filename_that_escapes_the_grids_directory() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		let icon = write_icon(data_dir, "hades.png");
		write_library(
			data_dir,
			&format!(
				r#"
entry:
  bottle:
    name: Gaming
    path: Gaming
  icon: {}
  id: abc-123
  name: Hades
  thumbnail: grid:../../etc/passwd
"#,
				icon.display()
			),
		);
		make_bottle(data_dir, "Gaming");
		// The unguarded join walks bottle_dir/grids/../../etc/passwd, and a ".." walk
		// only resolves when the directory before it exists, so create both grids/ and
		// the file the walk lands on. A missing guard then selects the planted file and
		// this assertion fails.
		fs::create_dir_all(data_dir.join("bottles/Gaming/grids")).unwrap();
		let escaped = data_dir.join("bottles/etc/passwd");
		fs::create_dir_all(escaped.parent().unwrap()).unwrap();
		fs::write(&escaped, b"planted").unwrap();

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert_eq!(applications[0].boxart, Some(icon));
	}

	#[test]
	fn propagates_launch_options_to_every_application() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(data_dir, LIBRARY_ENTRY);
		make_bottle(data_dir, "Gaming");

		let mut config = scanner_config(data_dir.to_path_buf());
		config.pre_command = vec![vec!["/usr/bin/echo".into(), "pre".into()]];
		config.post_command = vec![vec!["/usr/bin/echo".into(), "post".into()]];
		config.launch_timeout_secs = 10;
		config.stdout = Some("journal".to_string());

		let applications = scan_bottles_applications(&config).unwrap();

		assert_eq!(applications[0].pre_command, vec![vec!["/usr/bin/echo", "pre"]]);
		assert_eq!(applications[0].post_command, vec![vec!["/usr/bin/echo", "post"]]);
		assert_eq!(applications[0].launch_timeout_secs, 10);
		assert_eq!(applications[0].stdout, Some("journal".to_string()));
	}

	#[test]
	#[ignore = "manual: reads a real Bottles data directory"]
	fn scans_real_bottles_data() {
		let Some(data_dir) = std::env::var_os("BOTTLES_TEST_DATA_DIR") else {
			panic!("Set BOTTLES_TEST_DATA_DIR to a real Bottles data directory.");
		};

		let applications = scan_bottles_applications(&scanner_config(PathBuf::from(data_dir))).unwrap();

		for application in &applications {
			println!(
				"{}: {:?} boxart={:?}",
				application.title, application.command, application.boxart
			);
		}
	}

	#[test]
	fn prefers_the_directory_that_contains_the_library() {
		let tempdir = tempdir().unwrap();
		let root = tempdir.path();

		let native = root.join("native");
		fs::create_dir_all(&native).unwrap();
		let flatpak = root.join("flatpak");
		write_library(&flatpak, LIBRARY_ENTRY);

		assert_eq!(choose_data_dir(&native, Some(&flatpak)), flatpak);

		let missing_native = root.join("native-missing");
		assert_eq!(choose_data_dir(&missing_native, Some(&native)), native);

		assert_eq!(choose_data_dir(&native, None), native);
	}

	#[test]
	fn a_malformed_entry_costs_only_itself() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
good:
  bottle:
    name: Gaming
    path: Gaming
  id: good-id
  name: Hades
bad:
  bottle:
    name: Gaming
    path: Gaming
  id: bad-id
  name: [1, 2]
"#,
		);
		make_bottle(data_dir, "Gaming");

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Hades");
	}

	#[test]
	fn keeps_a_third_same_titled_entry_distinct() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
first:
  bottle:
    name: Gaming
    path: Gaming
  id: a
  name: Hades
second:
  bottle:
    name: Gaming
    path: Gaming
  id: b
  name: Hades
third:
  bottle:
    name: Gaming
    path: Gaming
  id: c
  name: Hades
"#,
		);
		make_bottle(data_dir, "Gaming");

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		let titles: Vec<&str> = applications.iter().map(|app| app.title.as_str()).collect();
		assert_eq!(titles, vec!["Hades", "Hades (Gaming)", "Hades (Gaming 2)"]);
	}

	#[test]
	fn ignores_a_bottle_path_that_escapes_the_data_directory() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
entry:
  bottle:
    name: Escape
    path: ../evil
  id: abc-123
  name: Hades
"#,
		);
		// The unguarded join is data_dir/bottles/../evil, and a ".." walk only resolves
		// when the directory before it exists, so create bottles/ as well. With both in
		// place a missing guard exposes the entry and this assertion fails.
		fs::create_dir_all(data_dir.join("bottles")).unwrap();
		fs::create_dir_all(data_dir.join("evil")).unwrap();

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert!(applications.is_empty());
	}

	#[test]
	fn resolves_a_relative_icon_against_the_data_directory() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
entry:
  bottle:
    name: Gaming
    path: Gaming
  icon: icons/hades.png
  id: abc-123
  name: Hades
"#,
		);
		make_bottle(data_dir, "Gaming");
		let icon = data_dir.join("icons/hades.png");
		fs::create_dir_all(icon.parent().unwrap()).unwrap();
		fs::write(&icon, b"image").unwrap();

		let applications = scan_bottles_applications(&scanner_config(data_dir.to_path_buf())).unwrap();

		assert_eq!(applications[0].boxart, Some(icon));
	}

	#[test]
	fn substitution_does_not_expand_the_values_it_inserts() {
		let expanded = substitute(
			&["-b".to_string(), "{bottle}".to_string(), "{name}".to_string()],
			&[("{bottle}", "{name}"), ("{name}", "Hades")],
		);

		assert_eq!(expanded, vec!["-b", "{name}", "Hades"]);
	}

	#[test]
	fn umu_command_is_optional() {
		let config: BottlesApplicationScannerConfig = toml::from_str(
			r#"
data_dir = "/tmp/bottles"
command = ["bottles-cli", "run"]
"#,
		)
		.unwrap();

		assert!(config.umu_command.is_empty());
	}

	#[test]
	fn skips_umu_entries_when_no_umu_command_is_configured() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path();
		write_library(
			data_dir,
			r#"
umu-entry:
  id: umu:def-456
  name: UMU Game
  source: umu
  source_id: def-456
"#,
		);
		write_umu_game(data_dir, "def-456", "ready");

		let mut config = scanner_config(data_dir.to_path_buf());
		config.umu_command = Vec::new();

		let applications = scan_bottles_applications(&config).unwrap();

		assert!(applications.is_empty());
	}
}
