//! Authorization uses delivered action serials, including typed keyboard input.

use super::*;

#[test]
fn typed_key_press_can_authorize_a_popup_grab() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	h.input(CompositorInputEvent::TypeText { text: "a".into() });
	let serial = h
		.client
		.events
		.iter()
		.find_map(|event| match event {
			ClientEvent::Key(_, serial, _, 1) => Some(*serial),
			_ => None,
		})
		.expect("typed key press delivered");
	let popup = h.uncommitted_popup(&root.xdg_surface, (100, 100, 160, 100));
	popup.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	popup.surface.commit();
	h.roundtrip();
	assert!(
		!h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"a real typed key press is an input action"
	);
	assert_eq!(h.client.keyboard_focus, Some(popup.surface.id().protocol_id()));
}

#[test]
fn key_release_serial_does_not_authorize_a_popup_grab() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	h.input(CompositorInputEvent::KeyDown { keycode: 139 });
	h.input(CompositorInputEvent::KeyUp { keycode: 139 });
	let serial = h
		.client
		.events
		.iter()
		.find_map(|event| match event {
			ClientEvent::Key(_, serial, _, 0) => Some(*serial),
			_ => None,
		})
		.expect("key release delivered");
	let popup = h.uncommitted_popup(&root.xdg_surface, (100, 100, 160, 100));
	popup.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	popup.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id()))
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
}
