//! Renderer-independent scene decisions shared with protocol regression tests.

use smithay::backend::renderer::element::surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree};
use smithay::backend::renderer::element::{Id, Kind, RenderElementStates};
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::backend::renderer::{ImportAll, Renderer};
use smithay::desktop::PopupManager;
use smithay::desktop::utils::{
	OutputPresentationFeedback, send_frames_surface_tree, take_presentation_feedback_surface_tree,
};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Monotonic, Point};
use smithay::wayland::presentation::Refresh;

use super::state::MoonshineCompositor;

/// Assemble the WSI scene using the same renderer interface in production and tests.
pub(super) fn override_render_elements<R>(
	renderer: &mut R,
	override_surface: &WlSurface,
	popups: &[(WlSurface, Point<i32, Logical>)],
) -> Vec<WaylandSurfaceRenderElement<R>>
where
	R: Renderer + ImportAll,
	R::TextureId: Clone + 'static,
{
	// Render elements are front-to-back. Replace only the game's base content;
	// native popup surface trees retain their position and stacking above it.
	let mut elements = Vec::new();
	for (surface, location) in popups {
		elements.extend(render_elements_from_surface_tree(
			renderer,
			surface,
			(location.x, location.y),
			1.0,
			1.0,
			Kind::Unspecified,
		));
	}
	elements.extend(render_elements_from_surface_tree(
		renderer,
		override_surface,
		(0, 0),
		1.0,
		1.0,
		Kind::Unspecified,
	));
	elements
}

impl MoonshineCompositor {
	/// Popup surface trees in front-to-back order, with global surface origins.
	pub(super) fn popup_surfaces_for_render(&self) -> Vec<(WlSurface, Point<i32, Logical>)> {
		let override_active = self.is_override_active();
		self.space
			.elements()
			.rev()
			.filter(|window| !override_active || self.focused_window.as_ref() == Some(*window))
			.flat_map(|window| {
				let location = self.space.element_location(window).unwrap_or_default();
				window.toplevel().into_iter().flat_map(move |root| {
					PopupManager::popups_for_surface(root.wl_surface()).filter_map(move |(popup, offset)| {
						let surface = popup.wl_surface();
						let mapped =
							with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false);
						mapped.then(|| (surface.clone(), location + offset - popup.geometry().loc))
					})
				})
			})
			.collect()
	}

	/// Whether cursor and popup visibility allow either single-buffer scanout path.
	pub(super) fn can_direct_scanout_scene(&self) -> bool {
		self.last_pointer_activity
			.is_none_or(|time| time.elapsed() > std::time::Duration::from_secs(3))
			&& self.popup_surfaces_for_render().is_empty()
	}

	/// Complete callbacks after the composed frame has finished rendering.
	pub(super) fn send_composited_frame_callbacks(&self, render_states: &RenderElementStates) {
		let override_active = self.is_override_active();
		let mut feedback = OutputPresentationFeedback::new(&self.output);
		let presented_on_output = |surface: &WlSurface, _: &smithay::wayland::compositor::SurfaceData| {
			render_states
				.element_was_presented(Id::from_wayland_resource(surface))
				.then(|| self.output.clone())
		};
		self.space.elements().for_each(|window| {
			window.send_frame(
				&self.output,
				self.clock.now(),
				Some(std::time::Duration::ZERO),
				|_, _| Some(self.output.clone()),
			);
			if !override_active {
				window.take_presentation_feedback(&mut feedback, presented_on_output, |_, _| {
					wp_presentation_feedback::Kind::empty()
				});
			}
		});
		if override_active && let Some((ref surface, _)) = self.override_surface {
			send_frames_surface_tree(
				surface,
				&self.output,
				self.clock.now(),
				Some(std::time::Duration::ZERO),
				|_, _| Some(self.output.clone()),
			);
			take_presentation_feedback_surface_tree(surface, &mut feedback, presented_on_output, |_, _| {
				wp_presentation_feedback::Kind::empty()
			});
			for (popup, _) in self.popup_surfaces_for_render() {
				take_presentation_feedback_surface_tree(&popup, &mut feedback, presented_on_output, |_, _| {
					wp_presentation_feedback::Kind::empty()
				});
			}
		}
		let frame_period = self
			.output
			.preferred_mode()
			.map(|mode| std::time::Duration::from_nanos(1_000_000_000_000u64 / mode.refresh.max(1) as u64))
			.unwrap_or(std::time::Duration::from_millis(11));
		feedback.presented::<smithay::utils::Time<Monotonic>, Monotonic>(
			self.clock.now(),
			Refresh::Fixed(frame_period),
			0,
			wp_presentation_feedback::Kind::empty(),
		);
	}
}

#[cfg(test)]
mod tests {
	use smithay::backend::renderer::element::{Element, Id, RenderElementStates};
	use smithay::backend::renderer::test::{DummyFramebuffer, DummyRenderer};
	use smithay::desktop::space::{SpaceRenderElements, space_render_elements};
	use smithay::utils::Point;

	use super::super::tests::Harness;
	use super::override_render_elements;

	fn render_scene(h: &mut Harness) -> RenderElementStates {
		let mut renderer = DummyRenderer;
		let elements = if h.state.is_override_active() {
			let popups = h.state.popup_surfaces_for_render();
			override_render_elements(&mut renderer, &h.state.override_surface.as_ref().unwrap().0, &popups)
				.into_iter()
				.map(SpaceRenderElements::Surface)
				.collect()
		} else {
			space_render_elements(&mut renderer, [&h.state.space], &h.state.output, 1.0).unwrap()
		};
		h.state
			.damage_tracker
			.render_output(&mut renderer, &mut DummyFramebuffer, 0, &elements, [0.0, 0.0, 0.0, 1.0])
			.expect("render actual SHM surface trees with the test backend")
			.states
	}

	#[test]
	fn mapped_popup_keeps_compositing_after_cursor_inactivity() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		h.state.last_pointer_activity = Some(std::time::Instant::now() - std::time::Duration::from_secs(4));

		assert!(
			!h.state.can_direct_scanout_scene(),
			"a committed menu must remain visible when the streamed cursor fades out"
		);
	}

	#[test]
	fn popup_without_a_buffer_does_not_disable_direct_scanout() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let _menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.state.last_pointer_activity = None;

		assert!(h.state.can_direct_scanout_scene());
		assert!(h.state.popup_surfaces_for_render().is_empty());
	}

	#[test]
	fn destroying_last_popup_restores_direct_scanout() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		h.state.last_pointer_activity = None;
		assert!(!h.state.can_direct_scanout_scene());
		h.state.screen_dirty = false;

		menu.popup.destroy();
		menu.xdg_surface.destroy();
		menu.surface.destroy();
		h.roundtrip();

		assert!(
			h.state.screen_dirty,
			"closing a stationary menu must repaint immediately"
		);
		assert!(h.state.can_direct_scanout_scene());
		assert!(h.state.popup_surfaces_for_render().is_empty());
	}

	#[test]
	fn override_scene_keeps_focused_nested_menus_in_front_to_back_order() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		// Create both toplevels before their popup trees: mapping a new
		// toplevel correctly dismisses popups belonging to the old focus.
		let other_root = h.toplevel();
		h.map(&other_root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		let submenu = h.popup(&menu.xdg_surface, (80, 20, 90, 60));
		h.map(&submenu.surface, 90, 60);
		let other_menu = h.popup(&other_root.xdg_surface, (5, 5, 50, 50));
		h.map(&other_menu.surface, 50, 50);

		let root_surface = h.server_surface(&root.surface);
		h.state.focused_window = h
			.state
			.space
			.elements()
			.find(|window| window.toplevel().is_some_and(|t| t.wl_surface() == &root_surface))
			.cloned();
		h.state.focused_x11_window = None;
		h.state.override_surface = Some((root_surface, 0));
		h.state.last_pointer_activity = None;

		assert_eq!(
			h.state.popup_surfaces_for_render(),
			vec![
				(h.server_surface(&submenu.surface), Point::from((120, 70))),
				(h.server_surface(&menu.surface), Point::from((40, 50))),
			],
			"WSI replaces the game buffer but must preserve that game's popup stack"
		);
		assert!(!h.state.can_direct_scanout_scene());
	}

	#[test]
	fn override_render_elements_import_popup_buffers_before_the_game_buffer() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		let root_surface = h.server_surface(&root.surface);
		let menu_surface = h.server_surface(&menu.surface);
		let mut renderer = DummyRenderer;

		let elements = override_render_elements(&mut renderer, &root_surface, &[(menu_surface, (40, 50).into())]);
		assert_eq!(
			elements.len(),
			2,
			"both menu and replacement game buffer must reach the renderer"
		);
		assert_eq!(
			elements[0].geometry(1.0.into()),
			smithay::utils::Rectangle::new((40, 50).into(), (120, 100).into())
		);
		assert_eq!(
			elements[1].geometry(1.0.into()),
			smithay::utils::Rectangle::new((0, 0).into(), (800, 600).into())
		);
	}

	#[test]
	fn popup_render_origin_accounts_for_client_side_window_geometry() {
		let mut h = Harness::new();
		let root = h.toplevel();
		root.xdg_surface.set_window_geometry(10, 20, 600, 400);
		h.map(&root.surface, 640, 480);
		let root_surface = h.server_surface(&root.surface);
		let root_window = h
			.state
			.space
			.elements()
			.find(|window| window.toplevel().is_some_and(|t| t.wl_surface() == &root_surface))
			.cloned()
			.unwrap();
		h.state.space.map_element(root_window, (35, 45), false);
		let menu = h.popup(&root.xdg_surface, (70, 80, 80, 40));
		menu.xdg_surface.set_window_geometry(7, 9, 80, 40);
		h.map(&menu.surface, 94, 58);

		assert_eq!(
			h.state.popup_surfaces_for_render(),
			vec![(h.server_surface(&menu.surface), Point::from((98, 116)))],
			"popup positioning is relative to parent window geometry, not buffer shadows"
		);
	}

	#[test]
	fn composited_menu_receives_frame_and_presentation_callbacks() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		let frame = h.request_frame(&menu.surface);
		let presentation = h.request_presentation(&menu.surface);
		h.roundtrip();

		let render_states = render_scene(&mut h);
		h.state.send_composited_frame_callbacks(&render_states);
		h.roundtrip();

		assert!(
			h.frame_done(frame),
			"popup animation must receive the next-frame callback"
		);
		assert!(
			h.presentation_received(presentation),
			"composited popup must receive presentation feedback"
		);
	}

	#[test]
	fn override_composition_completes_game_and_popup_callbacks() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		let root_surface = h.server_surface(&root.surface);
		h.state.override_surface = Some((root_surface, 0));
		let root_frame = h.request_frame(&root.surface);
		let menu_frame = h.request_frame(&menu.surface);
		let root_presentation = h.request_presentation(&root.surface);
		let menu_presentation = h.request_presentation(&menu.surface);
		h.roundtrip();

		let render_states = render_scene(&mut h);
		h.state.send_composited_frame_callbacks(&render_states);
		h.roundtrip();

		assert!(h.frame_done(root_frame) && h.frame_done(menu_frame));
		assert!(
			h.presentation_received(root_presentation),
			"WSI present must unblock during menu composition"
		);
		assert!(
			h.presentation_received(menu_presentation),
			"popup must share the composed-frame presentation"
		);
	}

	#[test]
	fn inactive_override_surface_is_not_reported_as_presented() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let root_surface = h.server_surface(&root.surface);
		let root_window = h.state.space.elements().next().cloned().unwrap();
		h.state.space.unmap_elem(&root_window);
		h.state.override_surface = Some((root_surface, 42));
		h.state.focused_x11_window = None;
		let presentation = h.request_presentation(&root.surface);
		h.roundtrip();

		let render_states = render_scene(&mut h);
		h.state.send_composited_frame_callbacks(&render_states);
		h.roundtrip();

		assert!(
			!h.presentation_received(presentation),
			"a hidden, inactive WSI surface was not presented"
		);
	}

	#[test]
	fn native_space_composition_imports_nested_menus_before_their_parent() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		let submenu = h.popup(&menu.xdg_surface, (80, 20, 90, 60));
		h.map(&submenu.surface, 90, 60);
		let elements = space_render_elements(&mut DummyRenderer, [&h.state.space], &h.state.output, 1.0).unwrap();
		assert_eq!(
			elements.iter().map(|element| element.id().clone()).collect::<Vec<_>>(),
			vec![
				Id::from_wayland_resource(&h.server_surface(&submenu.surface)),
				Id::from_wayland_resource(&h.server_surface(&menu.surface)),
				Id::from_wayland_resource(&h.server_surface(&root.surface)),
			]
		);
	}

	#[test]
	fn detaching_popup_buffer_restores_direct_scanout() {
		let mut h = Harness::new();
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		h.state.last_pointer_activity = None;
		assert!(!h.state.can_direct_scanout_scene());
		menu.surface.attach(None, 0, 0);
		menu.surface.commit();
		h.roundtrip();
		assert!(h.state.popup_surfaces_for_render().is_empty());
		assert!(h.state.can_direct_scanout_scene());
	}

	#[test]
	fn fully_occluded_menu_is_not_reported_as_presented() {
		let mut h = Harness::new();
		let covering_window = h.toplevel();
		h.map(&covering_window.surface, 800, 600);
		let root = h.toplevel();
		h.map(&root.surface, 800, 600);
		let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
		h.map(&menu.surface, 120, 100);
		let covering_surface = h.server_surface(&covering_window.surface);
		let covering = h
			.state
			.space
			.elements()
			.find(|window| window.toplevel().is_some_and(|t| t.wl_surface() == &covering_surface))
			.cloned()
			.unwrap();
		h.state.space.raise_element(&covering, false);
		let presentation = h.request_presentation(&menu.surface);
		h.roundtrip();
		let render_states = render_scene(&mut h);
		assert!(
			!render_states.element_was_presented(Id::from_wayland_resource(&h.server_surface(&menu.surface))),
			"opaque foreground window must occlude the menu"
		);

		h.state.send_composited_frame_callbacks(&render_states);
		h.roundtrip();

		assert!(
			!h.presentation_received(presentation),
			"a completely occluded popup was not presented"
		);
	}
}
