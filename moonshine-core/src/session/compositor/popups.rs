//! Native xdg_popup lifecycle, placement and input management.

use smithay::desktop::{
	PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window, find_popup_root_surface,
	get_popup_toplevel_coords,
};
use smithay::input::Seat;
use smithay::input::pointer::{ClickGrab, Focus, MotionEvent};
use smithay::input::touch::TouchDownGrab;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_wm_base;
use smithay::reexports::wayland_server::{
	Resource,
	protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
};
use smithay::utils::{IsAlive, SERIAL_COUNTER, Serial};
use smithay::wayland::compositor::get_role;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::{PopupSurface, XDG_POPUP_ROLE};

use super::popup_touch::PopupTouchGrab;
use super::state::MoonshineCompositor;

impl MoonshineCompositor {
	/// Only input actions actually delivered to this client authorize a grab.
	pub(super) fn record_input_serial(&mut self, serial: Serial, surface: Option<WlSurface>) {
		if let Some(client) = surface.and_then(|surface| surface.client()) {
			self.input_serials.record(serial.into(), client.id());
		}
	}

	pub(super) fn grab_popup(&mut self, popup: PopupSurface, seat_resource: WlSeat, serial: Serial) {
		let kind = PopupKind::Xdg(popup.clone());
		let Ok(root) = find_popup_root_surface(&kind) else {
			popup.send_popup_done();
			return;
		};
		// A dismissed resource may remain alive while its client processes
		// popup_done. It must never reacquire input outside the visible tree.
		if self.popups.find_popup(popup.wl_surface()).is_none() {
			popup.send_popup_done();
			return;
		}
		let owner = popup.wl_surface().client();
		let keyboard = self.seat.get_keyboard();
		let pointer = self.seat.get_pointer();
		let touch = self.seat.get_touch();
		let valid = Seat::<Self>::from_resource(&seat_resource).as_ref() == Some(&self.seat)
			&& owner
				.as_ref()
				.is_some_and(|client| self.input_serials.contains(serial.into(), &client.id()))
			&& self.focused_window.as_ref().and_then(|w| w.wl_surface()).as_deref() == Some(&root)
			&& keyboard.as_ref().is_none_or(|handle| {
				handle
					.with_grab(|_, grab| grab.is::<PopupKeyboardGrab<Self>>())
					.unwrap_or(true)
			}) && pointer.as_ref().is_none_or(|handle| {
			handle
				.with_grab(|_, grab| {
					grab.is::<PopupPointerGrab<Self>>()
						|| (grab.is::<ClickGrab<Self>>()
							&& grab
								.start_data()
								.focus
								.as_ref()
								.is_some_and(|(surface, _)| surface.id().same_client_as(&root.id())))
				})
				.unwrap_or(true)
		}) && touch.as_ref().is_none_or(|handle| {
			handle
				.with_grab(|_, grab| {
					grab.is::<PopupTouchGrab>()
						|| (grab.is::<TouchDownGrab<Self>>()
							&& grab
								.start_data()
								.focus
								.as_ref()
								.is_some_and(|(surface, _)| surface.same_client_as(&root.id())))
				})
				.unwrap_or(true)
		});
		if !valid {
			let _ = PopupManager::dismiss_popup(&root, &kind);
			self.screen_dirty = true;
			return;
		}
		let parent = popup.get_parent_surface().expect("validated popup root");
		let popup_parent = get_role(&parent) == Some(XDG_POPUP_ROLE);
		if popup_parent && self.popups.find_popup(&parent).is_none() {
			// A submenu request can already be in flight when its grabbing
			// parent is dismissed. Dismiss it as well, without killing the client.
			let _ = PopupManager::dismiss_popup(&root, &kind);
			self.screen_dirty = true;
			return;
		}
		let current = self
			.popup_grab
			.as_ref()
			.filter(|grab| !grab.has_ended())
			.and_then(|grab| grab.current_grab())
			.and_then(|focus| focus.wl_surface().map(|surface| surface.into_owned()));
		if current.as_ref().is_some_and(|surface| surface != &parent) || (popup_parent && current.is_none()) {
			self.popup_parent_protocol_error(&popup);
			return;
		}
		let root_focus = self.focused_window.clone().expect("validated popup root").into();
		let seat = self.seat.clone();
		let Ok(grab) = self.popups.grab_popup(root_focus, kind, &seat, serial) else {
			return;
		};
		if let Some(keyboard) = keyboard {
			keyboard.set_focus(self, grab.current_grab(), serial);
			keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
		}
		if let Some(pointer) = pointer {
			pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
		}
		if let Some(touch) = touch {
			touch.set_grab(self, PopupTouchGrab::new(&grab), serial);
		}
		self.popup_grab = Some(grab);
	}

	fn popup_parent_protocol_error(&self, popup: &PopupSurface) {
		let Some(client) = popup.wl_surface().client() else {
			return;
		};
		let mut shell_id = None;
		let _ = self
			.display_handle
			.backend_handle()
			.with_all_objects_for(client.id(), |id| {
				if shell_id.is_none() && id.interface().name == "xdg_wm_base" {
					shell_id = Some(id);
				}
			});
		// Smithay does not expose a popup's wm_base resource. An error is
		// fatal to the whole client, so any of its live wm_base bindings is
		// suitable even if it bound the same global more than once. Resolve
		// the resource outside the backend iterator's internal lock.
		if let Some(id) = shell_id
			&& let Ok(shell) = xdg_wm_base::XdgWmBase::from_id(&self.display_handle, id)
		{
			shell.post_error(
				xdg_wm_base::Error::NotTheTopmostPopup,
				"a grabbing popup must be parented to the topmost grabbing popup or toplevel",
			);
		}
	}

	pub(super) fn dismiss_popups_for_window(&mut self, window: Option<&Window>) {
		if let Some(grab) = &mut self.popup_grab
			&& window.is_none_or(|window| {
				grab.keyboard_grab_start_data()
					.focus
					.as_ref()
					.and_then(|focus| focus.window())
					== Some(window)
			}) {
			grab.ungrab(PopupUngrabStrategy::All);
		}
		if let Some(root) = window.and_then(|w| w.wl_surface()) {
			let popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
			for (popup, _) in popups.into_iter().rev() {
				let _ = PopupManager::dismiss_popup(&root, &popup);
			}
		}
		self.screen_dirty = true;
		self.reconcile_popup_grab();
	}

	/// A null-buffer unmap clears the popup's parent role data before the
	/// compositor sees the commit. Descendants still point to that surface,
	/// so test membership by walking down from the current grabbed popup.
	fn current_grab_descends_from(&self, ancestor: &WlSurface) -> bool {
		let Some(grab) = &self.popup_grab else { return false };
		if grab.has_ended() {
			return false;
		}
		let Some(focus) = grab.current_grab() else { return false };
		let Some(surface) = focus.wl_surface() else {
			return false;
		};
		let mut current = surface.into_owned();
		loop {
			if &current == ancestor {
				return true;
			}
			let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(&current) else {
				return false;
			};
			let Some(parent) = popup.get_parent_surface() else {
				return false;
			};
			current = parent;
		}
	}

	fn dismiss_popup_branch(&mut self, root: &WlSurface, popup: &PopupKind) {
		if self.current_grab_descends_from(popup.wl_surface()) {
			while let Some(grab) = &mut self.popup_grab {
				if grab.has_ended() {
					break;
				}
				let reached_target = grab
					.current_grab()
					.and_then(|focus| focus.wl_surface().map(|surface| surface.as_ref() == popup.wl_surface()))
					.unwrap_or(false);
				grab.ungrab(PopupUngrabStrategy::Topmost);
				if reached_target {
					break;
				}
			}
		}
		let _ = PopupManager::dismiss_popup(root, popup);
		self.screen_dirty = true;
	}

	/// Reconcile immediately, not on the next input event: a client may close
	/// a submenu or disappear while the streamed image and pointer are idle.
	pub(super) fn reconcile_popup_grab(&mut self) {
		self.popups.cleanup();
		let Some(grab) = self.popup_grab.as_ref() else { return };
		let ended = grab.has_ended();
		let focus = grab.current_grab().filter(IsAlive::alive);
		if ended {
			self.popup_grab = None;
			self.screen_dirty = true;
			if let Some(keyboard) = self.seat.get_keyboard()
				&& keyboard
					.with_grab(|_, grab| grab.is::<PopupKeyboardGrab<Self>>())
					.unwrap_or(false)
			{
				keyboard.unset_grab(self);
			}
			if let Some(pointer) = self.seat.get_pointer()
				&& pointer
					.with_grab(|_, grab| grab.is::<PopupPointerGrab<Self>>())
					.unwrap_or(false)
			{
				pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), self.clock.now().as_millis());
			}
			if let Some(touch) = self.seat.get_touch()
				&& touch.with_grab(|_, grab| grab.is::<PopupTouchGrab>()).unwrap_or(false)
			{
				touch.unset_grab(self);
			}
		}
		if let Some(keyboard) = self.seat.get_keyboard() {
			keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
		}
	}

	pub(super) fn refresh_popup_pointer(&mut self) {
		let under = super::input::find_surface_at(self, self.cursor_position);
		let Some(pointer) = self.seat.get_pointer() else { return };
		if self.popup_pointer_target != under || pointer.current_focus() != under.as_ref().map(|(s, _)| s.clone()) {
			self.popup_pointer_target = under.clone();
			pointer.motion(
				self,
				under,
				&MotionEvent {
					location: self.cursor_position,
					serial: SERIAL_COUNTER.next_serial(),
					time: self.clock.now().as_millis(),
				},
			);
			pointer.frame(self);
		}
	}
	/// Positioner coordinates are relative to the parent's window geometry,
	/// which is distinct from its buffer origin when it has shadows/decorations.
	pub(super) fn unconstrain_popup(&self, popup: &PopupSurface) {
		let kind = PopupKind::Xdg(popup.clone());
		let Ok(root) = find_popup_root_surface(&kind) else {
			return;
		};
		let Some(window) = self.space.elements().find(|w| w.wl_surface().as_deref() == Some(&root)) else {
			return;
		};
		let Some(window_geometry) = self.space.element_geometry(window) else {
			return;
		};
		let Some(mut target) = self.space.output_geometry(&self.output) else {
			return;
		};
		target.loc -= window_geometry.loc + get_popup_toplevel_coords(&kind);
		popup.with_pending_state(|state| {
			state.geometry = state.positioner.get_unconstrained_geometry(target);
		});
	}

	/// Run after client requests, even when no frame is rendered. This also
	/// updates output enter/leave and scale information for newly mapped menus.
	pub(super) fn refresh_popups(&mut self) {
		super::popup_touch_focus::prune_touch_contacts(&self.seat);
		let roots: Vec<_> = self
			.space
			.elements()
			.filter_map(|w| w.wl_surface().map(|s| s.into_owned()))
			.collect();
		for root in roots {
			let popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
			for (kind, _) in popups.into_iter().rev() {
				// Smithay resets the role's parent on a null-buffer unmap, while
				// the xdg_popup resource itself remains alive until destroy.
				if matches!(&kind, PopupKind::Xdg(popup) if popup.get_parent_surface().is_none()) {
					self.dismiss_popup_branch(&root, &kind);
					continue;
				}
				if let PopupKind::Xdg(popup) = kind
					&& popup.with_committed_state(|state| state.is_some_and(|s| s.positioner.reactive))
				{
					self.unconstrain_popup(&popup);
					if let Err(error) = popup.send_pending_configure() {
						tracing::debug!(?error, "Popup cannot be reactively reconfigured");
					}
				}
			}
		}
		self.popups.cleanup();
		self.space.refresh();
		self.reconcile_popup_grab();
		self.refresh_popup_pointer();
	}
}
