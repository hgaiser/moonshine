use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

enum StateCommand {
	GetUuid(oneshot::Sender<String>),
	Save(PathBuf, oneshot::Sender<Result<(), ()>>),
	HasClient(String, oneshot::Sender<bool>),
	AddClient(String),
	// RemoveClient(String, oneshot::Sender<bool>),
}

#[derive(Clone)]
pub struct State {
	command_tx: mpsc::Sender<StateCommand>,
	path: PathBuf,
}

impl State {
	pub async fn new() -> Result<Self, ()> {
		let path = dirs::data_dir()
			.ok_or_else(|| tracing::error!("Failed to get data directory."))?
			.join("moonshine")
			.join("state.toml");

		let (command_tx, command_rx) = mpsc::channel(10);

		let inner: StateInner;
		if path.exists() {
			let serialized =
				std::fs::read_to_string(&path).map_err(|e| tracing::error!("Failed to read state file: {e}"))?;
			inner = toml::from_str(&serialized)
				.map_err(|e| tracing::error!("Failed to parse state file at '{}': {e}", path.display()))?;

			tracing::debug!("Successfully loaded state from {:?}", path);
			tracing::trace!("State: {inner:?}");

			tokio::spawn(inner.run(command_rx));
		} else {
			let inner = StateInner::new();
			tokio::spawn(inner.run(command_rx));
		}

		let state = Self { command_tx, path };
		state.save().await?;

		Ok(state)
	}

	pub async fn get_uuid(&self) -> Result<String, ()> {
		let (uuid_tx, uuid_rx) = oneshot::channel();
		self.command_tx
			.send(StateCommand::GetUuid(uuid_tx))
			.await
			.map_err(|e| tracing::error!("Failed to send GetUuid command: {e}"))?;
		uuid_rx
			.await
			.map_err(|e| tracing::error!("Failed to receive GetUuid response: {e}"))
	}

	pub async fn save(&self) -> Result<(), ()> {
		let (result_tx, result_rx) = oneshot::channel();
		self.command_tx
			.send(StateCommand::Save(self.path.clone(), result_tx))
			.await
			.map_err(|e| tracing::error!("Failed to send Save command: {e}"))?;
		result_rx
			.await
			.map_err(|e| tracing::error!("Failed to receive Save response: {e}"))?
	}

	pub async fn has_client(&self, client: String) -> Result<bool, ()> {
		let (result_tx, result_rx) = oneshot::channel();
		self.command_tx
			.send(StateCommand::HasClient(client, result_tx))
			.await
			.map_err(|e| tracing::error!("Failed to send HasClient command: {e}"))?;
		let result = result_rx
			.await
			.map_err(|e| tracing::error!("Failed to receive HasClient response: {e}"))?;

		self.save().await?;

		Ok(result)
	}

	pub async fn add_client(&self, client: String) -> Result<(), ()> {
		self.command_tx
			.send(StateCommand::AddClient(client))
			.await
			.map_err(|e| tracing::error!("Failed to send AddClient command: {e}"))
	}

	// pub async fn remove_client(&self, client: String) -> Result<bool, ()> {
	// 	let (result_tx, result_rx) = oneshot::channel();
	// 	self.command_tx.send(StateCommand::RemoveClient(client, result_tx)).await
	// 		.map_err(|e| tracing::error!("Failed to send RemoveClient command: {e}"))?;
	// 	let result = result_rx.await.map_err(|e| tracing::error!("Failed to receive RemoveClient response: {e}"))?;

	// 	self.save().await?;

	// 	Ok(result)
	// }
}

#[derive(Debug, Serialize, Deserialize)]
struct StateInner {
	unique_id: String,
	clients: Vec<String>,
}

impl StateInner {
	fn new() -> Self {
		Self {
			unique_id: uuid::Uuid::new_v4().to_string(),
			clients: Default::default(),
		}
	}

	async fn run(mut self, mut command_rx: mpsc::Receiver<StateCommand>) {
		while let Some(command) = command_rx.recv().await {
			match command {
				StateCommand::GetUuid(uuid_tx) => {
					if uuid_tx.send(self.unique_id.clone()).is_err() {
						tracing::error!("Failed to send GetUuid result.");
					}
				},

				StateCommand::Save(file, result_tx) => {
					let result = self.save(&file);
					if result_tx.send(result).is_err() {
						tracing::error!("Failed to send Save result.");
					}
				},

				StateCommand::HasClient(client, result_tx) => {
					if result_tx.send(self.has_client(&client)).is_err() {
						tracing::error!("Failed to send HasClient result.");
					}
				},

				StateCommand::AddClient(client) => {
					// TODO: Return error to caller.
					let _ = self.add_client(client);
				},
				// StateCommand::RemoveClient(client, result_tx) => {
				// 	if result_tx.send(self.remove_client(client)).is_err() {
				// 		tracing::error!("Failed to send RemoveClient result.");
				// 	}
				// },
			}
		}
	}

	pub fn save<P: AsRef<Path>>(&self, file: P) -> Result<(), ()> {
		let parent_dir = file
			.as_ref()
			.parent()
			.ok_or_else(|| tracing::error!("Failed to get state dir for file {:?}", file.as_ref()))?;
		std::fs::create_dir_all(parent_dir).map_err(|e| tracing::error!("Failed to create state dir: {e}"))?;

		std::fs::write(
			file,
			toml::to_string_pretty(self).map_err(|e| tracing::error!("Failed to serialize state: {e}"))?,
		)
		.map_err(|e| tracing::error!("Failed to save state file: {e}"))
	}

	fn has_client(&self, key: &String) -> bool {
		self.clients.contains(key)
	}

	fn add_client(&mut self, key: String) -> bool {
		if self.clients.contains(&key) {
			tracing::error!("Failed to add client ('{key}'), client already exists.");
			false
		} else {
			self.clients.push(key);
			true
		}
	}

	// fn remove_client(&mut self, key: String) -> bool {
	// 	if !self.clients.contains(&key) {
	// 		tracing::error!("Failed to remove client ('{key}'), client doesn't exist.");
	// 		false
	// 	} else {
	// 		self.clients.retain(|c| c != &key);
	// 		true
	// 	}
	// }
}
