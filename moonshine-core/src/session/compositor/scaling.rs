//! Output scaling.
//!
//! Scales the composed scene so a window that is not the output size is fitted
//! (and centered) instead of drawn 1:1. The scaler is chosen by the
//! `GAMESCOPE_UPSCALE_SCALER` atom Steam publishes.

/// Scaling mode. Values match the `GAMESCOPE_UPSCALE_SCALER` atom:
/// `AUTO=0, INTEGER=1, FIT=2, FILL=3, STRETCH=4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpscaleScaler {
	/// Fit the source into the output, clamped to the max window scale.
	#[default]
	Auto,
	/// Fit with an integer scale factor, only when upscaling.
	Integer,
	/// Fit (letterbox) with a uniform `min` ratio.
	Fit,
	/// Fill (crop) with a uniform `max` ratio.
	Fill,
	/// Stretch to the output with independent per-axis ratios.
	Stretch,
}

impl UpscaleScaler {
	/// Read `MOONSHINE_UPSCALE_SCALER` (`auto`, `integer`, `fit`, `fill`,
	/// `stretch`), defaulting to [`UpscaleScaler::Auto`].
	pub fn from_env() -> Self {
		match std::env::var("MOONSHINE_UPSCALE_SCALER").ok().as_deref().map(str::trim) {
			Some("integer") => Self::Integer,
			Some("fit") => Self::Fit,
			Some("fill") => Self::Fill,
			Some("stretch") => Self::Stretch,
			Some(value) => value.parse::<u32>().map(Self::from_atom_value).unwrap_or_default(),
			None => Self::Auto,
		}
	}

	/// Map a `GAMESCOPE_UPSCALE_SCALER` atom value.
	pub fn from_atom_value(value: u32) -> Self {
		match value {
			1 => Self::Integer,
			2 => Self::Fit,
			3 => Self::Fill,
			4 => Self::Stretch,
			_ => Self::Auto,
		}
	}
}

/// Scale and centering offset to apply to the composed output, in output
/// pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputScale {
	pub scale_x: f64,
	pub scale_y: f64,
	pub offset_x: f64,
	pub offset_y: f64,
}

impl OutputScale {
	/// Whether the transform is a no-op (source already fills the output).
	pub fn is_identity(&self) -> bool {
		(self.scale_x - 1.0).abs() < f64::EPSILON && (self.scale_y - 1.0).abs() < f64::EPSILON
	}

	/// Compute the scale for a `source` window size within `output`, centering
	/// the scaled result.
	pub fn for_source(
		scaler: UpscaleScaler,
		output_w: f64,
		output_h: f64,
		source_w: f64,
		source_h: f64,
		max_window_scale: f64,
		global_scale: f64,
	) -> Self {
		let (scale_x, scale_y) = calc_scale_factor(
			scaler,
			output_w,
			output_h,
			source_w,
			source_h,
			max_window_scale,
			global_scale,
		);
		Self {
			scale_x,
			scale_y,
			offset_x: (output_w - source_w * scale_x) / 2.0,
			offset_y: (output_h - source_h * scale_y) / 2.0,
		}
	}
}

/// Returns `(scale_x, scale_y)` for a `source` size within `output`.
///
/// `max_window_scale` only applies to [`UpscaleScaler::Auto`]. `global_scale`
/// is the overscan × magnification multiplier (`STEAM_SCREEN_SCALE` ×
/// `STEAM_SCREEN_MAGNIFICATION`), normally 1.0.
pub fn calc_scale_factor(
	scaler: UpscaleScaler,
	output_w: f64,
	output_h: f64,
	source_w: f64,
	source_h: f64,
	max_window_scale: f64,
	global_scale: f64,
) -> (f64, f64) {
	if source_w <= 0.0 || source_h <= 0.0 || output_w <= 0.0 || output_h <= 0.0 {
		return (1.0, 1.0);
	}

	let x_ratio = output_w / source_w;
	let y_ratio = output_h / source_h;

	let (mut scale_x, mut scale_y) = match scaler {
		UpscaleScaler::Stretch => (x_ratio, y_ratio),
		UpscaleScaler::Fill => (x_ratio.max(y_ratio), x_ratio.max(y_ratio)),
		_ => (x_ratio.min(y_ratio), x_ratio.min(y_ratio)),
	};

	if scaler == UpscaleScaler::Auto {
		scale_x = max_window_scale.min(scale_x);
		scale_y = max_window_scale.min(scale_y);
	}

	if scaler == UpscaleScaler::Integer && scale_x > 1.0 {
		// x == y here, so flooring once is enough.
		scale_x = scale_x.floor();
		scale_y = scale_y.floor();
	}

	(scale_x * global_scale, scale_y * global_scale)
}

/// Map a `GAMESCOPE_SCALING_FILTER` atom value to a texture filter.
///
/// `NEAREST` (1) and `PIXEL` (4) select nearest-neighbour sampling; everything
/// else (`LINEAR` 0, `FSR` 2, `NIS` 3, unset) uses linear — the sharpening
/// filters are not implemented.
pub fn filter_from_atom_value(value: u32) -> smithay::backend::renderer::TextureFilter {
	use smithay::backend::renderer::TextureFilter;
	match value {
		1 | 4 => TextureFilter::Nearest,
		_ => TextureFilter::Linear,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn atom_values() {
		assert_eq!(UpscaleScaler::from_atom_value(0), UpscaleScaler::Auto);
		assert_eq!(UpscaleScaler::from_atom_value(1), UpscaleScaler::Integer);
		assert_eq!(UpscaleScaler::from_atom_value(2), UpscaleScaler::Fit);
		assert_eq!(UpscaleScaler::from_atom_value(3), UpscaleScaler::Fill);
		assert_eq!(UpscaleScaler::from_atom_value(4), UpscaleScaler::Stretch);
	}

	#[test]
	fn filter_atom_values() {
		use smithay::backend::renderer::TextureFilter;
		assert_eq!(filter_from_atom_value(0), TextureFilter::Linear);
		assert_eq!(filter_from_atom_value(1), TextureFilter::Nearest);
		assert_eq!(filter_from_atom_value(2), TextureFilter::Linear);
		assert_eq!(filter_from_atom_value(3), TextureFilter::Linear);
		assert_eq!(filter_from_atom_value(4), TextureFilter::Nearest);
	}

	#[test]
	fn fit_letterboxes_and_centers() {
		// 1280x600 window on a 1920x1200 output.
		let scale = OutputScale::for_source(UpscaleScaler::Fit, 1920.0, 1200.0, 1280.0, 600.0, f64::MAX, 1.0);
		assert_eq!((scale.scale_x, scale.scale_y), (1.5, 1.5));
		assert_eq!(scale.offset_x, 0.0);
		assert_eq!(scale.offset_y, 150.0);
	}

	#[test]
	fn fill_uses_max_ratio() {
		let (sx, sy) = calc_scale_factor(UpscaleScaler::Fill, 1920.0, 1200.0, 1280.0, 600.0, f64::MAX, 1.0);
		assert_eq!((sx, sy), (2.0, 2.0));
	}

	#[test]
	fn stretch_uses_independent_ratios() {
		let (sx, sy) = calc_scale_factor(UpscaleScaler::Stretch, 1920.0, 1200.0, 1280.0, 600.0, f64::MAX, 1.0);
		assert_eq!((sx, sy), (1.5, 2.0));
	}

	#[test]
	fn auto_clamps_to_max_scale() {
		let (sx, sy) = calc_scale_factor(UpscaleScaler::Auto, 1920.0, 1200.0, 1280.0, 600.0, 1.25, 1.0);
		assert_eq!((sx, sy), (1.25, 1.25));
	}

	#[test]
	fn integer_floors_only_when_upscaling() {
		let (sx, _) = calc_scale_factor(UpscaleScaler::Integer, 1920.0, 1200.0, 1280.0, 600.0, f64::MAX, 1.0);
		assert_eq!(sx, 1.0);
		let (sx, _) = calc_scale_factor(UpscaleScaler::Integer, 3840.0, 2400.0, 1280.0, 600.0, f64::MAX, 1.0);
		assert_eq!(sx, 3.0);
	}

	#[test]
	fn global_scale_multiplies() {
		let (sx, sy) = calc_scale_factor(UpscaleScaler::Fit, 1920.0, 1200.0, 1920.0, 1200.0, f64::MAX, 1.25);
		assert_eq!((sx, sy), (1.25, 1.25));
	}

	#[test]
	fn identity_when_source_matches_output() {
		let scale = OutputScale::for_source(UpscaleScaler::Auto, 1920.0, 1200.0, 1920.0, 1200.0, f64::MAX, 1.0);
		assert!(scale.is_identity());
		assert_eq!((scale.offset_x, scale.offset_y), (0.0, 0.0));
	}

	#[test]
	fn zero_source_is_identity() {
		let scale = OutputScale::for_source(UpscaleScaler::Fit, 1920.0, 1200.0, 0.0, 0.0, f64::MAX, 1.0);
		assert!(scale.is_identity());
	}
}
