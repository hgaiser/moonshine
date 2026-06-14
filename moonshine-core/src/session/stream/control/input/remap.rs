//! Hold-to-Home button remapping for gamepads.
//!
//! When enabled, holding the Back/Select button for a given duration emits the
//! Home/Guide button instead of Back. A short tap (released before the
//! threshold) still emits Back.
//! NOTE: since Back is withheld until either released or the threshold is
//! reached, you can't hold the Back button.
//!
//! The state machine is driven by two paths: `apply()` on every input event,
//! and `advance()` on a timer when deadlines fire. `source_pressed` is tracked
//! internally so `advance()` can resolve transitions without stale flags.

use std::time::{Duration, Instant};

use super::gamepad::GamepadConfig;

/// Transition that occurred during the last [`HoldToHome::apply`] or
/// [`HoldToHome::advance`] call. Used by the caller to react to state changes
/// (e.g. fire a rumble pulse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldTransition {
	/// No notable transition.
	None,
	/// Home/Guide was just activated (Pending -> HomeHeld).
	HomeActivated,
}

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
	/// Source released early; emitting a brief source tap until `release_at`.
	Tapping { release_at: Instant },
}

/// Per-gamepad remap state machine.
pub struct HoldToHome {
	/// Back button mask, or `None` when the remap is disabled.
	source_mask: Option<u32>,
	hold: Duration,
	state: State,
	/// Whether the source button was last seen pressed. Updated on every `apply()`.
	source_pressed: bool,
	/// Whether to drop the physical Home/Guide button from passthrough.
	suppress_home: bool,
	/// The last passthrough flags (all buttons except source and optionally Home).
	/// Stored so `advance()` can reuse them when the timer fires, preserving
	/// other button state that might not be re-sent by the client.
	last_passthrough: u32,
}

impl HoldToHome {
	pub fn new(config: &GamepadConfig) -> Self {
		// The remap always targets the Back/Select button; a zero hold disables it.
		let source_mask = (config.home_button.hold_ms != 0).then_some(BACK_FLAG);

		Self {
			source_mask,
			hold: Duration::from_millis(config.home_button.hold_ms),
			state: State::Inactive,
			source_pressed: false,
			suppress_home: config.home_button.suppress_home,
			last_passthrough: 0,
		}
	}

	/// Process incoming button flags at time `now`, returning the flags that
	/// should actually be applied to the gamepad and any transition that occurred.
	pub fn apply(&mut self, flags: u32, now: Instant) -> (u32, HoldTransition) {
		let source_mask = match self.source_mask {
			Some(mask) => mask,
			// Disabled: pass through untouched.
			None => return (flags, HoldTransition::None),
		};

		let source_pressed = flags & source_mask != 0;
		self.source_pressed = source_pressed;
		// We manage the source bit ourselves; never pass the raw source bit through.
		// Optionally suppress the physical Home/Guide button too.
		let passthrough = if self.suppress_home {
			flags & !source_mask & !SPECIAL_FLAG
		} else {
			flags & !source_mask
		};
		self.last_passthrough = passthrough;

		match self.state {
			State::Inactive => {
				if source_pressed {
					self.state = State::Pending {
						deadline: now + self.hold,
					};
				}
				(passthrough, HoldTransition::None)
			},
			State::Pending { deadline } => {
				if !source_pressed {
					// Released before the threshold: emit a brief source tap.
					self.state = State::Tapping {
						release_at: now + TAP_DURATION,
					};
					(passthrough | source_mask, HoldTransition::None)
				} else if now >= deadline {
					// Home stays pressed as long as Back is held.
					self.state = State::HomeHeld;
					(passthrough | SPECIAL_FLAG, HoldTransition::HomeActivated)
				} else {
					(passthrough, HoldTransition::None)
				}
			},
			State::HomeHeld => {
				if source_pressed {
					(passthrough | SPECIAL_FLAG, HoldTransition::None)
				} else {
					self.state = State::Inactive;
					(passthrough, HoldTransition::None)
				}
			},
			State::Tapping { release_at } => {
				if now >= release_at {
					self.state = State::Inactive;
					(passthrough, HoldTransition::None)
				} else {
					// Keep the tap held until release_at, regardless of further input.
					(passthrough | source_mask, HoldTransition::None)
				}
			},
		}
	}

	/// Advance the state machine using internally tracked button state. Called
	/// by the timer task when a deadline fires. Returns the flags to apply and
	/// any transition that occurred.
	pub fn advance(&mut self, now: Instant) -> (u32, HoldTransition) {
		let source_mask = match self.source_mask {
			Some(mask) => mask,
			None => return (0, HoldTransition::None),
		};

		let passthrough = self.last_passthrough;
		match self.state {
			State::Pending { deadline } => {
				if now >= deadline && self.source_pressed {
					self.state = State::HomeHeld;
					(passthrough | SPECIAL_FLAG, HoldTransition::HomeActivated)
				} else {
					(0, HoldTransition::None)
				}
			},
			State::Tapping { release_at } => {
				if now >= release_at {
					self.state = State::Inactive;
					(passthrough, HoldTransition::None)
				} else {
					(passthrough | source_mask, HoldTransition::None)
				}
			},
			_ => (0, HoldTransition::None),
		}
	}

	/// The next time at which `advance()` should be called, or `None` if
	/// nothing is pending.
	pub fn next_deadline(&self) -> Option<Instant> {
		match self.state {
			State::Pending { deadline } => Some(deadline),
			State::Tapping { release_at } => Some(release_at),
			State::Inactive | State::HomeHeld => None,
		}
	}
}
