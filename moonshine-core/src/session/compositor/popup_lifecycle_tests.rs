//! Lifecycle and placement regressions driven through the Wayland protocol.

use super::*;
use smithay::utils::Point;

fn popup_with_positioner(
	h: &mut Harness,
	parent: &xdg_surface::XdgSurface,
	positioner: &xdg_positioner::XdgPositioner,
) -> ClientPopup {
	let surface = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	let xdg_surface = h.client.shell.as_ref().unwrap().get_xdg_surface(&surface, &h.qh, ());
	let popup = xdg_surface.get_popup(Some(parent), positioner, &h.qh, ());
	surface.commit();
	h.roundtrip();
	ClientPopup {
		surface,
		xdg_surface,
		popup,
	}
}

fn configured_geometry(h: &Harness, popup: &ClientPopup) -> (i32, i32, i32, i32) {
	h.client
		.events
		.iter()
		.rev()
		.find_map(|event| match event {
			ClientEvent::PopupConfigure(id, geometry) if *id == popup.popup.id().protocol_id() => Some(*geometry),
			_ => None,
		})
		.expect("popup must receive its configured geometry")
}

#[test]
fn popup_configuration_waits_for_the_initial_surface_commit() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.uncommitted_popup(&root.xdg_surface, (30, 40, 100, 80));
	h.roundtrip();
	assert!(!h.client.events.iter().any(|event| matches!(event,
		ClientEvent::PopupConfigure(id, _) if *id == menu.popup.id().protocol_id()
	)));
	menu.surface.commit();
	h.roundtrip();
	assert_eq!(configured_geometry(&h, &menu), (30, 40, 100, 80));
}

#[test]
fn committed_submenu_is_tracked_at_its_parent_relative_position() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.popup(&root.xdg_surface, (30, 40, 160, 120));
	h.map(&menu.surface, 160, 120);
	let child = h.popup(&menu.xdg_surface, (120, 20, 90, 70));
	h.map(&child.surface, 90, 70);
	let tree: Vec<_> = PopupManager::popups_for_surface(&h.server_surface(&root.surface))
		.map(|(popup, offset)| (popup.wl_surface().clone(), offset))
		.collect();
	assert_eq!(
		tree,
		vec![
			(h.server_surface(&child.surface), Point::from((150, 60))),
			(h.server_surface(&menu.surface), Point::from((30, 40))),
		]
	);
}

#[test]
fn popup_positioner_constraints_keep_menus_inside_the_output() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let positioner = h.positioner((790, 590, 120, 100));
	positioner.set_constraint_adjustment(
		xdg_positioner::ConstraintAdjustment::SlideX | xdg_positioner::ConstraintAdjustment::SlideY,
	);
	let menu = popup_with_positioner(&mut h, &root.xdg_surface, &positioner);
	assert_eq!(configured_geometry(&h, &menu), (680, 500, 120, 100));
}

#[test]
fn reposition_replies_in_order_and_waits_for_client_commit_to_move() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.popup(&root.xdg_surface, (30, 40, 100, 80));
	h.map(&menu.surface, 100, 80);
	h.client.events.clear();
	let positioner = h.positioner((200, 150, 100, 80));
	menu.popup.reposition(&positioner, 42);
	h.roundtrip();
	let events: Vec<_> = h
		.client
		.events
		.iter()
		.filter(|event| {
			matches!(
				event,
				ClientEvent::Repositioned(_, _) | ClientEvent::PopupConfigure(_, _) | ClientEvent::Configure(_, _)
			)
		})
		.collect();
	assert!(matches!(
		events.as_slice(),
		[
			ClientEvent::Repositioned(_, 42),
			ClientEvent::PopupConfigure(_, (200, 150, 100, 80)),
			ClientEvent::Configure(_, _),
		]
	));
	let root_surface = h.server_surface(&root.surface);
	assert_eq!(
		PopupManager::popups_for_surface(&root_surface).next().unwrap().1,
		(30, 40).into()
	);
	menu.surface.commit();
	h.roundtrip();
	assert_eq!(
		PopupManager::popups_for_surface(&root_surface).next().unwrap().1,
		(200, 150).into()
	);
}

#[test]
fn reactive_popup_is_reconstrained_when_its_parent_moves() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let positioner = h.positioner((790, 590, 120, 100));
	positioner.set_reactive();
	positioner.set_constraint_adjustment(
		xdg_positioner::ConstraintAdjustment::SlideX | xdg_positioner::ConstraintAdjustment::SlideY,
	);
	let menu = popup_with_positioner(&mut h, &root.xdg_surface, &positioner);
	h.map(&menu.surface, 120, 100);
	assert_eq!(configured_geometry(&h, &menu), (680, 500, 120, 100));
	let root_surface = h.server_surface(&root.surface);
	let window = h
		.state
		.space
		.elements()
		.find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root_surface))
		.cloned()
		.unwrap();
	h.state.space.map_element(window, (100, 100), false);
	h.roundtrip();
	assert_eq!(configured_geometry(&h, &menu), (580, 400, 120, 100));
	let count = h
		.client
		.events
		.iter()
		.filter(|e| matches!(e, ClientEvent::PopupConfigure(_, _)))
		.count();
	menu.surface.commit();
	h.roundtrip();
	h.roundtrip();
	assert_eq!(
		h.client
			.events
			.iter()
			.filter(|e| matches!(e, ClientEvent::PopupConfigure(_, _)))
			.count(),
		count,
		"stable popup geometry must not cause an endless configure loop"
	);
}

#[test]
fn destroying_a_menu_invalidates_a_static_frame_and_removes_it_from_its_parent() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.popup(&root.xdg_surface, (30, 40, 100, 80));
	h.map(&menu.surface, 100, 80);
	h.state.screen_dirty = false;
	menu.popup.destroy();
	menu.xdg_surface.destroy();
	menu.surface.destroy();
	h.roundtrip();
	assert!(
		h.state.screen_dirty,
		"dismissed menus must disappear without another mouse motion"
	);
	assert_eq!(
		PopupManager::popups_for_surface(&h.server_surface(&root.surface)).count(),
		0
	);
}

#[test]
fn null_buffer_unmaps_popup_without_retaining_it_in_the_parent_tree() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.popup(&root.xdg_surface, (30, 40, 100, 80));
	h.map(&menu.surface, 100, 80);
	h.state.screen_dirty = false;
	menu.surface.attach(None, 0, 0);
	menu.surface.commit();
	h.roundtrip();
	assert!(h.state.screen_dirty);
	assert_eq!(
		PopupManager::popups_for_surface(&h.server_surface(&root.surface)).count(),
		0,
		"a live xdg_popup resource without a parent must not retain a mapped popup tree"
	);
}

#[test]
fn mapped_popup_receives_output_membership_and_scroll_input() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.popup(&root.xdg_surface, (100, 100, 160, 100));
	h.map(&menu.surface, 160, 100);
	assert!(
		h.client
			.events
			.contains(&ClientEvent::OutputEnter(menu.surface.id().protocol_id()))
	);
	assert!(h.client.events.contains(&ClientEvent::OutputScale(1)));
	h.move_pointer(120, 130);
	h.input(CompositorInputEvent::ScrollVertical { amount: 120 });
	h.input(CompositorInputEvent::ScrollHorizontal { amount: 120 });
	assert!(h.client.events.contains(&ClientEvent::PointerAxis(
		Some(menu.surface.id().protocol_id()),
		0,
		-15.0
	)));
	assert!(h.client.events.contains(&ClientEvent::PointerAxis(
		Some(menu.surface.id().protocol_id()),
		1,
		15.0
	)));
}
