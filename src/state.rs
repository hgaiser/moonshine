use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StateData {
	unique_id: String,
	#[serde(default)]
	clients: HashSet<String>,
	#[serde(default)]
	paired_certs: HashSet<String>,
}

impl StateData {
	fn new() -> Self {
		Self {
			unique_id: uuid::Uuid::new_v4().to_string(),
			clients: Default::default(),
			paired_certs: Default::default(),
		}
	}
}

#[derive(Clone)]
pub struct State {
	data: Arc<RwLock<StateData>>,
	path: PathBuf,
}

impl State {
	pub fn new() -> Result<Self, ()> {
		let path = dirs::data_dir()
			.ok_or_else(|| tracing::error!("Failed to get data directory."))?
			.join("moonshine")
			.join("state.toml");

		let data = if path.exists() {
			let serialized =
				std::fs::read_to_string(&path).map_err(|e| tracing::error!("Failed to read state file: {e}"))?;
			let data: StateData = toml::from_str(&serialized)
				.map_err(|e| tracing::error!("Failed to parse state file at '{}': {e}", path.display()))?;

			tracing::debug!("Successfully loaded state from {:?}", path);
			tracing::trace!("State: {data:?}");

			data
		} else {
			StateData::new()
		};

		let state = Self {
			data: Arc::new(RwLock::new(data)),
			path,
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

	pub fn save(&self) -> Result<(), ()> {
		let data = self
			.data
			.read()
			.map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
		let parent_dir = self
			.path
			.parent()
			.ok_or_else(|| tracing::warn!("Failed to get state dir for file {:?}", self.path))?;
		std::fs::create_dir_all(parent_dir).map_err(|e| tracing::warn!("Failed to create state dir: {e}"))?;

		std::fs::write(
			&self.path,
			toml::to_string_pretty(&*data).map_err(|e| tracing::warn!("Failed to serialize state: {e}"))?,
		)
		.map_err(|e| tracing::warn!("Failed to save state file: {e}"))
	}

	pub fn has_client(&self, client: String) -> Result<bool, ()> {
		let data = self
			.data
			.read()
			.map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
		Ok(data.clients.contains(&client))
	}

	pub fn add_client(&self, client: String) -> Result<bool, ()> {
		let mut data = self
			.data
			.write()
			.map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
		if data.clients.contains(&client) {
			tracing::warn!("Failed to add client ('{client}'), client already exists.");
			Ok(false)
		} else {
			data.clients.insert(client);
			self.save()?;
			Ok(true)
		}
	}

	pub fn has_paired_cert(&self, fingerprint: String) -> Result<bool, ()> {
		let data = self
			.data
			.read()
			.map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
		Ok(data.paired_certs.contains(&fingerprint))
	}

	pub fn add_paired_cert(&self, fingerprint: String) -> Result<bool, ()> {
		let mut data = self
			.data
			.write()
			.map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
		let inserted = data.paired_certs.insert(fingerprint);
		if inserted {
			self.save()?;
		} else {
			tracing::warn!("Failed to add paired cert, already exists.");
		}
		Ok(inserted)
	}

	// pub fn remove_client(&self, client: String) -> Result<bool, ()> {
	// 	let mut data = self.data.write().map_err(|poison| tracing::error!("RwLock poisoned: {poison}"))?;
	// 	if !data.clients.contains(&client) {
	// 		tracing::error!("Failed to remove client ('{client}'), client doesn't exist.");
	// 		return Ok(false);
	// 	}
	// 	data.clients.shift_remove(&client);
	// 	self.save()?;
	// 	Ok(true)
	// }
}
