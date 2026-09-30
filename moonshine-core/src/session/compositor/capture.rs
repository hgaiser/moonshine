//! Complete-scene eligibility, independent of application input focus.
use super::CaptureMode;

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
