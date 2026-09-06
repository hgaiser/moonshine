//! Input regressions exercise transport injection and observe real client events.

use super::*;

fn root(harness: &mut Harness) -> ClientSurface {
	let root = harness.toplevel();
	harness.map(&root.surface, 800, 600);
	root
}

fn grabbed_popup(
	harness: &mut Harness,
	parent: &xdg_surface::XdgSurface,
	geometry: (i32, i32, i32, i32),
	serial: u32,
) -> ClientPopup {
	let popup = harness.uncommitted_popup(parent, geometry);
	popup.popup.grab(harness.client.seat.as_ref().unwrap(), serial);
	popup.surface.commit();
	harness.roundtrip();
	harness.map(&popup.surface, geometry.2, geometry.3);
	popup
}

fn destroy_popup(harness: &mut Harness, popup: ClientPopup) {
	popup.popup.destroy();
	popup.xdg_surface.destroy();
	popup.surface.destroy();
	harness.roundtrip();
}

fn limit_root_input(harness: &mut Harness, root: &ClientSurface) {
	let region = harness
		.client
		.compositor
		.as_ref()
		.unwrap()
		.create_region(&harness.qh, ());
	region.add(0, 0, 400, 300);
	root.surface.set_input_region(Some(&region));
	root.surface.commit();
	region.destroy();
	harness.roundtrip();
}

#[test]
fn popup_relative_motion_enters_menu_on_the_same_event_that_crosses_its_edge() {
	let mut h = Harness::new();
	let root = root(&mut h);
	let popup = h.popup(&root.xdg_surface, (100, 100, 160, 100));
	h.map(&popup.surface, 160, 100);
	h.move_pointer(95, 120);
	h.client.events.clear();
	h.input(CompositorInputEvent::MouseMoveRelative { dx: 10, dy: 0 });
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PointerEnter(popup.surface.id().protocol_id(), 5.0, 20.0)),
		"relative motion must hit-test the new cursor position: {:?}",
		h.client.events
	);
}

#[test]
fn popup_mapping_under_stationary_pointer_updates_the_next_click_target() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(120, 120);
	let popup = h.popup(&root.xdg_surface, (100, 100, 160, 100));
	h.map(&popup.surface, 160, 100);
	h.client.events.clear();
	h.press_pointer();
	assert!(h.client.events.iter().any(|event| matches!(event, ClientEvent::PointerButton(Some(surface), _, 0x110, 1) if *surface == popup.surface.id().protocol_id())), "a menu mapped under a stationary cursor must receive its first click: {:?}", h.client.events);
}

#[test]
fn popup_keyboard_action_can_open_a_grab_and_deliver_menu_navigation() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.input(CompositorInputEvent::KeyDown { keycode: 139 });
	let serial = h
		.client
		.events
		.iter()
		.rev()
		.find_map(|event| match event {
			ClientEvent::Key(_, serial, 139, 1) => Some(*serial),
			_ => None,
		})
		.expect("opening keyboard event");
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.client.events.clear();
	h.input(CompositorInputEvent::KeyDown { keycode: 108 });
	assert!(
		h.client.events.iter().any(
			|event| matches!(event, ClientEvent::Key(Some(surface), _, 108, 1) if *surface == popup.surface.id().protocol_id())
		),
		"keyboard-opened popup must receive navigation keys: {:?}",
		h.client.events
	);
}

#[test]
fn popup_pointer_grab_preserves_owner_events_and_dismisses_on_empty_space() {
	let mut h = Harness::new();
	let root = root(&mut h);
	limit_root_input(&mut h, &root);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.move_pointer(30, 30);
	h.client.events.clear();
	h.press_pointer();
	assert!(h.client.events.iter().any(|event| matches!(event, ClientEvent::PointerButton(Some(surface), _, _, 1) if *surface == root.surface.id().protocol_id())), "same-client parent receives owner-events");
	assert!(
		!h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"the client decides how to handle clicks in its own parent"
	);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.move_pointer(700, 500);
	h.input(CompositorInputEvent::MouseButtonDown { button: 0x110 });
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"empty-space click dismisses the popup grab"
	);
	assert_eq!(
		h.client.keyboard_focus,
		Some(root.surface.id().protocol_id()),
		"dismissal restores the root keyboard focus"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
}

#[test]
fn popup_nested_destroy_restores_keyboard_focus_without_another_input_event() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.move_pointer(120, 120);
	let serial = h.press_pointer();
	let child = grabbed_popup(&mut h, &popup.xdg_surface, (30, 20, 80, 60), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	assert_eq!(h.client.keyboard_focus, Some(child.surface.id().protocol_id()));
	destroy_popup(&mut h, child);
	assert_eq!(
		h.client.keyboard_focus,
		Some(popup.surface.id().protocol_id()),
		"closing a submenu restores the parent menu immediately"
	);
	destroy_popup(&mut h, popup);
	assert_eq!(
		h.client.keyboard_focus,
		Some(root.surface.id().protocol_id()),
		"closing the last menu restores the application immediately"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
}

#[test]
fn popup_focus_switch_dismisses_old_grab_and_focuses_the_new_window() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	let next = h.toplevel();
	h.map(&next.surface, 800, 600);
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"switching applications dismisses the old menu"
	);
	assert_eq!(
		h.client.keyboard_focus,
		Some(next.surface.id().protocol_id()),
		"popup grab must not trap focus on the old app"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
}

#[test]
fn popup_unknown_input_serial_cannot_steal_the_seat_grab() {
	let mut h = Harness::new();
	let root = root(&mut h);
	let popup = h.uncommitted_popup(&root.xdg_surface, (100, 100, 160, 100));
	popup.popup.grab(h.client.seat.as_ref().unwrap(), u32::MAX - 17);
	popup.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"unauthorized grab must be dismissed"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert_eq!(h.client.keyboard_focus, Some(root.surface.id().protocol_id()));
}

#[test]
fn popup_input_serial_delivered_to_another_client_cannot_authorize_a_grab() {
	let mut h = Harness::new();
	let _first_root = root(&mut h);
	h.move_pointer(30, 30);
	let first_client_serial = h.press_pointer();
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	let _first_client = h.add_client();
	let second_root = root(&mut h);
	let popup = h.uncommitted_popup(&second_root.xdg_surface, (100, 100, 160, 100));
	popup.popup.grab(h.client.seat.as_ref().unwrap(), first_client_serial);
	popup.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"a real serial delivered to a different Wayland client must not authorize a grab"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert_eq!(h.client.keyboard_focus, Some(second_root.surface.id().protocol_id()));
}

#[test]
fn popup_grab_requested_on_another_seat_is_rejected() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	let primary_seat_id = h.client.seat.as_ref().unwrap().id();
	let mut unrelated_seat = h
		.state
		.seat_state
		.new_wl_seat(&h.state.display_handle, "unrelated-test-seat");
	unrelated_seat.add_pointer();
	unrelated_seat
		.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 200, 25)
		.unwrap();
	unrelated_seat.add_touch();
	h.roundtrip();
	h.roundtrip();
	assert_ne!(
		h.client.seat.as_ref().unwrap().id(),
		primary_seat_id,
		"fixture bound the unrelated seat"
	);
	let popup = h.uncommitted_popup(&root.xdg_surface, (100, 100, 160, 100));
	popup.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	popup.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"a serial from the streaming seat cannot authorize a different seat"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!unrelated_seat.get_keyboard().unwrap().is_grabbed());
	assert!(!unrelated_seat.get_pointer().unwrap().is_grabbed());
}

#[test]
fn popup_touch_opening_routes_new_contacts_and_dismisses_on_empty_space() {
	let mut h = Harness::new();
	let root = root(&mut h);
	limit_root_input(&mut h, &root);
	h.input(CompositorInputEvent::TouchDown {
		slot: 0,
		x: 0.05,
		y: 0.05,
	});
	let serial = h
		.client
		.events
		.iter()
		.rev()
		.find_map(|event| match event {
			ClientEvent::TouchDown(_, serial, 0, _, _) => Some(*serial),
			_ => None,
		})
		.expect("opening touch event");
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::TouchUp { slot: 0 });
	h.input(CompositorInputEvent::TouchDown {
		slot: 3,
		x: 0.05,
		y: 0.05,
	});
	assert!(
		h.client.events.iter().any(
			|event| matches!(event, ClientEvent::TouchDown(surface, _, 3, _, _) if *surface == root.surface.id().protocol_id())
		),
		"touch owner-events remain available to the popup's parent"
	);
	assert!(
		!h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id()))
	);
	h.input(CompositorInputEvent::TouchUp { slot: 3 });
	h.client.events.clear();
	h.input(CompositorInputEvent::TouchDown {
		slot: 1,
		x: 125.0 / 799.0,
		y: 150.0 / 599.0,
	});
	assert!(h.client.events.iter().any(|event| matches!(event, ClientEvent::TouchDown(surface, _, 1, x, y) if *surface == popup.surface.id().protocol_id() && (*x - 25.0).abs() < 0.01 && (*y - 50.0).abs() < 0.01)), "native touch must reach the popup with local coordinates: {:?}", h.client.events);
	h.input(CompositorInputEvent::TouchUp { slot: 1 });
	h.input(CompositorInputEvent::TouchDown {
		slot: 2,
		x: 0.875,
		y: 0.875,
	});
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"touching empty space dismisses menu grabs"
	);
	assert_eq!(h.client.keyboard_focus, Some(root.surface.id().protocol_id()));
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
}

#[test]
fn popup_touch_grab_preserves_each_contact_while_another_finger_enters_the_menu() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.input(CompositorInputEvent::TouchDown {
		slot: 0,
		x: 0.05,
		y: 0.05,
	});
	let serial = h
		.client
		.events
		.iter()
		.rev()
		.find_map(|event| match event {
			ClientEvent::TouchDown(_, serial, 0, _, _) => Some(*serial),
			_ => None,
		})
		.unwrap();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.client.events.clear();
	// The opening finger is still held on the parent when a second contact
	// lands on the menu. Each contact must retain its own initial surface.
	h.input(CompositorInputEvent::TouchDown {
		slot: 1,
		x: 125.0 / 799.0,
		y: 150.0 / 599.0,
	});
	assert!(h.client.events.iter().any(|event| matches!(event, ClientEvent::TouchDown(surface, _, 1, x, y) if *surface == popup.surface.id().protocol_id() && (*x - 25.0).abs() < 0.01 && (*y - 50.0).abs() < 0.01)), "second finger must reach the popup despite the opening contact still being down: {:?}", h.client.events);
	h.input(CompositorInputEvent::TouchMove {
		slot: 0,
		x: 125.0 / 799.0,
		y: 150.0 / 599.0,
	});
	assert!(
		h.client.events.iter().any(
			|event| matches!(event, ClientEvent::TouchMotion(0, x, y) if (*x - 125.0).abs() < 0.01 && (*y - 150.0).abs() < 0.01)
		),
		"opening finger stays on parent in parent-local coordinates"
	);
	h.input(CompositorInputEvent::TouchMove {
		slot: 1,
		x: 300.0 / 799.0,
		y: 300.0 / 599.0,
	});
	assert!(
		h.client.events.iter().any(
			|event| matches!(event, ClientEvent::TouchMotion(1, x, y) if (*x - 200.0).abs() < 0.01 && (*y - 200.0).abs() < 0.01)
		),
		"menu contact retains its origin after moving outside the menu"
	);
	h.input(CompositorInputEvent::TouchUp { slot: 0 });
	h.input(CompositorInputEvent::TouchUp { slot: 1 });
	assert_eq!(h.client.keyboard_focus, Some(popup.surface.id().protocol_id()));
}

#[test]
fn popup_opened_with_mouse_can_be_dismissed_by_native_touch() {
	let mut h = Harness::new();
	let root = root(&mut h);
	limit_root_input(&mut h, &root);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.input(CompositorInputEvent::TouchDown {
		slot: 0,
		x: 0.875,
		y: 0.875,
	});
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"all popup grabs must handle native touch, including menus opened with a mouse"
	);
	assert_eq!(h.client.keyboard_focus, Some(root.surface.id().protocol_id()));
}

#[test]
fn popup_destroying_its_root_releases_grabs_and_clears_keyboard_focus() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	root.toplevel.destroy();
	root.xdg_surface.destroy();
	root.surface.destroy();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id()))
	);
	assert!(h.state.seat.get_keyboard().unwrap().current_focus().is_none());
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
}

#[test]
fn popup_destroying_an_inactive_window_does_not_dismiss_the_active_menu() {
	let mut h = Harness::new();
	let inactive = root(&mut h);
	let active = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &active.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.client.events.clear();
	inactive.toplevel.destroy();
	inactive.xdg_surface.destroy();
	inactive.surface.destroy();
	h.roundtrip();
	assert!(
		!h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"destroying another window must not cancel the active root's menu"
	);
	assert_eq!(h.client.keyboard_focus, Some(popup.surface.id().protocol_id()));
	assert!(h.state.seat.get_keyboard().unwrap().is_grabbed());
}

#[test]
fn popup_null_buffer_unmap_releases_grabs_and_restores_root_focus() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	popup.surface.attach(None, 0, 0);
	popup.surface.commit();
	h.roundtrip();
	assert_eq!(h.client.keyboard_focus, Some(root.surface.id().protocol_id()));
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
	assert!(h.state.popup_surfaces_for_render().is_empty());
}

#[test]
fn popup_wsi_input_uses_nested_popup_buffer_origins_with_window_geometry_offsets() {
	let mut h = Harness::new();
	let root = h.toplevel();
	root.xdg_surface.set_window_geometry(10, 15, 800, 600);
	h.map(&root.surface, 820, 630);
	let popup = h.popup(&root.xdg_surface, (100, 100, 160, 100));
	popup.xdg_surface.set_window_geometry(4, 6, 160, 100);
	h.map(&popup.surface, 168, 112);
	let child = h.popup(&popup.xdg_surface, (30, 20, 70, 50));
	child.xdg_surface.set_window_geometry(2, 3, 70, 50);
	h.map(&child.surface, 74, 56);
	h.state.override_surface = Some((h.server_surface(&root.surface), 0));
	h.client.events.clear();
	h.move_pointer(138, 132);
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PointerEnter(child.surface.id().protocol_id(), 10.0, 15.0)),
		"bypass hit testing must use exactly the nested buffer origin used for rendering: {:?}",
		h.client.events
	);
}

#[test]
fn popup_belonging_to_a_hidden_wsi_root_cannot_intercept_input() {
	let mut h = Harness::new();
	let hidden = root(&mut h);
	let active = root(&mut h);
	// Create this non-grabbing popup after the focus change so this is an
	// actual hidden tree, not one already dismissed by the focus transition.
	let popup = h.popup(&hidden.xdg_surface, (100, 100, 160, 100));
	h.map(&popup.surface, 160, 100);
	h.state.override_surface = Some((h.server_surface(&active.surface), 0));
	h.client.events.clear();
	h.move_pointer(120, 120);
	h.press_pointer();
	assert!(h.client.events.iter().any(|event| matches!(event, ClientEvent::PointerButton(Some(surface), _, _, 1) if *surface == active.surface.id().protocol_id())));
	assert!(
		!h.client.events.iter().any(
			|event| matches!(event, ClientEvent::PointerEnter(surface, _, _) if *surface == popup.surface.id().protocol_id())
		),
		"invisible menus cannot capture the streamed pointer"
	);
}

#[test]
fn popup_root_null_unmap_dismisses_menus_and_can_later_be_remapped() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	root.surface.attach(None, 0, 0);
	root.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"unmapping a root must dismiss its menus"
	);
	assert!(h.state.seat.get_keyboard().unwrap().current_focus().is_none());
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
	destroy_popup(&mut h, popup);
	root.surface.commit();
	h.roundtrip();
	h.map(&root.surface, 800, 600);
	assert_eq!(
		h.client.keyboard_focus,
		Some(root.surface.id().protocol_id()),
		"the retained toplevel role can remap and receive input again"
	);
}

#[test]
fn popup_null_unmapping_a_grabbed_submenu_restores_its_parent_menu() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.move_pointer(120, 120);
	let serial = h.press_pointer();
	let child = grabbed_popup(&mut h, &popup.xdg_surface, (30, 20, 80, 60), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.client.events.clear();
	child.surface.attach(None, 0, 0);
	child.surface.commit();
	h.roundtrip();
	assert!(
		!h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"unmapping the topmost submenu must not dismiss its parent"
	);
	assert_eq!(h.client.keyboard_focus, Some(popup.surface.id().protocol_id()));
	assert!(h.state.seat.get_keyboard().unwrap().is_grabbed());
}

#[test]
fn popup_null_unmapping_a_tooltip_does_not_release_another_popups_grab() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	let tooltip = h.popup(&root.xdg_surface, (400, 400, 80, 30));
	h.map(&tooltip.surface, 80, 30);
	h.client.events.clear();
	tooltip.surface.attach(None, 0, 0);
	tooltip.surface.commit();
	h.roundtrip();
	assert!(
		!h.client
			.events
			.contains(&ClientEvent::PopupDone(popup.popup.id().protocol_id())),
		"a non-grabbing tooltip must not dismiss the menu's grab"
	);
	assert_eq!(h.client.keyboard_focus, Some(popup.surface.id().protocol_id()));
	assert!(h.state.seat.get_keyboard().unwrap().is_grabbed());
}

#[test]
fn popup_client_socket_disconnect_cleans_up_grabs_and_focus() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	let _popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	h.input(CompositorInputEvent::TouchDown {
		slot: 0,
		x: 125.0 / 799.0,
		y: 150.0 / 599.0,
	});
	let server_root = h.server_surface(&root.surface);
	// An independent connection drives server dispatch after the menu-owning
	// client's actual socket closes. No xdg destroy requests are sent.
	let disconnected_peer = h.add_client();
	let socket = UnixStream::from(
		disconnected_peer
			.connection
			.backend()
			.poll_fd()
			.try_clone_to_owned()
			.unwrap(),
	);
	h.state.screen_dirty = false;
	socket.shutdown(std::net::Shutdown::Both).unwrap();
	drop(disconnected_peer);
	h.roundtrip();
	assert!(!smithay::utils::IsAlive::alive(&server_root));
	assert!(h.state.popup_grab.is_none());
	assert!(h.state.focused_window.is_none());
	assert!(h.state.seat.get_keyboard().unwrap().current_focus().is_none());
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
	assert!(h.state.screen_dirty, "disconnect must invalidate the streamed scene");
	assert!(
		super::super::popup_touch_focus::take_touch_contacts(&h.state.seat).is_empty(),
		"disconnect must release recorded contact recipients"
	);
}

#[test]
fn popup_touch_cancel_after_frames_cancels_contacts_without_closing_the_menu() {
	let mut h = Harness::new();
	let root = root(&mut h);
	h.input(CompositorInputEvent::TouchDown {
		slot: 0,
		x: 0.05,
		y: 0.05,
	});
	let serial = h
		.client
		.events
		.iter()
		.rev()
		.find_map(|event| match event {
			ClientEvent::TouchDown(_, serial, 0, _, _) => Some(*serial),
			_ => None,
		})
		.unwrap();
	let popup = grabbed_popup(&mut h, &root.xdg_surface, (100, 100, 160, 100), serial);
	h.input(CompositorInputEvent::TouchDown {
		slot: 1,
		x: 125.0 / 799.0,
		y: 150.0 / 599.0,
	});
	h.client.events.clear();
	h.input(CompositorInputEvent::TouchCancelAll);
	assert_eq!(
		h.client
			.events
			.iter()
			.filter(|event| **event == ClientEvent::TouchCancel)
			.count(),
		1,
		"cancel must reach the client once even after all down events have been framed"
	);
	assert_eq!(
		h.client.keyboard_focus,
		Some(popup.surface.id().protocol_id()),
		"cancelling contacts does not dismiss an otherwise active menu"
	);
	h.client.events.clear();
	h.input(CompositorInputEvent::TouchMove {
		slot: 0,
		x: 0.2,
		y: 0.2,
	});
	h.input(CompositorInputEvent::TouchUp { slot: 0 });
	h.input(CompositorInputEvent::TouchUp { slot: 1 });
	assert!(
		!h.client
			.events
			.iter()
			.any(|event| matches!(event, ClientEvent::TouchMotion(..) | ClientEvent::TouchUp(..))),
		"cancelled contacts must not receive later motion/up"
	);
	h.input(CompositorInputEvent::TouchDown {
		slot: 1,
		x: 125.0 / 799.0,
		y: 150.0 / 599.0,
	});
	assert!(
		h.client.events.iter().any(
			|event| matches!(event, ClientEvent::TouchDown(surface, _, 1, _, _) if *surface == popup.surface.id().protocol_id())
		),
		"new contacts work after cancellation"
	);
}
