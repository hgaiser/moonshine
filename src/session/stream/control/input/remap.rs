//! Hold-to-Home button remapping for gamepads.
//!
//! When configured, holding a source button (Back/Select or Share/Capture) for
//! a given duration emits the Home/Guide button instead of the source button.
//! A short tap (released before the threshold) still emits the source button.
//! NOTE: since the source button is withheld until either released or threshold reached, you can't
//! hold the source button.
//!
//! The logic is a pure, time-driven state machine so it can be unit-tested
//! without a real gamepad: feed it the incoming Moonlight button flags plus the
//! current time and it returns the flags that should actually be applied.

use std::time::{Duration, Instant};

use crate::config::{GamepadConfig, HomeButtonSource};

/// Moonlight button flags relevant to the remap.
pub const BACK_FLAG: u32 = 0x0020;
pub const SPECIAL_FLAG: u32 = 0x0400;
pub const MISC_FLAG: u32 = 0x0020_0000;

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
	/// Source button mask, or `None` when the remap is disabled.
	source_mask: Option<u32>,
	hold: Duration,
	/// Whether to auto-release Home after a brief tap (true) or hold it until
	/// the source button is released (false, allowing Home chords).
	auto_release: bool,
	state: State,
}

impl HoldToHome {
	pub fn new(config: &GamepadConfig) -> Self {
		let source_mask = if config.home_button_hold_ms == 0 {
			None
		} else {
			Some(match config.home_button_source {
				HomeButtonSource::Back => BACK_FLAG,
				HomeButtonSource::Share => MISC_FLAG,
			})
		};

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

#[cfg(test)]
mod tests {
	use super::*;

	fn cfg(source: HomeButtonSource, hold_ms: u64, auto_release: bool) -> GamepadConfig {
		GamepadConfig {
			home_button_source: source,
			home_button_hold_ms: hold_ms,
			home_button_auto_release: auto_release,
		}
	}

	#[test]
	fn disabled_passes_flags_through_unchanged() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 0, false));
		let t = Instant::now();
		assert_eq!(remap.apply(BACK_FLAG | 0x1000, t), BACK_FLAG | 0x1000);
		assert_eq!(remap.next_deadline(), None);
	}

	#[test]
	fn holding_past_threshold_emits_home_and_withholds_source() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 1000, false));
		let t0 = Instant::now();

		// Press: source withheld, timer scheduled.
		assert_eq!(remap.apply(BACK_FLAG, t0), 0);
		assert_eq!(remap.next_deadline(), Some(t0 + Duration::from_millis(1000)));

		// Still held before threshold: still withheld.
		assert_eq!(remap.apply(BACK_FLAG, t0 + Duration::from_millis(500)), 0);

		// Past threshold while held: Home emitted, source still withheld.
		assert_eq!(remap.apply(BACK_FLAG, t0 + Duration::from_millis(1000)), SPECIAL_FLAG);
		assert_eq!(remap.next_deadline(), None);
	}

	#[test]
	fn home_held_until_source_released() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 1000, false));
		let t0 = Instant::now();
		remap.apply(BACK_FLAG, t0);
		remap.apply(BACK_FLAG, t0 + Duration::from_millis(1000));

		// Still holding: Home stays pressed, combos pass through.
		assert_eq!(
			remap.apply(BACK_FLAG | 0x1000, t0 + Duration::from_millis(1500)),
			SPECIAL_FLAG | 0x1000
		);

		// Released: Home released.
		assert_eq!(remap.apply(0, t0 + Duration::from_millis(2000)), 0);
		assert_eq!(remap.next_deadline(), None);
	}

	#[test]
	fn auto_release_taps_home_then_releases_while_source_held() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 1000, true));
		let t0 = Instant::now();
		assert_eq!(remap.apply(BACK_FLAG, t0), 0);

		// Threshold: Home pressed, with a release timer scheduled.
		let thresh = t0 + Duration::from_millis(1000);
		assert_eq!(remap.apply(BACK_FLAG, thresh), SPECIAL_FLAG);
		assert_eq!(remap.next_deadline(), Some(thresh + TAP_DURATION));

		// Within the tap window, source still held: Home stays pressed.
		assert_eq!(remap.apply(BACK_FLAG, thresh + Duration::from_millis(50)), SPECIAL_FLAG);

		// After the tap window, source STILL held: Home auto-released.
		assert_eq!(remap.apply(BACK_FLAG, thresh + TAP_DURATION), 0);
		assert_eq!(remap.next_deadline(), None);

		// Continuing to hold does not re-trigger Home.
		assert_eq!(remap.apply(BACK_FLAG, thresh + Duration::from_millis(3000)), 0);

		// Releasing the source returns to idle without emitting the source button.
		assert_eq!(remap.apply(0, thresh + Duration::from_millis(3100)), 0);
		assert_eq!(remap.next_deadline(), None);
	}

	#[test]
	fn auto_release_off_holds_home_for_chords() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 1000, false));
		let t0 = Instant::now();
		remap.apply(BACK_FLAG, t0);

		let thresh = t0 + Duration::from_millis(1000);
		assert_eq!(remap.apply(BACK_FLAG, thresh), SPECIAL_FLAG);
		// No auto-release: no timer, Home stays down while held.
		assert_eq!(remap.next_deadline(), None);
		assert_eq!(
			remap.apply(BACK_FLAG, thresh + Duration::from_millis(5000)),
			SPECIAL_FLAG
		);
		// Released: Home released.
		assert_eq!(remap.apply(0, thresh + Duration::from_millis(6000)), 0);
	}

	#[test]
	fn short_tap_emits_source_button() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 1000, false));
		let t0 = Instant::now();

		// Press then release before threshold.
		assert_eq!(remap.apply(BACK_FLAG, t0), 0);
		let release = t0 + Duration::from_millis(200);
		assert_eq!(remap.apply(0, release), BACK_FLAG);
		assert_eq!(remap.next_deadline(), Some(release + TAP_DURATION));

		// Tap held until release_at.
		assert_eq!(remap.apply(0, release + Duration::from_millis(50)), BACK_FLAG);

		// After release_at: tap ends.
		assert_eq!(remap.apply(0, release + TAP_DURATION), 0);
		assert_eq!(remap.next_deadline(), None);
	}

	#[test]
	fn share_source_uses_misc_flag() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Share, 1000, false));
		let t0 = Instant::now();

		// Back is not the source, so it passes through untouched.
		assert_eq!(remap.apply(BACK_FLAG, t0), BACK_FLAG);
		assert_eq!(remap.next_deadline(), None);

		// Share (MISC) is withheld and starts the timer.
		assert_eq!(remap.apply(MISC_FLAG, t0), 0);
		assert_eq!(remap.apply(MISC_FLAG, t0 + Duration::from_millis(1000)), SPECIAL_FLAG);
	}

	#[test]
	fn real_guide_button_passes_through() {
		let mut remap = HoldToHome::new(&cfg(HomeButtonSource::Back, 1000, false));
		let t0 = Instant::now();
		// A controller with a real guide button: SPECIAL_FLAG passes through untouched.
		assert_eq!(remap.apply(SPECIAL_FLAG, t0), SPECIAL_FLAG);
	}
}
