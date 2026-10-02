//! Feedback routing for virtual controllers that outlive a controlling peer.
//!
//! A retained session keeps its virtual controllers across a Moonlight
//! reconnect so the running game never sees an unplug. Device lifetime and
//! input ownership are therefore separate: Inputtino's native callbacks
//! (rumble, LED, adaptive triggers) are installed once per device and must not
//! stay bound to the feedback channel of the peer that created it.
//!
//! [`FeedbackRoute`] is the indirection those callbacks hold. Revocation is
//! synchronous: after [`FeedbackRoute::revoke`] returns, no callback can obtain
//! a sender, so feedback produced while the device is unowned is dropped and can
//! never reach the next peer. A callback that obtained the previous owner's
//! sender just before revocation can only deliver into that owner's channel,
//! which the control stream has already replaced.
//!
//! Persistent device state is different from feedback events. Inputtino
//! deduplicates trigger effects per trigger and a game does not re-send LED or
//! trigger configuration to a device it believes never disconnected, so a
//! claim replays the device's *current* LED colour and per-trigger effects to
//! the new owner (and re-requests motion reports for motion-capable devices).
//! Rumble is transient and is never replayed.

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;

use crate::session::stream::control::FeedbackCommand;
use crate::session::stream::control::feedback::{EnableMotionEventCommand, SetLedCommand, TriggerEffectCommand};

/// `TriggerEffectCommand::trigger_event_flags` bits (DualSense output report).
const RIGHT_TRIGGER_EFFECT: u8 = 0x04;
const LEFT_TRIGGER_EFFECT: u8 = 0x08;
/// Motion report rate requested from the client, in Hz.
const MOTION_REPORT_RATE: u16 = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TriggerState {
	effect_type: u8,
	data: [u8; 10],
}

#[derive(Default)]
struct RouteState {
	owner: Option<mpsc::Sender<FeedbackCommand>>,
	led: Option<(u8, u8, u8)>,
	left_trigger: Option<TriggerState>,
	right_trigger: Option<TriggerState>,
}

impl RouteState {
	fn record(&mut self, command: &FeedbackCommand) {
		match command {
			FeedbackCommand::SetLed(led) => self.led = Some(led.rgb),
			FeedbackCommand::TriggerEffect(effect) => {
				if effect.trigger_event_flags & LEFT_TRIGGER_EFFECT != 0 {
					self.left_trigger = Some(TriggerState {
						effect_type: effect.type_left,
						data: effect.left,
					});
				}
				if effect.trigger_event_flags & RIGHT_TRIGGER_EFFECT != 0 {
					self.right_trigger = Some(TriggerState {
						effect_type: effect.type_right,
						data: effect.right,
					});
				}
			},
			FeedbackCommand::Rumble(_) | FeedbackCommand::EnableMotionEvent(_) => {},
		}
	}
}

/// Owner-switchable feedback destination for one virtual controller.
#[derive(Clone)]
pub(crate) struct FeedbackRoute {
	index: u8,
	/// Whether the device reports motion, so each owner must enable it.
	motion: bool,
	state: Arc<Mutex<RouteState>>,
}

impl FeedbackRoute {
	pub fn new(index: u8, motion: bool) -> Self {
		Self {
			index,
			motion,
			state: Default::default(),
		}
	}

	fn state(&self) -> MutexGuard<'_, RouteState> {
		// Holders never panic while locked; recover the plain data if one did.
		self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	/// Deliver feedback from an Inputtino callback thread to the current owner.
	///
	/// The lock is released before the (possibly blocking) send, so revocation
	/// is never delayed by a slow channel.
	pub fn deliver_blocking(&self, command: FeedbackCommand) {
		let owner = {
			let mut state = self.state();
			state.record(&command);
			state.owner.clone()
		};
		if let Some(owner) = owner {
			let _ = owner.blocking_send(command);
		}
	}

	/// Deliver host-generated feedback (the Home shortcut pulse) without
	/// blocking the gamepad thread.
	pub fn try_deliver(&self, command: FeedbackCommand) {
		let state = self.state();
		if let Some(owner) = &state.owner {
			let _ = owner.try_send(command);
		}
	}

	pub fn is_owned(&self) -> bool {
		self.state().owner.as_ref().is_some_and(|owner| !owner.is_closed())
	}

	/// Revoke the current owner; feedback is dropped until the next claim.
	pub fn revoke(&self) {
		self.state().owner = None;
	}

	/// Bind the route to `owner`. Returns `false` if it already was.
	///
	/// On a change, the device's persistent LED and trigger state is queued to
	/// the new owner while the lock is held, so any callback that runs after
	/// the claim is ordered after the replay. Motion enables are order
	/// independent and returned for the caller to send with backpressure.
	pub fn claim(&self, owner: &mpsc::Sender<FeedbackCommand>) -> Option<Vec<FeedbackCommand>> {
		let mut state = self.state();
		if state.owner.as_ref().is_some_and(|current| current.same_channel(owner)) {
			return None;
		}
		state.owner = Some(owner.clone());
		let id = u16::from(self.index);
		if let Some(rgb) = state.led {
			let _ = owner.try_send(FeedbackCommand::SetLed(SetLedCommand { id, rgb }));
		}
		if state.left_trigger.is_some() || state.right_trigger.is_some() {
			let left = state.left_trigger.unwrap_or_default();
			let right = state.right_trigger.unwrap_or_default();
			let mut flags = 0;
			if state.left_trigger.is_some() {
				flags |= LEFT_TRIGGER_EFFECT;
			}
			if state.right_trigger.is_some() {
				flags |= RIGHT_TRIGGER_EFFECT;
			}
			let _ = owner.try_send(FeedbackCommand::TriggerEffect(TriggerEffectCommand {
				id,
				trigger_event_flags: flags,
				type_left: left.effect_type,
				type_right: right.effect_type,
				left: left.data,
				right: right.data,
			}));
		}
		let motion = if self.motion {
			[
				inputtino::JoypadMotionType::ACCELERATION as u8,
				inputtino::JoypadMotionType::GYROSCOPE as u8,
			]
			.into_iter()
			.map(|motion_type| {
				FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
					id,
					report_rate: MOTION_REPORT_RATE,
					motion_type,
				})
			})
			.collect()
		} else {
			Vec::new()
		};
		Some(motion)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::control::feedback::RumbleCommand;

	fn rumble(level: u16) -> FeedbackCommand {
		FeedbackCommand::Rumble(RumbleCommand {
			id: 3,
			low_frequency: level,
			high_frequency: level,
		})
	}

	fn trigger(flags: u8, left: u8, right: u8) -> FeedbackCommand {
		FeedbackCommand::TriggerEffect(TriggerEffectCommand {
			id: 3,
			trigger_event_flags: flags,
			type_left: left,
			type_right: right,
			left: [left; 10],
			right: [right; 10],
		})
	}

	fn drain(rx: &mut mpsc::Receiver<FeedbackCommand>) -> Vec<FeedbackCommand> {
		std::iter::from_fn(|| rx.try_recv().ok()).collect()
	}

	/// Native callbacks run on Inputtino threads, never inside the runtime.
	fn from_native_thread(route: &FeedbackRoute, command: FeedbackCommand) {
		let route = route.clone();
		std::thread::spawn(move || route.deliver_blocking(command))
			.join()
			.unwrap();
	}

	#[test]
	fn feedback_after_revocation_never_reaches_any_peer() {
		let route = FeedbackRoute::new(3, false);
		let (old, mut old_rx) = mpsc::channel(10);
		let (new, mut new_rx) = mpsc::channel(10);
		route.claim(&old);
		from_native_thread(&route, rumble(1));
		assert_eq!(drain(&mut old_rx), vec![rumble(1)]);

		route.revoke();
		assert!(!route.is_owned());
		from_native_thread(&route, rumble(2));
		route.try_deliver(rumble(3));
		assert!(drain(&mut old_rx).is_empty(), "revoked owner receives nothing");

		route.claim(&new);
		assert!(drain(&mut new_rx).is_empty(), "rumble is transient and not replayed");
		from_native_thread(&route, rumble(4));
		assert_eq!(drain(&mut new_rx), vec![rumble(4)]);
		assert!(drain(&mut old_rx).is_empty());
	}

	#[test]
	fn closed_owner_is_not_considered_owned() {
		let route = FeedbackRoute::new(0, false);
		let (owner, owner_rx) = mpsc::channel(10);
		route.claim(&owner);
		assert!(route.is_owned());
		drop(owner_rx);
		assert!(!route.is_owned());
	}

	#[test]
	fn reclaim_by_the_same_owner_is_a_no_op() {
		let route = FeedbackRoute::new(1, true);
		let (owner, mut rx) = mpsc::channel(10);
		assert_eq!(route.claim(&owner).unwrap().len(), 2);
		assert!(route.claim(&owner.clone()).is_none(), "same channel, no re-enable");
		assert!(drain(&mut rx).is_empty());
	}

	#[test]
	fn new_owner_receives_current_persistent_state_and_motion_enable() {
		let route = FeedbackRoute::new(3, true);
		let (old, mut old_rx) = mpsc::channel(10);
		route.claim(&old);
		from_native_thread(&route, FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (1, 2, 3) }));
		// Left and right effects arrive separately; both persist.
		from_native_thread(&route, trigger(LEFT_TRIGGER_EFFECT, 5, 0));
		from_native_thread(&route, trigger(RIGHT_TRIGGER_EFFECT, 0, 7));
		route.revoke();
		// State changes while unowned are not delivered but are current state.
		from_native_thread(&route, FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (9, 9, 9) }));
		assert_eq!(drain(&mut old_rx).len(), 3);

		let (new, mut new_rx) = mpsc::channel(10);
		let motion = route.claim(&new).unwrap();
		assert_eq!(
			drain(&mut new_rx),
			vec![
				FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (9, 9, 9) }),
				FeedbackCommand::TriggerEffect(TriggerEffectCommand {
					id: 3,
					trigger_event_flags: LEFT_TRIGGER_EFFECT | RIGHT_TRIGGER_EFFECT,
					type_left: 5,
					type_right: 7,
					left: [5; 10],
					right: [7; 10],
				}),
			]
		);
		assert_eq!(motion.len(), 2);
		assert!(
			motion
				.iter()
				.all(|command| matches!(command, FeedbackCommand::EnableMotionEvent(enable) if enable.id == 3))
		);
	}

	#[test]
	fn devices_without_motion_or_state_replay_nothing() {
		let route = FeedbackRoute::new(2, false);
		let (owner, mut rx) = mpsc::channel(10);
		assert!(route.claim(&owner).unwrap().is_empty());
		assert!(drain(&mut rx).is_empty());
	}
}
