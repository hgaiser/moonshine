//! Hold-to-Home button remapping for gamepads.
//!
//! When enabled, holding the Back/Select button for a given duration emits the
//! Home/Guide button instead of Back. A short tap (released before the
//! threshold) still emits Back.
//! NOTE: since Back is withheld until either released or the threshold is
//! reached, you can't hold the Back button.
//!
//! The logic is a pure, time-driven state machine so it can be unit-tested
//! without a real gamepad: feed it the incoming Moonlight button flags plus the
//! current time and it returns the flags that should actually be applied.

use std::time::{Duration, Instant};

use crate::config::GamepadConfig;

/// Moonlight button flags relevant to the remap.
pub const BACK_FLAG: u32 = 0x0020;
pub const SPECIAL_FLAG: u32 = 0x0400;

/// How long the synthesised source-button tap is held when the button is
/// released before the hold threshold, so games reliably register it.
const TAP_DURATION: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
	/// Source button not active; nothing pending.
	Inactive,
	/// Source button held, waiting to see if it becomes a hold. Source bit is withheld.
	Pending { deadline: Instant },
	/// Threshold reached; Home/Guide is being emitted until the source is released.
	HomeHeld,
	/// Threshold reached with auto-release; Home/Guide is tapped until `release_at`.
	HomeTap { release_at: Instant },
	/// Home tap finished but the source is still held; swallow it until released.
	Consumed,
	/// Source released early; emitting a brief source tap until `release_at`.
	Tapping { release_at: Instant },
}

/// Per-gamepad remap state machine.
pub struct HoldToHome {
	/// Back button mask, or `None` when the remap is disabled.
	source_mask: Option<u32>,
	hold: Duration,
	/// Whether to auto-release Home after a brief tap (true) or hold it until
	/// Back is released (false, allowing Home chords).
	auto_release: bool,
	state: State,
}

impl HoldToHome {
	pub fn new(config: &GamepadConfig) -> Self {
		// The remap always targets the Back/Select button; a zero hold disables it.
		let source_mask = (config.home_button_hold_ms != 0).then_some(BACK_FLAG);

		Self {
			source_mask,
			hold: Duration::from_millis(config.home_button_hold_ms),
			auto_release: config.home_button_auto_release,
			state: State::Inactive,
		}
	}

	/// Process incoming button flags at time `now`, returning the flags that
	/// should actually be applied to the gamepad.
	pub fn apply(&mut self, flags: u32, now: Instant) -> u32 {
		let source_mask = match self.source_mask {
			Some(mask) => mask,
			// Disabled: pass through untouched.
			None => return flags,
		};

		let source_pressed = flags & source_mask != 0;
		// We manage the source bit ourselves; never pass the raw source bit through.
		let passthrough = flags & !source_mask;

		match self.state {
			State::Inactive => {
				if source_pressed {
					self.state = State::Pending {
						deadline: now + self.hold,
					};
				}
				passthrough
			},
			State::Pending { deadline } => {
				if !source_pressed {
					// Released before the threshold: emit a brief source tap.
					self.state = State::Tapping {
						release_at: now + TAP_DURATION,
					};
					passthrough | source_mask
				} else if now >= deadline {
					self.state = if self.auto_release {
						State::HomeTap {
							release_at: now + TAP_DURATION,
						}
					} else {
						State::HomeHeld
					};
					passthrough | SPECIAL_FLAG
				} else {
					passthrough
				}
			},
			State::HomeHeld => {
				if source_pressed {
					passthrough | SPECIAL_FLAG
				} else {
					self.state = State::Inactive;
					passthrough
				}
			},
			State::HomeTap { release_at } => {
				if now >= release_at {
					// Auto-release the Home tap. If the source is still held, swallow it
					// until released so Home is not immediately re-triggered.
					self.state = if source_pressed {
						State::Consumed
					} else {
						State::Inactive
					};
					passthrough
				} else {
					// Hold Home until release_at, regardless of further input.
					passthrough | SPECIAL_FLAG
				}
			},
			State::Consumed => {
				if !source_pressed {
					self.state = State::Inactive;
				}
				passthrough
			},
			State::Tapping { release_at } => {
				if now >= release_at {
					self.state = State::Inactive;
					passthrough
				} else {
					// Keep the tap held until release_at, regardless of further input.
					passthrough | source_mask
				}
			},
		}
	}

	/// The next time at which `apply` should be re-invoked (with the last known
	/// flags) so a pending timer can fire, or `None` if nothing is pending.
	pub fn next_deadline(&self) -> Option<Instant> {
		match self.state {
			State::Pending { deadline } => Some(deadline),
			State::HomeTap { release_at } => Some(release_at),
			State::Tapping { release_at } => Some(release_at),
			State::Inactive | State::HomeHeld | State::Consumed => None,
		}
	}
}
