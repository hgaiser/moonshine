use std::{collections::HashSet, path::PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LutrisApplicationScannerConfig {
	/// Path to the Lutris database (ie. `~/.local/share/lutris/pga.db`).
	#[serde(default = "default_pga_db")]
	pub pga_db: PathBuf,

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

fn default_pga_db() -> PathBuf {
	dirs::data_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("lutris")
		.join("pga.db")
}

fn is_valid_slug(slug: &str) -> bool {
	slug.chars()
		.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub(crate) fn scan_lutris_applications(config: &LutrisApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
	let db_path = &config.pga_db;

	let binding = db_path.to_string_lossy();
	let expanded = shellexpand::full(&binding)
		.map_err(|e| tracing::warn!("Failed to expand Lutris database path {:?}: {e}", db_path))?;

	let db_path = PathBuf::from(expanded.as_ref());

	if !db_path.exists() {
		tracing::debug!("Lutris database not found at {:?}.", db_path);
		return Ok(Vec::new());
	}

	let conn = Connection::open(&db_path)
		.map_err(|e| tracing::warn!("Failed to open Lutris database at {:?}: {e}", db_path))?;

	conn.busy_timeout(std::time::Duration::from_secs(5))
		.map_err(|e| tracing::warn!("Failed to set busy timeout: {e}"))?;

	let mut stmt = conn
		.prepare("SELECT id, name, slug, directory FROM games WHERE installed = 1")
		.map_err(|e| tracing::warn!("Failed to prepare query: {e}"))?;

	let games: Vec<(i64, String, String, Option<String>)> = stmt
		.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
		.map_err(|e| tracing::warn!("Failed to query games: {e}"))?
		.filter_map(|r| r.ok())
		.collect();

	if games.is_empty() {
		tracing::debug!("No installed games found in Lutris database.");
		return Ok(Vec::new());
	}

	let coverart_dir = dirs::data_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("lutris")
		.join("coverart");

	let coverart_set = if coverart_dir.exists() {
		std::fs::read_dir(&coverart_dir)
			.ok()
			.map(|entries| {
				entries
					.filter_map(|e| e.ok())
					.map(|e| e.file_name().to_string_lossy().to_string())
					.collect::<HashSet<_>>()
			})
			.unwrap_or_default()
	} else {
		HashSet::new()
	};

	let mut applications = Vec::new();
	let mut skipped = 0;

	for (_id, name, slug, _directory) in games {
		if !is_valid_slug(&slug) {
			tracing::debug!("Skipping game '{}' with invalid slug '{}'.", name, slug);
			skipped += 1;
			continue;
		}

		let mut boxart_path = None;
		for ext in &["jpg", "png"] {
			let art_name = format!("{}.{}", slug, ext);
			if coverart_set.contains(&art_name) {
				boxart_path = Some(coverart_dir.join(art_name));
				break;
			}
		}

		let application = ApplicationConfig {
			title: name,
			pre_command: config.pre_command.clone(),
			post_command: config.post_command.clone(),
			command: config.command.iter().map(|cmd| cmd.replace("{slug}", &slug)).collect(),
			boxart: boxart_path,
			stdout: config.stdout.clone(),
			stderr: config.stderr.clone(),
			launch_timeout_secs: config.launch_timeout_secs,
		};

		applications.push(application);
	}

	tracing::debug!("Scanned {} Lutris games ({} skipped).", applications.len(), skipped);

	Ok(applications)
}
