//! Record popup authorization at the actual touch-delivery boundary.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use smithay::backend::input::TouchSlot;
use smithay::input::Seat;
use smithay::input::dnd::{DndFocus, Source};
use smithay::input::touch::{DownEvent, MotionEvent, OrientationEvent, ShapeEvent, TouchTarget, UpEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{DisplayHandle, Resource};
use smithay::utils::{IsAlive, Logical, Point, Serial};
use smithay::wayland::seat::WaylandFocus;

use super::state::MoonshineCompositor;

/// A contact's recipient may differ from the surface under the finger because
/// an implicit or compositor-owned touch grab can redirect the down event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TouchFocusTarget(WlSurface);

/// Actual delivered contacts, including ones predating installation of a
/// popup grab. Seat-owned storage also keeps separate seats independent.
#[derive(Default)]
struct ActiveTouchContacts(Mutex<HashMap<TouchSlot, (TouchFocusTarget, DownEvent)>>);

fn active_contacts(seat: &Seat<MoonshineCompositor>) -> &ActiveTouchContacts {
	seat.user_data()
		.insert_if_missing_threadsafe(ActiveTouchContacts::default);
	seat.user_data().get::<ActiveTouchContacts>().unwrap()
}

pub(super) fn take_touch_contacts(seat: &Seat<MoonshineCompositor>) -> Vec<(TouchFocusTarget, DownEvent)> {
	active_contacts(seat)
		.0
		.lock()
		.unwrap()
		.drain()
		.map(|(_, contact)| contact)
		.collect()
}

pub(super) fn prune_touch_contacts(seat: &Seat<MoonshineCompositor>) {
	active_contacts(seat)
		.0
		.lock()
		.unwrap()
		.retain(|_, (target, _)| target.alive());
}

impl From<WlSurface> for TouchFocusTarget {
	fn from(surface: WlSurface) -> Self {
		Self(surface)
	}
}

impl IsAlive for TouchFocusTarget {
	fn alive(&self) -> bool {
		self.0.alive()
	}
}

impl WaylandFocus for TouchFocusTarget {
	fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
		Some(Cow::Borrowed(&self.0))
	}
}

impl TouchTarget<MoonshineCompositor> for TouchFocusTarget {
	fn down(&self, seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, event: &DownEvent, seq: Serial) {
		if self.alive() {
			active_contacts(seat)
				.0
				.lock()
				.unwrap()
				.insert(event.slot, (self.clone(), event.clone()));
		}
		data.record_input_serial(event.serial, Some(self.0.clone()));
		TouchTarget::down(&self.0, seat, data, event, seq);
	}

	fn up(&self, seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, event: &UpEvent, seq: Serial) {
		active_contacts(seat).0.lock().unwrap().remove(&event.slot);
		TouchTarget::up(&self.0, seat, data, event, seq);
	}

	fn motion(
		&self,
		seat: &Seat<MoonshineCompositor>,
		data: &mut MoonshineCompositor,
		event: &MotionEvent,
		seq: Serial,
	) {
		TouchTarget::motion(&self.0, seat, data, event, seq);
	}

	fn frame(&self, seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, seq: Serial) {
		TouchTarget::frame(&self.0, seat, data, seq);
	}

	fn cancel(&self, seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, seq: Serial) {
		active_contacts(seat)
			.0
			.lock()
			.unwrap()
			.retain(|_, (target, _)| !target.same_client_as(&self.0.id()));
		TouchTarget::cancel(&self.0, seat, data, seq);
	}

	fn shape(&self, seat: &Seat<MoonshineCompositor>, data: &mut MoonshineCompositor, event: &ShapeEvent, seq: Serial) {
		TouchTarget::shape(&self.0, seat, data, event, seq);
	}

	fn orientation(
		&self,
		seat: &Seat<MoonshineCompositor>,
		data: &mut MoonshineCompositor,
		event: &OrientationEvent,
		seq: Serial,
	) {
		TouchTarget::orientation(&self.0, seat, data, event, seq);
	}
}

// The seat also participates in Smithay's native and XWayland drag-and-drop
// paths; wrapping touch delivery must preserve their original surface behavior.
impl DndFocus<MoonshineCompositor> for TouchFocusTarget {
	type OfferData<S: Source> = <WlSurface as DndFocus<MoonshineCompositor>>::OfferData<S>;

	fn enter<S: Source>(
		&self,
		data: &mut MoonshineCompositor,
		dh: &DisplayHandle,
		source: Arc<S>,
		seat: &Seat<MoonshineCompositor>,
		location: Point<f64, Logical>,
		serial: &Serial,
	) -> Option<Self::OfferData<S>> {
		DndFocus::enter(&self.0, data, dh, source, seat, location, serial)
	}

	fn motion<S: Source>(
		&self,
		data: &mut MoonshineCompositor,
		offer: Option<&mut Self::OfferData<S>>,
		seat: &Seat<MoonshineCompositor>,
		location: Point<f64, Logical>,
		time: u32,
	) {
		DndFocus::motion(&self.0, data, offer, seat, location, time);
	}

	fn leave<S: Source>(
		&self,
		data: &mut MoonshineCompositor,
		offer: Option<&mut Self::OfferData<S>>,
		seat: &Seat<MoonshineCompositor>,
	) {
		DndFocus::leave(&self.0, data, offer, seat);
	}

	fn drop<S: Source>(
		&self,
		data: &mut MoonshineCompositor,
		offer: Option<&mut Self::OfferData<S>>,
		seat: &Seat<MoonshineCompositor>,
	) {
		DndFocus::drop(&self.0, data, offer, seat);
	}
}
