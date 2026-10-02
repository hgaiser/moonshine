//! Infrequent administrative writes: complete, private, synced staging files.
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

pub(crate) fn directory(path: &Path) -> &Path {
	path.parent()
		.filter(|parent| !parent.as_os_str().is_empty())
		.unwrap_or_else(|| Path::new("."))
}

pub(crate) fn create_directories(directory: &Path) -> io::Result<()> {
	let mut missing = Vec::new();
	let mut cursor = directory;
	while !cursor.exists() && !cursor.as_os_str().is_empty() {
		missing.push(cursor);
		let Some(parent) = cursor.parent() else {
			break;
		};
		cursor = parent;
	}
	std::fs::create_dir_all(directory)?;
	// Sync newly created directory entries as well as the eventual file entry.
	for path in missing.into_iter().rev() {
		File::open(path)?.sync_all()?;
		File::open(self::directory(path))?.sync_all()?;
	}
	Ok(())
}

pub(crate) fn stage(path: &Path, bytes: &[u8]) -> io::Result<tempfile::NamedTempFile> {
	let parent = directory(path);
	create_directories(parent)?;
	// tempfile uses create-new and mode 0600 on Unix, independently of umask.
	let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
	if let Err(error) = checkpoint("write") {
		// Fault injection exercises a genuinely partial staging write.
		temporary.write_all(&bytes[..bytes.len() / 2])?;
		return Err(error);
	}
	temporary.write_all(bytes)?;
	checkpoint("file_sync")?;
	temporary.as_file().sync_all()?;
	Ok(temporary)
}

pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
	checkpoint("dir_sync")?;
	File::open(directory(path))?.sync_all()
}

pub(crate) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
	let temporary = stage(path, bytes)?;
	checkpoint("rename")?;
	temporary.persist(path).map_err(|e| e.error)?;
	sync_parent(path)
}

pub(crate) fn create(path: &Path, bytes: &[u8]) -> io::Result<()> {
	let temporary = stage(path, bytes)?;
	checkpoint("create")?;
	temporary.persist_noclobber(path).map_err(|e| e.error)?;
	sync_parent(path)
}

#[cfg(not(test))]
fn checkpoint(_: &str) -> io::Result<()> {
	Ok(())
}

#[cfg(test)]
thread_local! {
	static FAILURE: std::cell::RefCell<Option<(&'static str, usize, i32)>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn fail_next(operation: &'static str, errno: i32) {
	FAILURE.with(|failure| *failure.borrow_mut() = Some((operation, 0, errno)));
}
#[cfg(test)]
pub(crate) fn fail_after(operation: &'static str, successful_calls: usize, errno: i32) {
	FAILURE.with(|failure| *failure.borrow_mut() = Some((operation, successful_calls, errno)));
}
#[cfg(test)]
fn checkpoint(operation: &str) -> io::Result<()> {
	FAILURE.with(|failure| {
		let mut failure = failure.borrow_mut();
		if failure.is_some_and(|(expected, _, _)| expected == operation) {
			if let Some((_, remaining, _)) = failure.as_mut()
				&& *remaining > 0
			{
				*remaining -= 1;
				return Ok(());
			}
			let (_, _, errno) = failure.take().unwrap();
			if errno == 0 {
				std::process::exit(91);
			}
			Err(io::Error::from_raw_os_error(errno))
		} else {
			Ok(())
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::PermissionsExt;

	#[test]
	fn staged_file_is_private_and_old_file_survives_failed_replacement() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("state");
		replace(&path, b"old complete state").unwrap();
		for (operation, errno) in [
			("write", libc::ENOSPC),
			("file_sync", libc::EIO),
			("rename", libc::EACCES),
		] {
			fail_next(operation, errno);
			assert!(replace(&path, b"new complete state").is_err());
			assert_eq!(std::fs::read(&path).unwrap(), b"old complete state");
			assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
		}
		let staged = stage(&path, b"new complete state").unwrap();
		assert_eq!(staged.as_file().metadata().unwrap().permissions().mode() & 0o777, 0o600);
		// An interrupted update before publication leaves the old file intact.
		drop(staged);
		assert_eq!(std::fs::read(&path).unwrap(), b"old complete state");
		fail_next("dir_sync", libc::EIO);
		assert!(replace(&path, b"new complete state").is_err());
		assert_eq!(std::fs::read(&path).unwrap(), b"new complete state");
	}
}
