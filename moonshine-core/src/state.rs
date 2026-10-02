use std::collections::{HashMap, HashSet};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StateData {
	unique_id: String,
	#[serde(default)]
	clients: HashSet<String>,
	#[serde(default)]
	paired_certs: HashSet<String>,
	/// Explicit associations only; legacy sets cannot safely be zipped together.
	#[serde(default)]
	client_certs: HashMap<String, HashSet<String>>,
}

impl StateData {
	fn new() -> Self {
		Self {
			unique_id: uuid::Uuid::new_v4().to_string(),
			clients: Default::default(),
			paired_certs: Default::default(),
			client_certs: Default::default(),
		}
	}
}

#[derive(Clone)]
pub struct PersistentState {
	data: Arc<RwLock<StateData>>,
	path: PathBuf,
	available: Arc<AtomicBool>,
	_writer_lock: Option<Arc<std::fs::File>>,
}

impl PersistentState {
	#[cfg(test)]
	pub(crate) fn isolated(path: PathBuf) -> Self {
		Self {
			data: Arc::new(RwLock::new(StateData::new())),
			path,
			available: Arc::new(AtomicBool::new(true)),
			_writer_lock: None,
		}
	}

	pub(crate) fn new() -> Result<Self, ()> {
		let path = dirs::data_dir()
			.ok_or_else(|| tracing::error!("Failed to get data directory."))?
			.join("moonshine")
			.join("state.toml");

		crate::durable::create_directories(crate::durable::directory(&path))
			.map_err(|e| tracing::error!("Failed to create state directory: {e}"))?;
		let lock = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.mode(0o600)
			.custom_flags(libc::O_NOFOLLOW)
			.open(path.with_extension("lock"))
			.map_err(|e| tracing::error!("Failed to open state writer lock: {e}"))?;
		lock.try_lock()
			.map_err(|e| tracing::error!("Cannot acquire state writer lock (another service may be running): {e}"))?;
		let mut state = Self::load(path)?;
		state._writer_lock = Some(Arc::new(lock));
		Ok(state)
	}

	pub(crate) fn load(path: PathBuf) -> Result<Self, ()> {
		let exists = match std::fs::symlink_metadata(&path) {
			Ok(_) => true,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
			Err(e) => {
				tracing::error!("Failed to inspect state file: {e}");
				return Err(());
			},
		};
		let data = if exists {
			let serialized =
				std::fs::read_to_string(&path).map_err(|e| tracing::error!("Failed to read state file: {e}"))?;
			let data: StateData = toml::from_str(&serialized)
				.map_err(|e| tracing::error!("Failed to parse state file at '{}': {e}", path.display()))?;

			tracing::debug!("Successfully loaded state from {:?}", path);

			data
		} else {
			StateData::new()
		};

		let state = Self {
			data: Arc::new(RwLock::new(data)),
			path,
			available: Arc::new(AtomicBool::new(true)),
			_writer_lock: None,
		};
		state.save()?;

		Ok(state)
	}

	pub fn get_uuid(&self) -> Result<String, ()> {
		let data = self
			.data
			.read()
			.map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
		Ok(data.unique_id.clone())
	}

	fn persist(&self, data: &StateData) -> Result<(), ()> {
		let serialized = toml::to_string_pretty(data).map_err(|e| tracing::error!("Failed to serialize state: {e}"))?;
		crate::durable::replace(&self.path, serialized.as_bytes()).map_err(|e| {
			// A directory sync failure after rename has an uncertain durable outcome.
			// Refuse authorization until restart/recovery instead of using stale trust.
			self.available.store(false, Ordering::Release);
			tracing::error!("Failed to commit state; authorization disabled until restart: {e}");
		})
	}

	pub(crate) fn save(&self) -> Result<(), ()> {
		let data = self.data.write().map_err(|_| ())?;
		self.persist(&data)
	}

	fn transaction(&self, mutate: impl FnOnce(&mut StateData) -> Result<bool, ()>) -> Result<bool, ()> {
		let mut data = self.data.write().map_err(|_| ())?;
		if !self.available.load(Ordering::Acquire) {
			return Err(());
		}
		let mut next = data.clone();
		let changed = mutate(&mut next)?;
		if changed {
			self.persist(&next)?;
			*data = next;
		}
		Ok(changed)
	}

	pub(crate) fn has_client(&self, client: String) -> Result<bool, ()> {
		let data = self.data.read().map_err(|_| ())?;
		if !self.available.load(Ordering::Acquire) {
			return Err(());
		}
		Ok(data.clients.contains(&client))
	}

	pub(crate) fn has_paired_cert(&self, fingerprint: String) -> Result<bool, ()> {
		let data = self.data.read().map_err(|_| ())?;
		if !self.available.load(Ordering::Acquire) {
			return Err(());
		}
		Ok(data.paired_certs.contains(&fingerprint))
	}

	pub(crate) fn pair(&self, id: String, fingerprint: String) -> Result<bool, ()> {
		self.transaction(|data| {
			let client_added = data.clients.insert(id.clone());
			let cert_added = data.paired_certs.insert(fingerprint.clone());
			let association_added = data.client_certs.entry(id).or_default().insert(fingerprint);
			Ok(client_added || cert_added || association_added)
		})
	}

	/// Revoke a credential and every known alias for it. Legacy IDs are retained
	/// when their certificate association is unknown, but cannot authorize TLS.
	pub(crate) fn revoke(&self, id: Option<&str>, fingerprint: Option<&str>) -> Result<bool, ()> {
		self.transaction(|data| {
			let targets = if let Some(fp) = fingerprint {
				HashSet::from([fp.to_string()])
			} else if let Some(id) = id {
				data.client_certs.get(id).cloned().ok_or_else(|| {
					tracing::warn!("Client has no known certificate association; revoke by fingerprint");
				})?
			} else {
				return Err(());
			};
			let mut changed = false;
			for fp in &targets {
				changed |= data.paired_certs.remove(fp);
			}
			data.client_certs.retain(|client, certs| {
				certs.retain(|fp| !targets.contains(fp));
				if certs.is_empty() {
					data.clients.remove(client);
					false
				} else {
					true
				}
			});
			Ok(changed)
		})
	}

	#[cfg(test)]
	pub(crate) fn add_paired_cert(&self, fingerprint: String) -> Result<bool, ()> {
		self.transaction(|data| Ok(data.paired_certs.insert(fingerprint)))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn interrupted_commit_child_probe() {
		let Some(path) = std::env::var_os("PYROSHINE_STATE_CRASH_PATH") else {
			return;
		};
		let state = PersistentState::load(path.into()).unwrap();
		let operation = std::env::var("PYROSHINE_STATE_CRASH_OPERATION").unwrap();
		crate::durable::fail_next(if operation == "rename" { "rename" } else { "dir_sync" }, 0);
		let _ = state.pair("A".into(), "cert-A".into());
		panic!("child did not reach interruption point");
	}

	#[test]
	fn process_exit_during_commit_leaves_complete_old_or_new_state() {
		for operation in ["rename", "dir_sync"] {
			let directory = tempfile::tempdir().unwrap();
			let path = directory.path().join("state.toml");
			let state = PersistentState::load(path.clone()).unwrap();
			state.pair("B".into(), "cert-B".into()).unwrap();
			let uuid = state.get_uuid().unwrap();
			let result = std::process::Command::new(std::env::current_exe().unwrap())
				.args(["--exact", "state::tests::interrupted_commit_child_probe"])
				.env("PYROSHINE_STATE_CRASH_PATH", &path)
				.env("PYROSHINE_STATE_CRASH_OPERATION", operation)
				.output()
				.unwrap();
			assert_eq!(result.status.code(), Some(91));
			let restarted = PersistentState::load(path).unwrap();
			assert_eq!(restarted.get_uuid().unwrap(), uuid);
			assert!(restarted.has_paired_cert("cert-B".into()).unwrap());
			assert_eq!(
				restarted.has_paired_cert("cert-A".into()).unwrap(),
				operation == "dir_sync"
			);
			assert_eq!(restarted.has_client("A".into()).unwrap(), operation == "dir_sync");
		}
	}

	#[test]
	fn migration_revocation_aliases_and_restart_preserve_uuid_and_other_clients() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("state.toml");
		let uuid = "d6fbb8e9-35b8-4c82-9f2d-ea68c2ef8d12";
		std::fs::write(
			&path,
			format!("unique_id = '{uuid}'\nclients = ['legacy']\npaired_certs = ['legacy-cert']\n"),
		)
		.unwrap();
		let state = PersistentState::load(path.clone()).unwrap();
		assert_eq!(state.get_uuid().unwrap(), uuid);
		assert!(state.has_client("legacy".into()).unwrap());
		assert!(state.has_paired_cert("legacy-cert".into()).unwrap());
		assert!(state.revoke(Some("legacy"), None).is_err());
		state.pair("A".into(), "cert-A".into()).unwrap();
		state.pair("alias-A".into(), "cert-A".into()).unwrap();
		state.pair("B".into(), "cert-B".into()).unwrap();
		assert!(state.revoke(Some("A"), None).unwrap());
		let restarted = PersistentState::load(path).unwrap();
		assert_eq!(restarted.get_uuid().unwrap(), uuid);
		assert!(!restarted.has_client("A".into()).unwrap());
		assert!(!restarted.has_client("alias-A".into()).unwrap());
		assert!(!restarted.has_paired_cert("cert-A".into()).unwrap());
		assert!(restarted.has_paired_cert("cert-B".into()).unwrap());
		assert!(restarted.has_paired_cert("legacy-cert".into()).unwrap());
		assert!(restarted.revoke(None, Some("legacy-cert")).unwrap());
		assert!(restarted.has_client("legacy".into()).unwrap());
	}

	#[test]
	fn failed_commit_never_publishes_uncommitted_trust() {
		for operation in ["write", "file_sync", "rename", "dir_sync"] {
			let dir = tempfile::tempdir().unwrap();
			let path = dir.path().join("state.toml");
			let state = PersistentState::load(path.clone()).unwrap();
			state.pair("B".into(), "cert-B".into()).unwrap();
			let uuid = state.get_uuid().unwrap();
			crate::durable::fail_next(operation, libc::ENOSPC);
			assert!(state.pair("A".into(), "cert-A".into()).is_err());
			assert!(!state.data.read().unwrap().clients.contains("A"));
			assert!(state.has_paired_cert("cert-A".into()).is_err());
			let restarted = PersistentState::load(path).unwrap();
			assert_eq!(restarted.get_uuid().unwrap(), uuid);
			assert!(restarted.has_paired_cert("cert-B".into()).unwrap());
			assert_eq!(
				restarted.has_paired_cert("cert-A".into()).unwrap(),
				operation == "dir_sync"
			);
		}
	}

	#[test]
	fn concurrent_pairing_commits_keep_every_association() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("state.toml");
		let state = PersistentState::load(path.clone()).unwrap();
		let uuid = state.get_uuid().unwrap();
		std::thread::scope(|scope| {
			for index in 0..32 {
				let state = &state;
				scope.spawn(move || state.pair(format!("client-{index}"), format!("cert-{index}")).unwrap());
			}
		});
		let restarted = PersistentState::load(path).unwrap();
		assert_eq!(restarted.get_uuid().unwrap(), uuid);
		let data = restarted.data.read().unwrap();
		assert_eq!(data.clients.len(), 32);
		assert_eq!(data.paired_certs.len(), 32);
		assert_eq!(data.client_certs.len(), 32);
	}
}
