use std::path::PathBuf;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
	pub name: String,
	pub address: String,
	pub port: u16,

	pub tls: Tls,

	pub applications: Vec<Application>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Tls {
	pub port: u16,
	pub certificate_chain: PathBuf,
	pub private_key: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Application {
	pub title: String,
}
