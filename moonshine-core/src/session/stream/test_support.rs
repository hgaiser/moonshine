//! Test-only helpers shared by the media stream tests.

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

/// A socket identified by its kernel inode.
///
/// Release tests must prove a stopped handler closed its socket. Re-binding
/// the freed ephemeral port cannot prove that: a concurrently running test
/// may take the port first. A UDP port is free exactly when the last
/// descriptor of its socket closes, so this checks the descriptors instead.
pub(crate) struct SocketId(u64);

impl SocketId {
	pub(crate) fn of(socket: &impl AsRawFd) -> Self {
		let metadata = std::fs::metadata(format!("/proc/self/fd/{}", socket.as_raw_fd()))
			.expect("socket descriptor is visible in /proc/self/fd");
		Self(metadata.ino())
	}

	/// Whether any descriptor in this process still refers to the socket.
	pub(crate) fn is_open(&self) -> bool {
		let target = format!("socket:[{}]", self.0);
		std::fs::read_dir("/proc/self/fd")
			.expect("/proc/self/fd is readable")
			.filter_map(Result::ok)
			.filter_map(|entry| std::fs::read_link(entry.path()).ok())
			.any(|link| link.as_os_str() == target.as_str())
	}
}

#[cfg(test)]
mod tests {
	use super::SocketId;

	#[test]
	fn socket_stays_open_until_its_last_descriptor_closes() {
		let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
		let id = SocketId::of(&socket);
		assert!(id.is_open());
		// A duplicated descriptor keeps the port bound after the original drops.
		let duplicate = socket.try_clone().unwrap();
		drop(socket);
		assert!(id.is_open());
		drop(duplicate);
		assert!(!id.is_open());
	}
}
