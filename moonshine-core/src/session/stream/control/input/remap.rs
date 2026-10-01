//! Intentional Guide shortcuts, driven by input packets and one-shot deadlines.
//! Back+Start passes single buttons immediately. Once both are observed, it
//! releases/consumes both until both are up, even on cancellation. This avoids
//! synthetic taps and prevents the remaining chord member leaking into gameplay.
//! A single member already sent before chord recognition cannot be retracted.

use std::time::{Duration, Instant};

use super::gamepad::{GamepadConfig, HomeTrigger};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldTransition {
	None,
	HomeActivated,
}

pub const BACK_FLAG: u32 = 0x0020;
pub const START_FLAG: u32 = 0x0010;
pub const SPECIAL_FLAG: u32 = 0x0400;
const TAP_DURATION: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
	Inactive,
	Pending {
		deadline: Instant,
	},
	HomeHeld,
	/// Chord cancelled/released: consume members until both are up.
	Draining,
	/// Legacy short Back tap.
	Tapping {
		release_at: Instant,
	},
}

pub struct HoldToHome {
	trigger: HomeTrigger,
	hold: Duration,
	state: State,
	suppress_home: bool,
	/// Raw state is retained so *every* timer call preserves all held buttons,
	/// including calls made only to stop an activation rumble pulse.
	last_flags: u32,
}

impl HoldToHome {
	pub fn new(config: &GamepadConfig) -> Self {
		Self {
			trigger: config.home_button.trigger(),
			hold: Duration::from_millis(config.home_button.hold_ms),
			state: State::Inactive,
			suppress_home: config.home_button.suppress_home,
			last_flags: 0,
		}
	}

	pub fn apply(&mut self, flags: u32, now: Instant) -> (u32, HoldTransition) {
		self.last_flags = flags;
		let flags = if self.suppress_home {
			flags & !SPECIAL_FLAG
		} else {
			flags
		};
		let mask = match self.trigger {
			HomeTrigger::Disabled => return (flags, HoldTransition::None),
			HomeTrigger::HoldBack => BACK_FLAG,
			HomeTrigger::BackStart => BACK_FLAG | START_FLAG,
		};
		let pressed = flags & mask == mask;
		let passthrough = flags & !mask;
		let mut transition = HoldTransition::None;
		match self.state {
			State::Inactive if pressed => {
				self.state = State::Pending {
					deadline: now + self.hold,
				};
			},
			State::Pending { deadline } => {
				if !pressed {
					self.state = if self.trigger == HomeTrigger::HoldBack {
						State::Tapping {
							release_at: now + TAP_DURATION,
						}
					} else {
						State::Draining
					};
				} else if now >= deadline {
					self.state = State::HomeHeld;
					transition = HoldTransition::HomeActivated;
				}
			},
			State::HomeHeld if !pressed => {
				self.state = State::Draining;
			},
			State::Tapping { release_at } => {
				if pressed {
					// A new press starts a new hold, rather than getting lost in a tap.
					self.state = State::Pending {
						deadline: now + self.hold,
					};
				} else if now >= release_at {
					self.state = State::Inactive;
				}
			},
			_ => {},
		}
		if self.state == State::Draining && flags & mask == 0 {
			self.state = State::Inactive;
		}
		let output = match self.state {
			State::Inactive if self.trigger == HomeTrigger::BackStart => flags,
			State::HomeHeld => passthrough | SPECIAL_FLAG,
			State::Tapping { .. } => passthrough | BACK_FLAG,
			_ => passthrough,
		};
		(output, transition)
	}

	pub fn advance(&mut self, now: Instant) -> (u32, HoldTransition) {
		self.apply(self.last_flags, now)
	}

	pub fn next_deadline(&self) -> Option<Instant> {
		match self.state {
			State::Pending { deadline } => Some(deadline),
			State::Tapping { release_at } => Some(release_at),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(trigger: HomeTrigger) -> GamepadConfig {
		let mut config = GamepadConfig::default();
		config.home_button.trigger = Some(trigger);
		config.home_button.hold_ms = 750;
		config
	}

	#[test]
	fn plain_buttons_are_immediate_and_stay_held_on_timers() {
		let now = Instant::now();
		for trigger in [HomeTrigger::Disabled, HomeTrigger::BackStart] {
			for button in [BACK_FLAG, START_FLAG] {
				let mut remap = HoldToHome::new(&config(trigger));
				assert_eq!(remap.apply(button, now).0, button);
				assert!(remap.next_deadline().is_none());
				assert_eq!(remap.advance(now + Duration::from_secs(10)).0, button);
				assert_eq!(remap.apply(0, now).0, 0);
				assert_eq!(remap.advance(now).0, 0);
			}
		}
	}

	#[test]
	fn chord_activation_release_and_cancellation_preserve_other_buttons() {
		let now = Instant::now();
		let deadline = now + Duration::from_millis(750);
		for extra in [0, 0x1000, 0x10000, 0x20000, 0x40000, 0x80000, 0x3f1000] {
			for first in [0, BACK_FLAG, START_FLAG] {
				for release in [0, BACK_FLAG, START_FLAG] {
					for activate_on_timer in [false, true] {
						let mut r = HoldToHome::new(&config(HomeTrigger::BackStart));
						assert_eq!(r.apply(first | extra, now).0, first | extra);
						assert_eq!(r.apply(BACK_FLAG | START_FLAG | extra, now).0, extra);
						let output = if activate_on_timer {
							r.advance(deadline)
						} else {
							r.apply(BACK_FLAG | START_FLAG | extra, deadline)
						};
						assert_eq!(output, (extra | SPECIAL_FLAG, HoldTransition::HomeActivated));
						assert_eq!(
							r.advance(deadline + TAP_DURATION),
							(extra | SPECIAL_FLAG, HoldTransition::None)
						);
						assert_eq!(r.apply(release | extra, deadline).0, extra);
						assert!(r.next_deadline().is_none());
						assert_eq!(r.apply(0, deadline).0, 0);
						assert_eq!(r.apply(BACK_FLAG, deadline).0, BACK_FLAG);
					}
					let mut r = HoldToHome::new(&config(HomeTrigger::BackStart));
					r.apply(BACK_FLAG | START_FLAG | extra, now);
					assert_eq!(r.apply(release | extra, now + TAP_DURATION).0, extra);
					assert_eq!(r.advance(deadline).0, extra);
					assert!(r.next_deadline().is_none());
					assert_eq!(r.apply(0, deadline).0, 0);
				}
			}
		}
	}

	#[test]
	fn physical_guide_suppression_is_independent_of_synthetic_policy() {
		let now = Instant::now();
		for trigger in [HomeTrigger::Disabled, HomeTrigger::HoldBack, HomeTrigger::BackStart] {
			for suppress in [false, true] {
				let mut c = config(trigger);
				c.home_button.suppress_home = suppress;
				let mut r = HoldToHome::new(&c);
				let physical = if suppress { 0 } else { SPECIAL_FLAG };
				assert_eq!(r.apply(SPECIAL_FLAG, now).0, physical);
				assert_eq!(r.advance(now).0, physical);
				assert_eq!(r.apply(0, now).0, 0);
			}
		}
	}

	#[test]
	fn legacy_tap_and_hold_keep_deadlines_and_held_flags() {
		let now = Instant::now();
		let mut r = HoldToHome::new(&config(HomeTrigger::HoldBack));
		assert_eq!(r.apply(BACK_FLAG | 0x1000, now).0, 0x1000);
		assert_eq!(r.apply(0x1000, now + TAP_DURATION).0, BACK_FLAG | 0x1000);
		assert_eq!(r.advance(now + TAP_DURATION * 2).0, 0x1000);
		r.apply(BACK_FLAG | 0x1000, now);
		assert_eq!(
			r.advance(now + Duration::from_millis(750)).1,
			HoldTransition::HomeActivated
		);
		assert_eq!(r.advance(now + Duration::from_secs(1)).0, SPECIAL_FLAG | 0x1000);
		assert_eq!(r.apply(0, now).0, 0);
	}

	#[test]
	fn independent_slots_drop_pending_and_active_state_on_reconnect() {
		let now = Instant::now();
		let c = config(HomeTrigger::BackStart);
		let mut slots: [Option<HoldToHome>; 16] = std::array::from_fn(|_| None);
		for idx in [0, 15] {
			slots[idx] = Some(HoldToHome::new(&c));
		}
		slots[0].as_mut().unwrap().apply(BACK_FLAG | START_FLAG, now);
		assert_eq!(slots[15].as_mut().unwrap().apply(BACK_FLAG, now).0, BACK_FLAG);
		// Active-mask removal drops the slot, as does handler teardown.
		slots[0] = None;
		slots[0] = Some(HoldToHome::new(&c));
		assert!(slots[0].as_ref().unwrap().next_deadline().is_none());
		assert_eq!(slots[0].as_mut().unwrap().advance(now).0, 0);
		slots[0].as_mut().unwrap().apply(BACK_FLAG | START_FLAG, now);
		slots[0].as_mut().unwrap().advance(now + Duration::from_secs(1));
		slots[0] = Some(HoldToHome::new(&c));
		assert_eq!(slots[0].as_mut().unwrap().apply(BACK_FLAG, now).0, BACK_FLAG);
		assert_eq!(
			slots[15].as_mut().unwrap().advance(now + Duration::from_secs(1)).0,
			BACK_FLAG
		);
	}
}
