use std::{
	collections::HashMap,
	env, fs,
	path::{Path, PathBuf},
};

use walkdir::WalkDir;

use crate::config::{ApplicationConfig, DesktopApplicationScannerConfig};

const SUPPORTED_IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "ico"];
const SKIPPED_EXEC_FIELD_CODES: [char; 10] = ['f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'v', 'm'];

pub fn scan_desktop_applications(config: &DesktopApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	let mut applications = Vec::new();
	let mut dedupe_keys = std::collections::HashSet::new();
	let mut icon_resolver = IconResolver::new(config.resolve_icons);

	for directory in &config.directories {
		let Some(directory) = expand_path(directory) else {
			continue;
		};

		if !directory.exists() {
			tracing::debug!(
				"Skipping missing desktop application directory '{}'.",
				directory.display()
			);
			continue;
		}

		for entry in WalkDir::new(&directory)
			.follow_links(true)
			.into_iter()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_type().is_file())
		{
			if entry
				.path()
				.extension()
				.and_then(|extension| extension.to_str())
				.is_none_or(|extension| !extension.eq_ignore_ascii_case("desktop"))
			{
				continue;
			}

			let Some(application) = parse_desktop_application(entry.path(), config, &mut icon_resolver)? else {
				continue;
			};

			let dedupe_key = (
				application.title.trim().to_ascii_lowercase(),
				application.command.join("\u{1f}"),
			);
			if dedupe_keys.insert(dedupe_key) {
				applications.push(application);
			}
		}
	}

	Ok(applications)
}

fn parse_desktop_application(
	path: &Path,
	config: &DesktopApplicationScannerConfig,
	icon_resolver: &mut IconResolver,
) -> Result<Option<ApplicationConfig>, ()> {
	let entry = match DesktopEntry::load(path) {
		Ok(Some(entry)) => entry,
		Ok(None) => return Ok(None),
		Err(()) => return Ok(None),
	};

	if !entry.is_application() || entry.is_hidden() || entry.is_no_display() {
		return Ok(None);
	}

	if entry.is_terminal() && !config.include_terminal {
		return Ok(None);
	}

	check_try_exec(&entry);

	let Some(title) = entry.name() else {
		tracing::debug!("Skipping desktop entry without a name at '{}'.", path.display());
		return Ok(None);
	};

	let Some(command) = parse_exec_command(&entry, path) else {
		tracing::debug!("Skipping desktop entry without a usable Exec at '{}'.", path.display());
		return Ok(None);
	};

	let boxart = icon_resolver.resolve(entry.icon().map(str::trim).filter(|icon| !icon.is_empty()), path);

	Ok(Some(ApplicationConfig { title, boxart, command }))
}

#[derive(Debug, Default)]
struct DesktopEntry {
	fields: HashMap<String, String>,
	localized_names: HashMap<String, String>,
}

impl DesktopEntry {
	fn load(path: &Path) -> Result<Option<Self>, ()> {
		let contents = fs::read_to_string(path)
			.map_err(|e| tracing::warn!("Failed to read desktop entry '{}': {e}", path.display()))?;
		let mut entry = DesktopEntry::default();
		let mut in_desktop_entry = false;
		let mut found_desktop_entry = false;

		for line in contents.lines() {
			let line = line.trim();
			if line.is_empty() || line.starts_with('#') {
				continue;
			}

			if line.starts_with('[') && line.ends_with(']') {
				let section = &line[1..line.len() - 1];
				if section == "Desktop Entry" {
					in_desktop_entry = true;
					found_desktop_entry = true;
					continue;
				}

				if found_desktop_entry {
					break;
				}

				in_desktop_entry = false;
				continue;
			}

			if !in_desktop_entry {
				continue;
			}

			let Some((key, value)) = line.split_once('=') else {
				continue;
			};

			let key = key.trim();
			let value = unescape_desktop_value(value.trim());
			if let Some(locale) = key.strip_prefix("Name[").and_then(|key| key.strip_suffix(']')) {
				entry.localized_names.insert(locale.to_string(), value);
			} else {
				entry.fields.insert(key.to_string(), value);
			}
		}

		if !found_desktop_entry {
			return Ok(None);
		}

		Ok(Some(entry))
	}

	fn is_application(&self) -> bool {
		self.fields.get("Type").is_some_and(|value| value == "Application")
	}

	fn is_hidden(&self) -> bool {
		self.boolean_field("Hidden")
	}

	fn is_no_display(&self) -> bool {
		self.boolean_field("NoDisplay")
	}

	fn is_terminal(&self) -> bool {
		self.boolean_field("Terminal")
	}

	fn boolean_field(&self, key: &str) -> bool {
		self.fields
			.get(key)
			.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
	}

	fn name(&self) -> Option<String> {
		self.fields.get("Name").cloned().or_else(|| {
			let locale_preferences = locale_preferences();
			for locale in locale_preferences {
				if let Some(name) = self.localized_names.get(&locale) {
					return Some(name.clone());
				}
			}

			None
		})
	}

	fn icon(&self) -> Option<&str> {
		self.fields.get("Icon").map(String::as_str)
	}

	fn exec(&self) -> Option<&str> {
		self.fields.get("Exec").map(String::as_str)
	}

	fn try_exec(&self) -> Option<&str> {
		self.fields.get("TryExec").map(String::as_str)
	}
}

fn check_try_exec(entry: &DesktopEntry) {
	let Some(try_exec) = entry.try_exec().map(str::trim).filter(|value| !value.is_empty()) else {
		return;
	};

	match resolve_executable(try_exec) {
		Some(path) if is_executable(&path) => {},
		Some(path) => tracing::debug!(
			"Including desktop entry despite non-executable TryExec '{}', resolved to '{}'.",
			try_exec,
			path.display()
		),
		None => tracing::debug!("Including desktop entry despite unresolved TryExec '{}'.", try_exec),
	}
}

fn parse_exec_command(entry: &DesktopEntry, desktop_file: &Path) -> Option<Vec<String>> {
	let exec = entry.exec()?.trim();
	if exec.is_empty() {
		return None;
	}

	let tokens = shlex::split(exec).or_else(|| split_exec_fallback(exec))?;
	let mut command = Vec::new();

	for token in tokens {
		if token == "%i" {
			if let Some(icon) = entry.icon().map(str::trim).filter(|icon| !icon.is_empty()) {
				command.push("--icon".to_string());
				command.push(icon.to_string());
			}
			continue;
		}

		let expanded = expand_exec_token(&token, entry, desktop_file);
		if !expanded.is_empty() {
			command.push(expanded);
		}
	}

	if command.is_empty() {
		None
	} else {
		Some(command)
	}
}

fn expand_exec_token(token: &str, entry: &DesktopEntry, desktop_file: &Path) -> String {
	let mut expanded = String::new();
	let mut chars = token.chars();

	while let Some(character) = chars.next() {
		if character != '%' {
			expanded.push(character);
			continue;
		}

		let Some(field_code) = chars.next() else {
			break;
		};

		match field_code {
			'%' => expanded.push('%'),
			'c' => expanded.push_str(entry.name().as_deref().unwrap_or_default()),
			'k' => expanded.push_str(&desktop_file.to_string_lossy()),
			'i' => {
				if let Some(icon) = entry.icon().map(str::trim).filter(|icon| !icon.is_empty()) {
					expanded.push_str(icon);
				}
			},
			field_code if SKIPPED_EXEC_FIELD_CODES.contains(&field_code) => {},
			_ => {},
		}
	}

	expanded
}

fn split_exec_fallback(exec: &str) -> Option<Vec<String>> {
	let mut tokens = Vec::new();
	let mut current = String::new();
	let mut in_single_quotes = false;
	let mut in_double_quotes = false;
	let mut chars = exec.chars().peekable();

	while let Some(character) = chars.next() {
		match character {
			'\'' if !in_double_quotes => in_single_quotes = !in_single_quotes,
			'"' if !in_single_quotes => in_double_quotes = !in_double_quotes,
			'\\' if in_double_quotes || in_single_quotes => {
				if let Some(next) = chars.next() {
					current.push(next);
				}
			},
			character if character.is_whitespace() && !in_single_quotes && !in_double_quotes => {
				if !current.is_empty() {
					tokens.push(std::mem::take(&mut current));
				}
			},
			_ => current.push(character),
		}
	}

	if in_single_quotes || in_double_quotes {
		return None;
	}

	if !current.is_empty() {
		tokens.push(current);
	}

	Some(tokens)
}

fn unescape_desktop_value(value: &str) -> String {
	let mut unescaped = String::new();
	let mut chars = value.chars();

	while let Some(character) = chars.next() {
		if character != '\\' {
			unescaped.push(character);
			continue;
		}

		match chars.next() {
			Some('n') => unescaped.push('\n'),
			Some('t') => unescaped.push('\t'),
			Some('r') => unescaped.push('\r'),
			Some('s') => unescaped.push(' '),
			Some('\\') => unescaped.push('\\'),
			Some(other) => unescaped.push(other),
			None => break,
		}
	}

	unescaped
}

fn locale_preferences() -> Vec<String> {
	let mut locales = Vec::new();
	for key in ["LC_MESSAGES", "LC_ALL", "LANG"] {
		let Ok(locale) = env::var(key) else {
			continue;
		};
		let locale = locale.trim();
		if locale.is_empty() {
			continue;
		}

		let locale = locale
			.split('.')
			.next()
			.unwrap_or(locale)
			.split('@')
			.next()
			.unwrap_or(locale)
			.to_string();

		if !locale.is_empty() {
			locales.push(locale.clone());
			if let Some((language, _)) = locale.split_once('_') {
				locales.push(language.to_string());
			}
		}
	}

	locales.push("C".to_string());
	locales
}

fn resolve_executable(executable: &str) -> Option<PathBuf> {
	if executable.contains('/') {
		return expand_path(Path::new(executable));
	}

	let path = env::var_os("PATH")?;
	for directory in env::split_paths(&path) {
		let candidate = directory.join(executable);
		if candidate.exists() {
			return Some(candidate);
		}
	}

	None
}

fn expand_path(path: &Path) -> Option<PathBuf> {
	let path = path.to_string_lossy();
	let path = shellexpand::full(&path).ok()?;
	Some(PathBuf::from(path.as_ref()))
}

fn is_executable(path: &Path) -> bool {
	if !path.is_file() {
		return false;
	}

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;

		match fs::metadata(path) {
			Ok(metadata) => metadata.permissions().mode() & 0o111 != 0,
			Err(_) => false,
		}
	}

	#[cfg(not(unix))]
	{
		true
	}
}

pub(super) struct IconResolver {
	enabled: bool,
	cache: HashMap<String, Option<PathBuf>>,
	search_roots: Vec<PathBuf>,
}

impl IconResolver {
	pub(super) fn new(enabled: bool) -> Self {
		Self {
			enabled,
			cache: HashMap::new(),
			search_roots: icon_search_roots(),
		}
	}

	fn resolve(&mut self, icon: Option<&str>, desktop_file: &Path) -> Option<PathBuf> {
		if !self.enabled {
			return None;
		}

		let icon = icon?;
		if icon.is_empty() {
			return None;
		}

		if let Some(path) = self.resolve_icon_path(icon, desktop_file) {
			return Some(path);
		}

		if let Some(cached) = self.cache.get(icon) {
			return cached.clone();
		}

		let resolved = self.find_icon_by_name(icon);
		self.cache.insert(icon.to_string(), resolved.clone());
		resolved
	}

	fn resolve_icon_path(&self, icon: &str, desktop_file: &Path) -> Option<PathBuf> {
		let icon_path = Path::new(icon);
		if icon_path.is_absolute() {
			return is_supported_image(icon_path).then(|| icon_path.to_path_buf());
		}

		if icon.contains('/') {
			let path = desktop_file.parent()?.join(icon);
			return is_supported_image(&path).then_some(path);
		}

		None
	}

	pub(super) fn find_icon_by_name(&self, icon: &str) -> Option<PathBuf> {
		let icon_stem = Path::new(icon)
			.file_stem()
			.and_then(|stem| stem.to_str())
			.unwrap_or(icon);
		let icon_ext = Path::new(icon).extension().and_then(|extension| extension.to_str());
		let mut best_match = None;
		let mut best_score = i32::MIN;

		for root in &self.search_roots {
			if !root.exists() {
				continue;
			}

			for entry in WalkDir::new(root)
				.follow_links(true)
				.into_iter()
				.filter_map(|entry| entry.ok())
				.filter(|entry| entry.file_type().is_file())
			{
				let path = entry.path();
				if !is_supported_image(path) {
					continue;
				}

				let Some(file_stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
					continue;
				};

				if file_stem != icon_stem {
					continue;
				}

				if let Some(icon_ext) = icon_ext {
					let Some(candidate_ext) = path.extension().and_then(|extension| extension.to_str()) else {
						continue;
					};
					if !candidate_ext.eq_ignore_ascii_case(icon_ext) {
						continue;
					}
				}

				let score = score_icon_path(path);
				if score > best_score {
					best_score = score;
					best_match = Some(path.to_path_buf());
				}
			}
		}

		best_match
	}
}

fn icon_search_roots() -> Vec<PathBuf> {
	let mut roots = Vec::new();

	if let Some(data_home) = xdg_data_home() {
		roots.push(data_home.join("icons"));
		roots.push(data_home.join("pixmaps"));
	}

	for data_dir in xdg_data_dirs() {
		roots.push(data_dir.join("icons"));
		roots.push(data_dir.join("pixmaps"));
	}

	roots
}

fn xdg_data_home() -> Option<PathBuf> {
	if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
		return Some(PathBuf::from(data_home));
	}

	let home = env::var_os("HOME")?;
	Some(PathBuf::from(home).join(".local/share"))
}

fn xdg_data_dirs() -> Vec<PathBuf> {
	match env::var_os("XDG_DATA_DIRS") {
		Some(value) if !value.is_empty() => env::split_paths(&value).collect(),
		_ => vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")],
	}
}

fn is_supported_image(path: &Path) -> bool {
	path.is_file()
		&& path
			.extension()
			.and_then(|extension| extension.to_str())
			.is_some_and(|extension| {
				SUPPORTED_IMAGE_EXTENSIONS
					.iter()
					.any(|supported| supported.eq_ignore_ascii_case(extension))
			})
}

fn score_icon_path(path: &Path) -> i32 {
	let mut score = 0;
	let path_string = path.to_string_lossy();

	if path_string.contains("/hicolor/") {
		score += 50;
	}

	if path_string.contains("/apps/") {
		score += 25;
	}

	if path_string.contains("/scalable/") {
		score -= 5;
	}

	for component in path.components() {
		let component = component.as_os_str().to_string_lossy();
		let Some((width, height)) = component.split_once('x') else {
			continue;
		};

		let Ok(width) = width.parse::<i32>() else {
			continue;
		};
		let Ok(height) = height.parse::<i32>() else {
			continue;
		};

		score += std::cmp::min(width, height);
	}

	score
}

#[cfg(test)]
mod tests {
	use std::fs;
	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt;
	use std::sync::Mutex;

	use tempfile::tempdir;

	use super::*;

	fn write_file(path: &Path, contents: &str) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, contents).unwrap();
	}

	static ENV_MUTEX: Mutex<()> = Mutex::new(());

	fn scanner_config(directories: Vec<PathBuf>) -> DesktopApplicationScannerConfig {
		DesktopApplicationScannerConfig {
			directories,
			include_terminal: false,
			resolve_icons: true,
		}
	}

	#[test]
	fn parses_launchable_desktop_entry() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		let desktop_file = app_dir.join("moonshine-game.desktop");
		write_file(
			&desktop_file,
			r#"
[Desktop Entry]
Type=Application
Name=Moonshine Game
Exec=/usr/bin/example --flag %U %%done
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Moonshine Game");
		assert_eq!(applications[0].command, vec!["/usr/bin/example", "--flag", "%done"]);
	}

	#[test]
	fn filters_hidden_terminal_and_invalid_entries() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("hidden.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Hidden
Exec=/usr/bin/hidden
Hidden=true
"#,
		);
		write_file(
			&app_dir.join("terminal.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Terminal
Exec=/usr/bin/terminal
Terminal=true
"#,
		);
		write_file(
			&app_dir.join("link.desktop"),
			r#"
[Desktop Entry]
Type=Link
Name=Link
Exec=/usr/bin/link
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn includes_terminal_entries_when_configured() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("terminal.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Terminal
Exec=/usr/bin/terminal
Terminal=true
"#,
		);

		let mut config = scanner_config(vec![app_dir]);
		config.include_terminal = true;

		let applications = scan_desktop_applications(&config).unwrap();
		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Terminal");
	}

	#[test]
	fn includes_entries_with_failing_try_exec() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("broken.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Broken
Exec=/usr/bin/broken
TryExec=/definitely/missing
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Broken");
	}

	#[test]
	fn keeps_first_duplicate_entry() {
		let tempdir = tempdir().unwrap();
		let first_dir = tempdir.path().join("first");
		let second_dir = tempdir.path().join("second");
		let contents = r#"
[Desktop Entry]
Type=Application
Name=Duplicate
Exec=/usr/bin/duplicate --run
"#;
		write_file(&first_dir.join("duplicate.desktop"), contents);
		write_file(&second_dir.join("duplicate.desktop"), contents);

		let applications = scan_desktop_applications(&scanner_config(vec![first_dir, second_dir])).unwrap();
		assert_eq!(applications.len(), 1);
	}

	#[test]
	fn resolves_absolute_and_named_icons() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		let icon_root = tempdir.path().join("data/icons/hicolor/128x128/apps");
		let explicit_icon = tempdir.path().join("explicit.png");
		let named_icon = icon_root.join("moonshine.png");
		write_file(&explicit_icon, "png");
		write_file(&named_icon, "png");

		let _env_lock = ENV_MUTEX.lock().unwrap();
		let previous_data_home = env::var_os("XDG_DATA_HOME");
		env::set_var("XDG_DATA_HOME", tempdir.path().join("data"));

		write_file(
			&app_dir.join("explicit.desktop"),
			&format!(
				r#"
[Desktop Entry]
Type=Application
Name=Explicit Icon
Exec=/usr/bin/explicit
Icon={}
"#,
				explicit_icon.display()
			),
		);
		write_file(
			&app_dir.join("named.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Named Icon
Exec=/usr/bin/named
Icon=moonshine
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 2);
		let explicit_application = applications
			.iter()
			.find(|application| application.title == "Explicit Icon")
			.unwrap();
		assert_eq!(explicit_application.boxart.as_deref(), Some(explicit_icon.as_path()));

		let named_application = applications
			.iter()
			.find(|application| application.title == "Named Icon")
			.unwrap();
		assert_eq!(named_application.boxart.as_deref(), Some(named_icon.as_path()));

		match previous_data_home {
			Some(value) => env::set_var("XDG_DATA_HOME", value),
			None => env::remove_var("XDG_DATA_HOME"),
		}
	}

	#[test]
	fn try_exec_uses_path_lookup() {
		let tempdir = tempdir().unwrap();
		let bin_dir = tempdir.path().join("bin");
		let executable = bin_dir.join("moonshine-test");
		write_file(&executable, "#!/bin/sh\n");

		#[cfg(unix)]
		{
			let mut permissions = fs::metadata(&executable).unwrap().permissions();
			permissions.set_mode(0o755);
			fs::set_permissions(&executable, permissions).unwrap();
		}

		let _env_lock = ENV_MUTEX.lock().unwrap();
		let previous_path = env::var_os("PATH");
		env::set_var("PATH", &bin_dir);

		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("path.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Path Lookup
Exec=/usr/bin/ok
TryExec=moonshine-test
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 1);

		match previous_path {
			Some(value) => env::set_var("PATH", value),
			None => env::remove_var("PATH"),
		}
	}
}
