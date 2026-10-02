//! Recent user-action serials, scoped to the client that received the event.
//!
//! Popup grabs must refer to a button/key press or touch-down delivered to their
//! client. Keeping a bounded history also supports menus opened asynchronously
//! after the triggering event and nested menus reusing that event's serial.

const CAPACITY: usize = 64;

#[derive(Debug)]
pub(super) struct InputSerials<Client> {
	entries: std::collections::VecDeque<(u32, Client)>,
}

impl<Client> Default for InputSerials<Client> {
	fn default() -> Self {
		Self {
			entries: Default::default(),
		}
	}
}

impl<Client: PartialEq> InputSerials<Client> {
	pub fn record(&mut self, serial: u32, client: Client) {
		if self.entries.len() == CAPACITY {
			self.entries.pop_front();
		}
		self.entries.push_back((serial, client));
	}

	pub fn contains(&self, serial: u32, client: &Client) -> bool {
		self.entries
			.iter()
			.any(|(recorded, recipient)| *recorded == serial && recipient == client)
	}

	pub fn clear(&mut self) {
		self.entries.clear();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_a_delivered_action_authorizes_its_recipient() {
		let mut serials = InputSerials::default();
		assert!(!serials.contains(7, &"menu client"));
		serials.record(7, "menu client");
		assert!(serials.contains(7, &"menu client"));
		assert!(!serials.contains(7, &"other client"));
		assert!(!serials.contains(8, &"menu client"));
	}

	#[test]
	fn recent_actions_survive_later_actions_and_can_authorize_submenus() {
		let mut serials = InputSerials::default();
		serials.record(10, "menu client");
		serials.record(11, "menu client");
		assert!(serials.contains(10, &"menu client"));
		assert!(serials.contains(10, &"menu client"));
		assert!(serials.contains(11, &"menu client"));
	}

	#[test]
	fn bounded_history_expires_old_actions_and_handles_serial_wrap() {
		let mut serials = InputSerials::default();
		for offset in 0..=CAPACITY as u32 {
			serials.record(u32::MAX.wrapping_add(offset), 1);
		}
		assert!(!serials.contains(u32::MAX, &1));
		assert!(serials.contains(0, &1));
		assert!(serials.contains(CAPACITY as u32 - 1, &1));
		assert_eq!(serials.entries.len(), CAPACITY);
	}

	#[test]
	fn focus_loss_revokes_old_actions() {
		let mut serials = InputSerials::default();
		serials.record(10, "menu client");
		serials.clear();
		assert!(!serials.contains(10, &"menu client"));
	}
}
