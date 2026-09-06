//! Explicit popup grabs require a grabbing popup parent or a toplevel.

use super::*;

fn dispatch_or_protocol_error(h: &mut Harness) -> Option<wayland_client::backend::protocol::ProtocolError> {
	h.sync_requested += 1;
	let target = h.sync_requested;
	h.connection.display().sync(&h.qh, target);
	let deadline = Instant::now() + Duration::from_secs(2);
	while h.client.sync_done < target {
		assert!(
			Instant::now() < deadline,
			"invalid popup request did not complete or raise a protocol error"
		);
		if let Some(error) = h.connection.protocol_error() {
			return Some(error);
		}
		let _ = h.connection.flush();
		h.display.dispatch_clients(&mut h.state).expect("dispatch test server");
		h.state.refresh_popups();
		h.display.flush_clients().expect("flush test server");
		let _ = h.queue.dispatch_pending(&mut h.client);
		if let Some(read) = h.queue.prepare_read() {
			let mut poll_fd = libc::pollfd {
				fd: h.connection.backend().poll_fd().as_raw_fd(),
				events: libc::POLLIN,
				revents: 0,
			};
			if unsafe { libc::poll(&mut poll_fd, 1, 0) } > 0 {
				let _ = read.read();
			}
		}
		let _ = h.queue.dispatch_pending(&mut h.client);
	}
	h.connection.protocol_error()
}

#[test]
fn popup_grab_with_a_non_grabbing_popup_parent_is_a_protocol_error() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let parent = h.popup(&root.xdg_surface, (100, 100, 160, 100));
	h.map(&parent.surface, 160, 100);
	h.move_pointer(120, 120);
	let serial = h.press_pointer();
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	let child = h.uncommitted_popup(&parent.xdg_surface, (100, 20, 100, 80));
	child.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	child.surface.commit();
	let error =
		dispatch_or_protocol_error(&mut h).expect("a popup without an explicit grab cannot parent a grabbing popup");
	assert_eq!(error.object_interface, "xdg_wm_base");
	assert_eq!(error.code, u32::from(xdg_wm_base::Error::NotTheTopmostPopup));
}

#[test]
fn popup_denied_before_mapping_cannot_be_regrabbed_as_an_invisible_menu() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.uncommitted_popup(&root.xdg_surface, (100, 100, 160, 100));
	menu.popup.grab(h.client.seat.as_ref().unwrap(), u32::MAX - 99);
	menu.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(menu.popup.id().protocol_id()))
	);
	h.input(CompositorInputEvent::KeyDown { keycode: 139 });
	let serial = h
		.client
		.events
		.iter()
		.rev()
		.find_map(|event| match event {
			ClientEvent::Key(_, serial, _, 1) => Some(*serial),
			_ => None,
		})
		.expect("valid opening key action");
	h.client.events.clear();
	menu.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	menu.surface.commit();
	h.roundtrip();
	assert!(
		!h.state.seat.get_keyboard().unwrap().is_grabbed(),
		"a dismissed popup cannot reacquire input while absent from the rendered tree"
	);
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(menu.popup.id().protocol_id()))
	);
}

#[test]
fn popup_child_of_a_dismissed_grabbing_parent_is_immediately_dismissed() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let region = h.client.compositor.as_ref().unwrap().create_region(&h.qh, ());
	region.add(0, 0, 400, 300);
	root.surface.set_input_region(Some(&region));
	root.surface.commit();
	region.destroy();
	h.roundtrip();
	h.move_pointer(30, 30);
	let serial = h.press_pointer();
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	let parent = h.uncommitted_popup(&root.xdg_surface, (100, 100, 160, 100));
	parent.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	parent.surface.commit();
	h.roundtrip();
	h.map(&parent.surface, 160, 100);
	h.move_pointer(700, 500);
	h.input(CompositorInputEvent::MouseButtonDown { button: 0x110 });
	h.input(CompositorInputEvent::MouseButtonUp { button: 0x110 });
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(parent.popup.id().protocol_id()))
	);
	let child = h.uncommitted_popup(&parent.xdg_surface, (100, 20, 100, 80));
	child.popup.grab(h.client.seat.as_ref().unwrap(), serial);
	child.surface.commit();
	h.roundtrip();
	assert!(
		h.client
			.events
			.contains(&ClientEvent::PopupDone(child.popup.id().protocol_id())),
		"an in-flight submenu request cannot revive its already dismissed parent grab"
	);
	assert!(!h.state.seat.get_keyboard().unwrap().is_grabbed());
	assert!(!h.state.seat.get_pointer().unwrap().is_grabbed());
	assert!(!h.state.seat.get_touch().unwrap().is_grabbed());
}
