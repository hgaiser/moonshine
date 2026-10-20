use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use zbus::Connection;

/// Holds a logind sleep-inhibit file descriptor for the lifetime of a session.
///
/// While this value is alive, logind will not suspend or hibernate the host.
/// Dropping it (which closes the underlying fd) releases the inhibit, so it is
/// enough to keep this in scope for the duration of a streaming session.
///
/// Moonshine acquires the inhibit on the user's behalf; the `moonshine` group
/// plus the polkit rule shipped with Moonshine grant the required permission
/// (see `dist/50-moonshine-inhibit-sleep.rules`).
pub(crate) struct SleepInhibitor {
	_fd: OwnedFd,
}

impl SleepInhibitor {
	/// Ask logind to block sleep for the duration of the session.
	///
	/// Returns `None` if the inhibit could not be acquired (for example the user
	/// is not in the `moonshine` group, or the polkit rule is missing). This is
	/// non-fatal: streaming still works, the host may just suspend.
	pub async fn acquire() -> Option<Self> {
		let conn = match Connection::system().await {
			Ok(conn) => conn,
			Err(e) => {
				tracing::warn!("Sleep inhibit unavailable (no system bus): {e}");
				return None;
			},
		};

		let msg = match conn
			.call_method(
				Some("org.freedesktop.login1"),
				"/org/freedesktop/login1",
				Some("org.freedesktop.login1.Manager"),
				"Inhibit",
				&("sleep", "Moonshine", "streaming session", "block"),
			)
			.await
		{
			Ok(msg) => msg,
			Err(e) => {
				tracing::warn!(
					"Failed to acquire sleep inhibit; is the user in the 'moonshine' group? Streaming continues, but the host may suspend."
				);
				tracing::debug!("Sleep inhibit error: {e}");
				return None;
			},
		};

		// logind returns the inhibit handle as a file descriptor carried in the
		// message header, not in the method body. Duplicate it so it survives
		// independently of the message.
		let raw = msg.data().fds().first().map(|fd| fd.as_raw_fd())?;
		let duped = unsafe { libc::dup(raw) };
		if duped < 0 {
			tracing::warn!("Failed to duplicate sleep inhibit fd");
			return None;
		}
		let fd = unsafe { OwnedFd::from_raw_fd(duped) };
		Some(Self { _fd: fd })
	}
}
