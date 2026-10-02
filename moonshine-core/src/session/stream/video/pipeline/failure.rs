//! Failure contract at the encoder boundary (ARCH-001).
//!
//! Every frame-level failure says three things the pipeline owner needs:
//! which stage failed, how the stream recovers, and whether GPU work can still
//! read the frame's source buffer. The last answer decides whether the
//! compositor may reuse that buffer's content (`consumed`); the frame's
//! [`SourceLease`](crate::session::compositor::frame::SourceLease) keeps the
//! descriptors valid either way.
//!
//! [`FailurePolicy`] applies the recovery, bounds diagnostics and escalates a
//! failure that keeps repeating without any successful frame, so a backend
//! that has stopped working ends the stream instead of looping forever.

use std::time::{Duration, Instant};

/// Pipeline stage that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncodeStage {
	/// Creating per-session import/conversion objects.
	Setup,
	/// Importing the source DMA-BUF.
	Import,
	/// Converting/scaling the source into the encoder's input.
	Convert,
	/// Submitting encoder work.
	Submit,
	/// Waiting for submitted GPU work.
	Wait,
	/// Reading back the bitstream.
	Readback,
	/// Building transport packets.
	Packetize,
}

/// How the stream continues after a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
	/// Drop this frame. The encoder's reference chain is unaffected.
	DropFrame,
	/// Drop this frame and force an IDR: the encoder may have used it as a
	/// reference that the client will never receive.
	RequestIdr,
	/// The backend or device cannot produce further frames: stop the stream.
	Terminal,
}

/// Whether GPU work can still read the failed frame's source buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceAccess {
	/// No GPU work reading the source was submitted.
	NotSubmitted,
	/// Submitted reads are known to have completed (a fence/queue wait returned).
	Completed,
	/// Completion cannot be established (e.g. device loss during a wait).
	/// The source's content must not be released for reuse.
	Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct EncodeFailure {
	pub stage: EncodeStage,
	pub recovery: Recovery,
	pub source: SourceAccess,
	pub message: String,
}

impl EncodeFailure {
	pub(crate) fn new(
		stage: EncodeStage,
		recovery: Recovery,
		source: SourceAccess,
		message: impl Into<String>,
	) -> Self {
		Self {
			stage,
			recovery,
			source,
			message: message.into(),
		}
	}

	/// Whether the compositor may reuse the source buffer's content.
	pub(crate) fn releases_source(&self) -> bool {
		self.source != SourceAccess::Unknown
	}
}

impl std::fmt::Display for EncodeFailure {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{:?} failed: {}", self.stage, self.message)
	}
}

/// A failure streak this long, with no successful frame, is treated as a
/// backend failure even if each failure looked recoverable.
const ESCALATE_AFTER_FAILURES: u32 = 300;
const ESCALATE_AFTER: Duration = Duration::from_secs(5);
/// Repeated failures are summarized at most this often.
const LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Recovery policy and bounded diagnostics for one pipeline loop.
#[derive(Debug, Default)]
pub(crate) struct FailurePolicy {
	consecutive: u32,
	streak_started: Option<Instant>,
	last_log: Option<Instant>,
	suppressed: u64,
}

impl FailurePolicy {
	/// Record a failure and return the recovery to apply. Logs the first
	/// failure of a streak, then at most one summary per interval.
	pub(crate) fn record(&mut self, failure: &EncodeFailure, now: Instant) -> Recovery {
		self.consecutive = self.consecutive.saturating_add(1);
		let started = *self.streak_started.get_or_insert(now);
		let recovery = if failure.recovery != Recovery::Terminal
			&& self.consecutive >= ESCALATE_AFTER_FAILURES
			&& now.saturating_duration_since(started) >= ESCALATE_AFTER
		{
			Recovery::Terminal
		} else {
			failure.recovery
		};
		if recovery == Recovery::Terminal {
			tracing::error!(
				stage = ?failure.stage,
				error = %failure.message,
				consecutive_failures = self.consecutive,
				escalated = failure.recovery != Recovery::Terminal,
				"Video encoder failed permanently; stopping the stream"
			);
		} else if self
			.last_log
			.is_none_or(|last| now.saturating_duration_since(last) >= LOG_INTERVAL)
		{
			tracing::warn!(
				stage = ?failure.stage,
				error = %failure.message,
				recovery = ?recovery,
				consecutive_failures = self.consecutive,
				suppressed_since_last_report = self.suppressed,
				"Video frame failed"
			);
			self.last_log = Some(now);
			self.suppressed = 0;
		} else {
			self.suppressed += 1;
		}
		recovery
	}

	/// A frame completed: end the failure streak.
	pub(crate) fn record_success(&mut self) {
		if self.consecutive != 0 {
			tracing::info!(
				failed_frames = self.consecutive,
				"Video encoding recovered after failed frames"
			);
			self.consecutive = 0;
			self.streak_started = None;
			self.suppressed = 0;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn failure(recovery: Recovery) -> EncodeFailure {
		EncodeFailure::new(EncodeStage::Import, recovery, SourceAccess::NotSubmitted, "test")
	}

	#[test]
	fn terminal_failures_stop_immediately() {
		let mut policy = FailurePolicy::default();
		assert_eq!(
			policy.record(&failure(Recovery::Terminal), Instant::now()),
			Recovery::Terminal
		);
	}

	#[test]
	fn recoverable_failures_keep_their_recovery_until_the_streak_is_futile() {
		let mut policy = FailurePolicy::default();
		let start = Instant::now();
		// Many failures in a short burst stay recoverable (e.g. a format switch).
		for i in 0..ESCALATE_AFTER_FAILURES * 2 {
			let now = start + Duration::from_millis(u64::from(i));
			assert_eq!(policy.record(&failure(Recovery::RequestIdr), now), Recovery::RequestIdr);
		}
		// A long streak with few frames stays recoverable too.
		let mut sparse = FailurePolicy::default();
		for i in 0..10 {
			let now = start + Duration::from_secs(i * 10);
			assert_eq!(sparse.record(&failure(Recovery::DropFrame), now), Recovery::DropFrame);
		}
		// Both thresholds together escalate.
		let late = start + ESCALATE_AFTER;
		assert_eq!(policy.record(&failure(Recovery::DropFrame), late), Recovery::Terminal);
	}

	#[test]
	fn a_successful_frame_resets_the_streak() {
		let mut policy = FailurePolicy::default();
		let start = Instant::now();
		for i in 0..ESCALATE_AFTER_FAILURES {
			policy.record(
				&failure(Recovery::DropFrame),
				start + Duration::from_millis(u64::from(i)),
			);
		}
		policy.record_success();
		assert_eq!(
			policy.record(&failure(Recovery::DropFrame), start + ESCALATE_AFTER * 2),
			Recovery::DropFrame
		);
	}

	#[test]
	fn only_unknown_completion_retains_the_source() {
		for (source, releases) in [
			(SourceAccess::NotSubmitted, true),
			(SourceAccess::Completed, true),
			(SourceAccess::Unknown, false),
		] {
			let failure = EncodeFailure::new(EncodeStage::Wait, Recovery::Terminal, source, "test");
			assert_eq!(failure.releases_source(), releases);
		}
	}

	#[test]
	fn diagnostics_are_rate_limited() {
		let mut policy = FailurePolicy::default();
		let start = Instant::now();
		for i in 0..100 {
			policy.record(&failure(Recovery::DropFrame), start + Duration::from_millis(i));
		}
		assert_eq!(policy.suppressed, 99, "one report per interval");
		policy.record(&failure(Recovery::DropFrame), start + LOG_INTERVAL);
		assert_eq!(policy.suppressed, 0);
	}
}
