//! Explicit ownership of the session's XWayland child process.
//!
//! Smithay's `XWayland::spawn` keeps the `std::process::Child` private and
//! only promises that the server exits "once the connections to it are
//! closed". It never signals the child, and it reaps it on a detached thread
//! started when the Wayland client disconnects. That is not a lifecycle
//! guarantee the session can report completion on: if anything keeps the
//! Wayland client or the WM's X11 connection open, XWayland, its display lock
//! and its listening sockets outlive the session that reported `Idle`.
//!
//! [`OwnedChild`] identifies the process Smithay just forked from the current
//! thread and holds a pidfd for it. A pidfd names exactly that process (it is
//! immune to PID reuse), so teardown can wait for its exit with a bound and
//! escalate to `SIGKILL` without any risk of signalling an unrelated host
//! XWayland. Reaping stays with Smithay's thread; a pidfd is readable as soon
//! as the process has exited, zombie or not.

use std::collections::BTreeSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// Bound for a just-spawned child to take its executable's name.
const EXEC_NAME_SETTLE: Duration = Duration::from_millis(100);

/// Direct children forked by the calling thread.
///
/// Linux lists children per creating task in `/proc/thread-self/children`;
/// `Command::spawn` forks from the calling thread, so a snapshot before and
/// after a spawn isolates the new child even when other threads fork.
pub(super) fn thread_children() -> io::Result<BTreeSet<i32>> {
	let children = std::fs::read_to_string("/proc/thread-self/children")?;
	Ok(children.split_whitespace().filter_map(|pid| pid.parse().ok()).collect())
}

/// One verified direct child of this process, named by a pidfd.
#[derive(Debug)]
pub(super) struct OwnedChild {
	pid: i32,
	pidfd: OwnedFd,
}

/// How a bounded termination ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Termination {
	/// The process exited on its own within the grace period.
	Exited,
	/// The process ignored the grace period and was killed.
	Killed,
}

impl OwnedChild {
	/// Adopt the single child named `comm` that the calling thread forked
	/// since `before` was captured.
	///
	/// The candidate must be a direct child of this process with the expected
	/// command name; anything else (including zero or several candidates) is
	/// refused rather than guessed.
	pub(super) fn adopt_new_child(before: &BTreeSet<i32>, comm: &str) -> io::Result<Self> {
		// `Command::spawn` returns once the child's exec has released the
		// parent, but the kernel renames the child to the new executable only
		// after that release. Until then it still carries this thread's name,
		// so give a new child a short, bounded time to finish exec.
		let deadline = Instant::now() + EXEC_NAME_SETTLE;
		let pid = loop {
			let after = thread_children()?;
			let mut new = after.difference(before).copied().peekable();
			let has_new_child = new.peek().is_some();
			let mut candidates = new.filter(|&pid| is_direct_child(pid, comm));
			match (candidates.next(), candidates.next()) {
				(Some(pid), None) => break pid,
				(None, _) if has_new_child && Instant::now() < deadline => {
					std::thread::sleep(Duration::from_micros(200));
				},
				_ => {
					return Err(io::Error::new(
						io::ErrorKind::NotFound,
						format!("expected exactly one new `{comm}` child of this thread"),
					));
				},
			}
		};
		// SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
		let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
		if fd < 0 {
			return Err(io::Error::last_os_error());
		}
		// SAFETY: a non-negative pidfd_open result is a new fd owned by us.
		let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
		// An unreaped child's pid cannot be reused, and only Smithay reaps it
		// after the Wayland client disconnects; re-check anyway so the pidfd is
		// known to name the verified process.
		if !is_direct_child(pid, comm) {
			return Err(io::Error::new(
				io::ErrorKind::NotFound,
				"child changed while opening pidfd",
			));
		}
		Ok(Self { pid, pidfd })
	}

	pub(super) fn pid(&self) -> i32 {
		self.pid
	}

	/// Wait up to `timeout` for the process to exit. Returns `true` once it
	/// has exited (including as a not-yet-reaped zombie).
	pub(super) fn wait_exit(&self, timeout: Duration) -> io::Result<bool> {
		let deadline = Instant::now() + timeout;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			let mut poll = libc::pollfd {
				fd: self.pidfd.as_raw_fd(),
				events: libc::POLLIN,
				revents: 0,
			};
			let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
			// SAFETY: one valid pollfd for the duration of the call.
			match unsafe { libc::poll(&mut poll, 1, millis) } {
				n if n > 0 => return Ok(true),
				0 => return Ok(false),
				_ => {
					let error = io::Error::last_os_error();
					if error.kind() != io::ErrorKind::Interrupted {
						return Err(error);
					}
				},
			}
		}
	}

	/// Send `SIGKILL` through the pidfd. Signalling an already exited
	/// process is not an error.
	pub(super) fn kill(&self) -> io::Result<()> {
		// SAFETY: pidfd_send_signal with a valid pidfd, no siginfo and no flags.
		let result = unsafe {
			libc::syscall(
				libc::SYS_pidfd_send_signal,
				self.pidfd.as_raw_fd(),
				libc::SIGKILL,
				std::ptr::null::<libc::siginfo_t>(),
				0,
			)
		};
		if result == 0 {
			return Ok(());
		}
		let error = io::Error::last_os_error();
		if error.raw_os_error() == Some(libc::ESRCH) {
			Ok(())
		} else {
			Err(error)
		}
	}

	/// Wait for a graceful exit, then kill and wait again. `Err` means the
	/// process is still alive (or its state cannot be observed) after both
	/// bounds; the caller must not report it gone.
	pub(super) fn terminate(&self, grace: Duration, after_kill: Duration) -> io::Result<Termination> {
		if self.wait_exit(grace)? {
			return Ok(Termination::Exited);
		}
		self.kill()?;
		if self.wait_exit(after_kill)? {
			Ok(Termination::Killed)
		} else {
			Err(io::Error::new(
				io::ErrorKind::TimedOut,
				"process still alive after SIGKILL",
			))
		}
	}
}

/// Whether `pid` is a live-or-zombie direct child of this process named `comm`.
fn is_direct_child(pid: i32, comm: &str) -> bool {
	let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
		return false;
	};
	parse_stat(&stat).is_some_and(|(name, ppid)| name == comm && ppid == std::process::id() as i32)
}

/// Parse `comm` and `ppid` from `/proc/<pid>/stat`. `comm` may contain spaces
/// and parentheses, so it spans the first `(` to the last `)`.
fn parse_stat(stat: &str) -> Option<(&str, i32)> {
	let open = stat.find('(')?;
	let close = stat.rfind(')')?;
	let comm = stat.get(open + 1..close)?;
	let mut fields = stat.get(close + 1..)?.split_whitespace();
	let _state = fields.next()?;
	Some((comm, fields.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::{Command, Stdio};

	const HELPER: &str = "session::compositor::xwayland_process::tests::child_process_helper";
	const HELPER_SLEEP: &str = "MOONSHINE_TEST_CHILD_SLEEP_MS";

	/// Spawned as a child by the tests below. Re-executing the test binary by
	/// absolute path keeps them independent of `PATH`, which other tests in
	/// this process temporarily replace.
	#[test]
	#[ignore = "child process for the ownership tests"]
	fn child_process_helper() {
		if let Some(ms) = std::env::var(HELPER_SLEEP).ok().and_then(|ms| ms.parse().ok()) {
			std::thread::sleep(Duration::from_millis(ms));
		}
	}

	fn helper(sleep_ms: u64) -> Command {
		let mut command = Command::new(std::env::current_exe().unwrap());
		command
			.args(["--exact", HELPER, "--ignored", "--test-threads=1", "--quiet"])
			.env(HELPER_SLEEP, sleep_ms.to_string())
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null());
		command
	}

	/// The kernel command name of the helper: the executable name, truncated.
	fn helper_comm() -> String {
		let exe = std::env::current_exe().unwrap();
		let name = exe.file_name().unwrap().to_string_lossy();
		name.chars().take(15).collect()
	}

	fn spawn_adopted(sleep_ms: u64) -> (std::process::Child, OwnedChild) {
		let before = thread_children().unwrap();
		let child = helper(sleep_ms).spawn().unwrap();
		let owned = OwnedChild::adopt_new_child(&before, &helper_comm()).unwrap();
		assert_eq!(owned.pid() as u32, child.id());
		(child, owned)
	}

	#[test]
	fn stat_parsing_handles_hostile_command_names() {
		assert_eq!(parse_stat("42 (Xwayland) S 7 42 42"), Some(("Xwayland", 7)));
		assert_eq!(parse_stat("42 (a) b) (c) S 9 1"), Some(("a) b) (c", 9)));
		assert_eq!(parse_stat("garbage"), None);
	}

	#[test]
	fn graceful_exit_is_observed_without_killing() {
		let (mut child, owned) = spawn_adopted(0);
		assert_eq!(
			owned
				.terminate(Duration::from_secs(10), Duration::from_secs(1))
				.unwrap(),
			Termination::Exited
		);
		assert!(child.wait().unwrap().success(), "exited normally, not by signal");
	}

	#[test]
	fn unresponsive_child_is_killed_within_the_bound() {
		let (mut child, owned) = spawn_adopted(30_000);
		let started = Instant::now();
		assert_eq!(
			owned
				.terminate(Duration::from_millis(50), Duration::from_secs(5))
				.unwrap(),
			Termination::Killed
		);
		assert!(started.elapsed() < Duration::from_secs(5));
		assert!(!child.wait().unwrap().success());
		// Killing a reaped process through its pidfd is harmless.
		owned.kill().unwrap();
	}

	#[test]
	fn only_a_new_direct_child_with_the_expected_name_is_adopted() {
		let before = thread_children().unwrap();
		let mut child = helper(30_000).spawn().unwrap();
		// Wrong name: refused rather than guessed.
		assert!(OwnedChild::adopt_new_child(&before, "Xwayland").is_err());
		// Already-known children are never candidates.
		let after = thread_children().unwrap();
		assert!(OwnedChild::adopt_new_child(&after, &helper_comm()).is_err());
		// Processes that are not our children are never accepted.
		assert!(!is_direct_child(1, "systemd"));
		assert!(!is_direct_child(std::process::id() as i32, &helper_comm()));
		child.kill().unwrap();
		child.wait().unwrap();
	}

	#[test]
	fn adoption_does_not_race_the_childs_exec_rename() {
		// Each spawn returns before the kernel renames the child; adoption
		// must wait for the name instead of refusing the right process.
		for _ in 0..50 {
			let (mut child, _owned) = spawn_adopted(0);
			child.wait().unwrap();
		}
	}

	#[test]
	fn children_of_other_threads_are_not_adopted() {
		let before = thread_children().unwrap();
		let mut child = std::thread::spawn(|| helper(30_000).spawn().unwrap()).join().unwrap();
		assert!(OwnedChild::adopt_new_child(&before, &helper_comm()).is_err());
		child.kill().unwrap();
		child.wait().unwrap();
	}
}
