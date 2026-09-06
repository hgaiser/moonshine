//! A touch serial belongs to its delivered recipient, not the hit-tested one.

use super::*;

#[test]
fn popup_implicit_touch_recipient_cannot_authorize_the_client_under_another_finger() {
	let mut h = Harness::new();
	let first_root = h.toplevel();
	h.map(&first_root.surface, 800, 600);
	let mut first_client = h.add_client();
	let second_root = h.toplevel();
	h.map(&second_root.surface, 800, 600);
	let region = h.client.compositor.as_ref().unwrap().create_region(&h.qh, ());
	region.add(400, 0, 400, 600);
	second_root.surface.set_input_region(Some(&region));
	second_root.surface.commit();
	region.destroy();
	h.roundtrip();
	assert_eq!(h.client.keyboard_focus, Some(second_root.surface.id().protocol_id()));

	// The first finger's implicit grab owns subsequent contacts, even when
	// another finger is geometrically above a different client.
	h.input(CompositorInputEvent::TouchDown {
		slot: 0,
		x: 0.125,
		y: 0.25,
	});
	h.input(CompositorInputEvent::TouchDown {
		slot: 1,
		x: 0.75,
		y: 0.25,
	});
	assert!(
		!h.client
			.events
			.iter()
			.any(|event| matches!(event, ClientEvent::TouchDown(..))),
		"the hit-tested second client must not receive either implicitly grabbed contact"
	);
	h.swap_client(&mut first_client);
	h.roundtrip();
	let serial = h
		.client
		.events
		.iter()
		.find_map(|event| match event {
			ClientEvent::TouchDown(surface, serial, 1, _, _) if *surface == first_root.surface.id().protocol_id() => {
				Some(*serial)
			},
			_ => None,
		})
		.expect("the first client received the second contact's serial");
	h.swap_client(&mut first_client);
	h.input(CompositorInputEvent::TouchUp { slot: 1 });
	h.input(CompositorInputEvent::TouchUp { slot: 0 });

	// Simulate a guessed serial. The focused second root must still be denied:
	// releasing the implicit grab does not transfer ownership of prior input.
	let menu = h.uncommitted_popup(&second_root.xdg_surface, (450, 100, 100, 80));
	menu.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	menu.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(menu.popup.id().protocol_id())),
		"a serial delivered to the implicit-grab owner cannot authorize the hit-tested client"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
}
