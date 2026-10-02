//! Renderer-independent scene decisions for native Wayland popups.

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::PopupManager;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};

use super::state::MoonshineCompositor;

impl MoonshineCompositor {
	/// Popup surface trees in front-to-back order, with global surface origins.
	pub(super) fn popup_surfaces_for_render(&self) -> Vec<(WlSurface, Point<i32, Logical>)> {
		let override_active = self.is_override_active();
		self.space.elements().rev()
			.filter(|window| !override_active || self.focused_window.as_ref() == Some(*window))
			.flat_map(|window| {
				let location = self.space.element_location(window).unwrap_or_default();
				window.toplevel().into_iter().flat_map(move |root| {
					PopupManager::popups_for_surface(root.wl_surface()).filter_map(move |(popup, offset)| {
						let surface = popup.wl_surface();
						let mapped = with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false);
						mapped.then(|| (surface.clone(), location + offset - popup.geometry().loc))
					})
				})
			})
			.collect()
	}
}
