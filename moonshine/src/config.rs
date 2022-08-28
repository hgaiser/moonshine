use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
	pub name: String,
	pub address: String,
	pub port: u16,

	pub tls: Tls,
}

#[derive(Clone, Debug)]
pub struct Tls {
	pub port: u16,
	pub certificate_chain: PathBuf,
	pub private_key: PathBuf,
}
