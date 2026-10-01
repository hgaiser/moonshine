//! Capture lifecycle and complete-scene eligibility, independent of input focus.
use super::CaptureMode;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(super) struct CaptureTick {
	pub render: bool,
	pub released: usize,
}

/// Release completed scanout holds and flush client events before the static
/// screen gate. A client waiting for a reusable image cannot commit damage
/// until its buffer release arrives, even when no new capture is needed.
/// Only the encoder's acquire/release consumption flag permits dropping a hold.
pub(super) fn prepare_capture<K: Eq + Hash, B>(
	held: &mut Vec<(Arc<AtomicBool>, K, B)>,
	indices: &mut HashMap<K, usize>,
	screen_dirty: bool,
	capture_age: Duration,
	flush_releases: impl FnOnce(),
) -> CaptureTick {
	let previous = held.len();
	held.retain(|(consumed, buffer_id, _)| {
		if consumed.load(Ordering::Acquire) {
			indices.remove(buffer_id);
			false
		} else {
			true
		}
	});
	let released = previous - held.len();
	if released != 0 {
		flush_releases();
	}
	CaptureTick {
		render: screen_dirty || capture_age >= Duration::from_secs(1),
		released,
	}
}

#[derive(Default)]
pub(super) struct SceneExtras {
	pub cursor: bool,
	pub steam_overlay: bool,
	pub steam_notification: bool,
	pub external_overlay: bool,
	pub dropdown: bool,
	pub decoration: bool,
	pub scaling: bool,
}

impl SceneExtras {
	pub fn requires_composition(&self, mode: CaptureMode) -> bool {
		mode == CaptureMode::Composited
			|| self.cursor
			|| self.steam_overlay
			|| self.steam_notification
			|| self.external_overlay
			|| self.dropdown
			|| self.decoration
			|| self.scaling
	}
}

pub(super) fn surface_view_is_direct(
	view: smithay::backend::renderer::utils::SurfaceView,
	buffer_scale: i32,
	transform: smithay::utils::Transform,
	output: smithay::utils::Size<i32, smithay::utils::Logical>,
) -> bool {
	buffer_scale == 1
		&& transform == smithay::utils::Transform::Normal
		&& view.offset == (0, 0).into()
		&& view.dst == output
		&& view.src.loc == (0.0, 0.0).into()
		&& view.src.size == output.to_f64()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::compositor::cursor::CursorState;
	use smithay::input::pointer::CursorImageStatus;
	use std::cell::Cell;
	use std::rc::Rc;

	struct BufferHold(Rc<Cell<usize>>);
	impl Drop for BufferHold {
		fn drop(&mut self) {
			self.0.set(self.0.get() + 1);
		}
	}

	#[test]
	fn static_ticks_release_and_flush_completed_three_image_swapchain() {
		let released = Rc::new(Cell::new(0));
		let flags: Vec<_> = (0..3).map(|_| Arc::new(AtomicBool::new(false))).collect();
		let mut held: Vec<_> = flags
			.iter()
			.enumerate()
			.map(|(id, flag)| (flag.clone(), id, BufferHold(released.clone())))
			.collect();
		let mut indices: HashMap<_, _> = (0..3).map(|id| (id, id)).collect();
		// The game has no free image and cannot dirty the screen. Completion
		// occurs asynchronously after capture; no wall-clock wait is needed.
		for (id, flag) in flags.iter().enumerate() {
			flag.store(true, Ordering::Release);
			let tick = prepare_capture(&mut held, &mut indices, false, Duration::from_millis(7), || {
				// Flush must happen AFTER dropping the hold queues wl_buffer.release.
				assert_eq!(released.get(), id + 1);
			});
			assert!(!tick.render);
			assert_eq!(tick.released, 1);
			assert_eq!(held.len(), 2 - id);
			assert!(!indices.contains_key(&id));
			assert_eq!(indices.len(), held.len());
		}
		assert_eq!(released.get(), 3);
	}

	#[test]
	fn static_ticks_preserve_buffers_still_read_by_encoder() {
		let released = Rc::new(Cell::new(0));
		let flag = Arc::new(AtomicBool::new(false));
		let mut held = vec![(flag.clone(), 0, BufferHold(released.clone()))];
		let mut indices = HashMap::from([(0, 0)]);
		for _ in 0..10_000 {
			let tick = prepare_capture(&mut held, &mut indices, false, Duration::from_millis(7), || {
				panic!("unconsumed buffers must not be released or flushed");
			});
			assert!(!tick.render);
			assert_eq!(tick.released, 0);
		}
		assert_eq!(held.len(), 1);
		assert_eq!(indices.len(), 1);
		assert_eq!(released.get(), 0);
	}

	#[test]
	fn dirty_and_keepalive_ticks_still_render_and_release() {
		for (dirty, age) in [(true, Duration::ZERO), (false, Duration::from_secs(1))] {
			let released = Rc::new(Cell::new(0));
			let mut held = vec![(Arc::new(AtomicBool::new(true)), 0, BufferHold(released.clone()))];
			let mut indices = HashMap::from([(0, 0)]);
			let flushed = Cell::new(false);
			let tick = prepare_capture(&mut held, &mut indices, dirty, age, || flushed.set(true));
			assert!(tick.render);
			assert_eq!(tick.released, 1);
			assert_eq!(released.get(), 1);
			assert!(flushed.get());
			assert!(held.is_empty() && indices.is_empty());
		}
	}

	#[test]
	fn cropping_scaling_offsets_and_rotation_require_composition() {
		use smithay::backend::renderer::utils::SurfaceView;
		use smithay::utils::{Rectangle, Transform};
		let output = (1920, 1080).into();
		let view = SurfaceView {
			src: Rectangle::from_size((1920.0, 1080.0).into()),
			dst: output,
			offset: (0, 0).into(),
		};
		assert!(surface_view_is_direct(view, 1, Transform::Normal, output));
		assert!(!surface_view_is_direct(view, 2, Transform::Normal, output));
		assert!(!surface_view_is_direct(view, 1, Transform::_90, output));
		assert!(!surface_view_is_direct(
			SurfaceView {
				offset: (1, 0).into(),
				..view
			},
			1,
			Transform::Normal,
			output
		));
		assert!(!surface_view_is_direct(
			SurfaceView {
				dst: (1280, 720).into(),
				..view
			},
			1,
			Transform::Normal,
			output
		));
		assert!(!surface_view_is_direct(
			SurfaceView {
				src: Rectangle::from_size((3840.0, 2160.0).into()),
				..view
			},
			1,
			Transform::Normal,
			output
		));
	}

	#[test]
	fn configuration_defaults_and_only_complete_scene_modes() {
		let config: super::super::CompositorConfig = toml::from_str("").unwrap();
		assert_eq!(config.capture_mode, CaptureMode::Auto);
		let config: super::super::CompositorConfig = toml::from_str("capture_mode = \"composited\"").unwrap();
		assert_eq!(config.capture_mode, CaptureMode::Composited);
		assert!(toml::from_str::<super::super::CompositorConfig>("capture_mode = \"direct\"").is_err());
	}

	#[test]
	fn fullscreen_scene_can_resume_direct_after_each_extra_disappears() {
		let mut scene = SceneExtras::default();
		assert!(!scene.requires_composition(CaptureMode::Auto));
		for field in [0, 1, 2, 3, 4, 5, 6] {
			match field {
				0 => scene.cursor = true,
				1 => scene.steam_overlay = true,
				2 => scene.steam_notification = true,
				3 => scene.external_overlay = true,
				4 => scene.dropdown = true,
				5 => scene.decoration = true,
				_ => scene.scaling = true,
			}
			assert!(scene.requires_composition(CaptureMode::Auto));
			scene = SceneExtras::default();
			assert!(!scene.requires_composition(CaptureMode::Auto));
		}
		assert!(scene.requires_composition(CaptureMode::Composited));
	}
	#[test]
	fn cursor_intent_drives_scanout_without_an_idle_timeout() {
		let mut cursor = CursorState::default();
		assert!(
			!SceneExtras {
				cursor: cursor.visible(),
				..Default::default()
			}
			.requires_composition(CaptureMode::Auto)
		);
		cursor.activate_pointer();
		assert!(
			SceneExtras {
				cursor: cursor.visible(),
				..Default::default()
			}
			.requires_composition(CaptureMode::Auto)
		);
		cursor.set_image(CursorImageStatus::Hidden);
		assert!(
			!SceneExtras {
				cursor: cursor.visible(),
				..Default::default()
			}
			.requires_composition(CaptureMode::Auto)
		);
	}
}
