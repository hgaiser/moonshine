//! Minimal `wp_color_management_v1` and `wp_color_representation_v1` protocol
//! support for HDR passthrough.
//!
//! Since Moonshine always has a single fullscreen surface, full color-managed
//! compositing is not needed.  The compositor only needs to:
//!
//! 1. Advertise the protocol so applications can declare their color space.
//! 2. Track which color space the fullscreen surface is using.
//! 3. Pass through the pixel data unmodified (no color conversion).
//! 4. Tag the exported frame with the correct `FrameColorSpace`.
//!
//! The protocol handling code is implemented out-of-tree since Smithay 0.7.0
//! does not include it.  The protocol bindings come from the
//! `wayland-protocols` crate (staging feature).

use std::collections::HashMap;
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
	wp_color_management_output_v1, wp_color_management_surface_feedback_v1, wp_color_management_surface_v1,
	wp_color_manager_v1, wp_image_description_creator_icc_v1, wp_image_description_creator_params_v1,
	wp_image_description_info_v1, wp_image_description_v1,
};
use smithay::reexports::wayland_protocols::wp::color_representation::v1::server::{
	wp_color_representation_manager_v1, wp_color_representation_surface_v1,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::frame::{FrameColorSpace, HdrMetadata};
use super::state::MoonshineCompositor;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Transfer function as declared by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunction {
	Gamma22,
	St2084Pq,
}

/// Color primaries as declared by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primaries {
	Srgb,
	Bt2020,
}

/// A resolved image description created from parametric parameters.
#[derive(Debug, Clone, Copy)]
pub struct ImageDescription {
	pub transfer_function: TransferFunction,
	pub primaries: Primaries,
	/// Maximum content light level in cd/m² (nits), if declared.
	pub max_cll: Option<u32>,
	/// Maximum frame-average light level in cd/m² (nits), if declared.
	pub max_fall: Option<u32>,
	/// Mastering display luminance (min, max) in 0.0001 cd/m² units, if declared.
	pub mastering_luminance: Option<(u32, u32)>,
	/// Mastering display primaries [(Rx,Ry), (Gx,Gy), (Bx,By)] in 0.00002 units.
	pub mastering_primaries: Option<[(u32, u32); 3]>,
	/// White point (x, y) in 0.00002 units.
	pub white_point: Option<(u32, u32)>,
}

impl ImageDescription {
	pub fn srgb() -> Self {
		Self {
			transfer_function: TransferFunction::Gamma22,
			primaries: Primaries::Srgb,
			max_cll: None,
			max_fall: None,
			mastering_luminance: None,
			mastering_primaries: None,
			white_point: None,
		}
	}

	pub fn bt2020_pq() -> Self {
		Self {
			transfer_function: TransferFunction::St2084Pq,
			primaries: Primaries::Bt2020,
			max_cll: None,
			max_fall: None,
			mastering_luminance: None,
			mastering_primaries: None,
			white_point: None,
		}
	}

	pub fn to_frame_color_space(self) -> FrameColorSpace {
		if self.primaries == Primaries::Bt2020 && self.transfer_function == TransferFunction::St2084Pq {
			FrameColorSpace::Bt2020Pq
		} else {
			FrameColorSpace::Srgb
		}
	}
}

/// Builder state while creating a parametric image description.
#[derive(Debug, Default)]
pub(crate) struct CreatorParams {
	transfer_function: Option<TransferFunction>,
	primaries: Option<Primaries>,
	max_cll: Option<u32>,
	max_fall: Option<u32>,
	mastering_luminance: Option<(u32, u32)>,
	/// Mastering display primaries [(Rx,Ry), (Gx,Gy), (Bx,By)] in 0.00002 units.
	mastering_primaries: Option<[(u32, u32); 3]>,
	/// White point (x, y) in 0.00002 units.
	white_point: Option<(u32, u32)>,
}

// ---------------------------------------------------------------------------
// User-data types attached to protocol resources
// ---------------------------------------------------------------------------

/// User data for `wp_color_management_surface_v1`.
pub struct ColorSurfaceData {
	pub surface: WlSurface,
}

/// User data for `wp_image_description_v1`.
pub struct ImageDescriptionUserData {
	pub desc: ImageDescription,
}

/// User data for `wp_image_description_creator_params_v1`.
pub struct CreatorParamsUserData {
	pub params: Mutex<CreatorParams>,
}

/// User data for `wp_color_management_output_v1` (minimal).
pub struct ColorOutputData;

/// User data for `wp_color_management_surface_feedback_v1` (minimal).
pub struct ColorSurfaceFeedbackData {
	#[allow(dead_code)]
	pub surface: WlSurface,
}

/// User data for `wp_image_description_info_v1`.
pub struct ImageDescriptionInfoData;

/// User data for `wp_image_description_creator_icc_v1`.
pub struct IccCreatorData;

/// User data for `wp_color_representation_surface_v1`.
pub struct ColorRepresentationSurfaceData {
	#[allow(dead_code)]
	pub surface: WlSurface,
}

// ---------------------------------------------------------------------------
// Compositor-level color management state
// ---------------------------------------------------------------------------

/// Tracks per-surface color space declarations.
pub struct ColorManagementState {
	/// Pending image description per surface (applied on next commit).
	pending: HashMap<WlSurface, Option<ImageDescription>>,
	/// Current (committed) image description per surface.
	current: HashMap<WlSurface, ImageDescription>,
	/// Pending image description per surface from gamescope_swapchain (takes priority).
	gamescope_pending: HashMap<WlSurface, Option<ImageDescription>>,
	/// Current (committed) image description per surface from gamescope_swapchain.
	gamescope_current: HashMap<WlSurface, ImageDescription>,
	/// Whether HDR mode was negotiated with the Moonlight client.
	pub hdr: bool,
}

impl ColorManagementState {
	/// Create a new state and register the protocol globals.
	pub fn new(display: &DisplayHandle, hdr: bool) -> Self {
		// Advertise wp_color_manager_v1 (protocol version 1).
		display.create_global::<MoonshineCompositor, wp_color_manager_v1::WpColorManagerV1, _>(1, ());
		// Advertise wp_color_representation_manager_v1 (protocol version 1).
		display
			.create_global::<MoonshineCompositor, wp_color_representation_manager_v1::WpColorRepresentationManagerV1, _>(
				1,
				(),
			);

		Self {
			pending: HashMap::new(),
			current: HashMap::new(),
			gamescope_pending: HashMap::new(),
			gamescope_current: HashMap::new(),
			hdr,
		}
	}

	/// Set a pending image description for a surface (from `set_image_description`).
	pub fn set_pending(&mut self, surface: &WlSurface, desc: ImageDescription) {
		tracing::debug!(
			surface_id = ?surface.id(),
			color_space = ?desc.to_frame_color_space(),
			"set_pending"
		);
		self.pending.insert(surface.clone(), Some(desc));
	}

	/// Set a pending image description for a surface from gamescope_swapchain.
	/// This takes priority over wp_color_management declarations.
	pub fn set_gamescope_pending(&mut self, surface: &WlSurface, desc: ImageDescription) {
		tracing::debug!(
			surface_id = ?surface.id(),
			color_space = ?desc.to_frame_color_space(),
			"set_gamescope_pending (takes priority)"
		);
		self.gamescope_pending.insert(surface.clone(), Some(desc));
	}

	/// Clear the pending image description (from `unset_image_description`).
	pub fn unset_pending(&mut self, surface: &WlSurface) {
		self.pending.insert(surface.clone(), None);
	}

	/// Apply pending state on surface commit.
	pub fn commit(&mut self, surface: &WlSurface) {
		// Apply gamescope_swapchain pending state first.
		if let Some(pending) = self.gamescope_pending.remove(surface) {
			match pending {
				Some(desc) => {
					tracing::debug!(
						surface_id = ?surface.id(),
						color_space = ?desc.to_frame_color_space(),
						"commit: inserting gamescope color space into current"
					);
					self.gamescope_current.insert(surface.clone(), desc);
				},
				None => {
					tracing::debug!(
						surface_id = ?surface.id(),
						"commit: removing gamescope color space from current"
					);
					self.gamescope_current.remove(surface);
				},
			}
		}

		// Apply wp_color_management pending state.
		if let Some(pending) = self.pending.remove(surface) {
			match pending {
				Some(desc) => {
					tracing::debug!(
						surface_id = ?surface.id(),
						color_space = ?desc.to_frame_color_space(),
						num_current = self.current.len(),
						"commit: inserting into current"
					);
					self.current.insert(surface.clone(), desc);
				},
				None => {
					tracing::debug!(
						surface_id = ?surface.id(),
						"commit: removing from current"
					);
					self.current.remove(surface);
				},
			}
		}
	}

	/// Get the frame color space for the current fullscreen surface.
	///
	/// Returns `Bt2020Pq` if any mapped surface has declared BT.2020+PQ,
	/// otherwise returns `Srgb`.
	///
	/// Gamescope swapchain color space declarations take priority over
	/// wp_color_management declarations.
	pub fn frame_color_space(&self) -> FrameColorSpace {
		// Check gamescope_swapchain color spaces first (higher priority).
		for (surface, desc) in &self.gamescope_current {
			if desc.to_frame_color_space() == FrameColorSpace::Bt2020Pq {
				tracing::trace!(
					surface_id = ?surface.id(),
					num_gamescope_current = self.gamescope_current.len(),
					"frame_color_space: Bt2020Pq (from gamescope_swapchain)"
				);
				return FrameColorSpace::Bt2020Pq;
			}
		}
		// Fall back to wp_color_management color spaces.
		for (surface, desc) in &self.current {
			if desc.to_frame_color_space() == FrameColorSpace::Bt2020Pq {
				tracing::trace!(
					surface_id = ?surface.id(),
					num_current = self.current.len(),
					"frame_color_space: Bt2020Pq"
				);
				return FrameColorSpace::Bt2020Pq;
			}
		}
		FrameColorSpace::Srgb
	}

	/// Get HDR metadata from the current fullscreen surface, if any.
	///
	/// Returns `Some(HdrMetadata)` when a surface has declared BT.2020+PQ
	/// with max_cll or max_fall values.
	///
	/// Gamescope swapchain color space declarations take priority over
	/// wp_color_management declarations.
	pub fn hdr_metadata(&self) -> Option<HdrMetadata> {
		let sat = |v: u32| -> u16 { u16::try_from(v).unwrap_or(u16::MAX) };

		// Check gamescope_swapchain metadata first (higher priority).
		for desc in self.gamescope_current.values() {
			if desc.to_frame_color_space() != FrameColorSpace::Bt2020Pq {
				continue;
			}
			if desc.max_cll.is_none() && desc.max_fall.is_none() && desc.mastering_luminance.is_none() {
				continue;
			}
			return Some(HdrMetadata {
				display_primaries: desc.mastering_primaries.map_or([(0, 0); 3], |p| {
					[
						(sat(p[0].0), sat(p[0].1)),
						(sat(p[1].0), sat(p[1].1)),
						(sat(p[2].0), sat(p[2].1)),
					]
				}),
				white_point: desc.white_point.map_or((0, 0), |(x, y)| (sat(x), sat(y))),
				max_luminance: desc.mastering_luminance.map_or(0, |(_, max)| max),
				min_luminance: desc.mastering_luminance.map_or(0, |(min, _)| min),
				max_cll: sat(desc.max_cll.unwrap_or(0)),
				max_fall: sat(desc.max_fall.unwrap_or(0)),
			});
		}

		// Fall back to wp_color_management metadata.
		for desc in self.current.values() {
			if desc.to_frame_color_space() != FrameColorSpace::Bt2020Pq {
				continue;
			}
			if desc.max_cll.is_none() && desc.max_fall.is_none() && desc.mastering_luminance.is_none() {
				continue;
			}
			return Some(HdrMetadata {
				display_primaries: desc.mastering_primaries.map_or([(0, 0); 3], |p| {
					[
						(sat(p[0].0), sat(p[0].1)),
						(sat(p[1].0), sat(p[1].1)),
						(sat(p[2].0), sat(p[2].1)),
					]
				}),
				white_point: desc.white_point.map_or((0, 0), |(x, y)| (sat(x), sat(y))),
				max_luminance: desc.mastering_luminance.map_or(0, |(_, max)| max),
				min_luminance: desc.mastering_luminance.map_or(0, |(min, _)| min),
				max_cll: sat(desc.max_cll.unwrap_or(0)),
				max_fall: sat(desc.max_fall.unwrap_or(0)),
			});
		}
		None
	}

	/// Clean up tracking for a destroyed surface.
	pub fn surface_destroyed(&mut self, surface: &WlSurface) {
		self.pending.remove(surface);
		self.current.remove(surface);
		self.gamescope_pending.remove(surface);
		self.gamescope_current.remove(surface);
	}
}

// ---------------------------------------------------------------------------
// wp_color_manager_v1 — Global
// ---------------------------------------------------------------------------

impl GlobalDispatch<wp_color_manager_v1::WpColorManagerV1, ()> for MoonshineCompositor {
	fn bind(
		_state: &mut Self,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<wp_color_manager_v1::WpColorManagerV1>,
		_global_data: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		tracing::debug!("wp_color_manager_v1: client bound global");
		let resource = data_init.init(resource, ());

		// Advertise supported capabilities.
		resource.supported_intent(wp_color_manager_v1::RenderIntent::Perceptual);
		resource.supported_feature(wp_color_manager_v1::Feature::Parametric);
		resource.supported_feature(wp_color_manager_v1::Feature::SetPrimaries);
		resource.supported_feature(wp_color_manager_v1::Feature::SetMasteringDisplayPrimaries);
		resource.supported_feature(wp_color_manager_v1::Feature::ExtendedTargetVolume);
		resource.supported_feature(wp_color_manager_v1::Feature::SetLuminances);
		resource.supported_feature(wp_color_manager_v1::Feature::WindowsScrgb);
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::Srgb);
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::Gamma22);
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::St2084Pq);
		resource.supported_primaries_named(wp_color_manager_v1::Primaries::Srgb);
		resource.supported_primaries_named(wp_color_manager_v1::Primaries::Bt2020);
		resource.done();
	}
}

impl Dispatch<wp_color_manager_v1::WpColorManagerV1, ()> for MoonshineCompositor {
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_color_manager_v1::WpColorManagerV1,
		request: wp_color_manager_v1::Request,
		_data: &(),
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		tracing::debug!(?request, "wp_color_manager_v1 request");
		match request {
			wp_color_manager_v1::Request::Destroy => {},

			wp_color_manager_v1::Request::GetSurface { id, surface } => {
				data_init.init(id, ColorSurfaceData { surface });
			},

			wp_color_manager_v1::Request::GetOutput { id, .. } => {
				data_init.init(id, ColorOutputData);
			},

			wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
				data_init.init(id, ColorSurfaceFeedbackData { surface });
			},

			wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
				data_init.init(
					obj,
					CreatorParamsUserData {
						params: Mutex::new(CreatorParams::default()),
					},
				);
			},

			wp_color_manager_v1::Request::CreateIccCreator { obj } => {
				data_init.init(obj, IccCreatorData);
			},

			wp_color_manager_v1::Request::CreateWindowsScrgb { image_description } => {
				// Windows scRGB officially means BT.709 primaries + extended linear,
				// but Proton/DXVK's gamescope WSI layer converts the scRGB surface data
				// to BT.2020+PQ before submitting the buffer. We therefore map it to
				// Bt2020Pq so the encoder treats the content as passthrough HDR rather
				// than applying an unnecessary sRGB→BT.2020+PQ conversion.
				let desc = ImageDescription::bt2020_pq();
				let resource = data_init.init(image_description, ImageDescriptionUserData { desc });
				resource.ready(0);
			},

			_ => {
				tracing::debug!("Unhandled wp_color_manager_v1 request");
			},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_management_surface_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_management_surface_v1::WpColorManagementSurfaceV1, ColorSurfaceData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_management_surface_v1::WpColorManagementSurfaceV1,
		request: wp_color_management_surface_v1::Request,
		data: &ColorSurfaceData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_management_surface_v1::Request::Destroy => {},

			wp_color_management_surface_v1::Request::SetImageDescription {
				image_description,
				render_intent: _,
			} => {
				if let Some(desc_data) = image_description.data::<ImageDescriptionUserData>() {
					tracing::debug!(
						?desc_data.desc,
						"Surface set image description"
					);
					if let Some(cm) = &mut state.color_management {
						cm.set_pending(&data.surface, desc_data.desc);
					}
				}
			},

			wp_color_management_surface_v1::Request::UnsetImageDescription => {
				tracing::debug!("Surface unset image description");
				if let Some(cm) = &mut state.color_management {
					cm.unset_pending(&data.surface);
				}
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_creator_params_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1, CreatorParamsUserData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
		request: wp_image_description_creator_params_v1::Request,
		data: &CreatorParamsUserData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_image_description_creator_params_v1::Request::Create { image_description } => {
				let params = data.params.lock().unwrap();
				let desc = ImageDescription {
					transfer_function: params.transfer_function.unwrap_or(TransferFunction::Gamma22),
					primaries: params.primaries.unwrap_or(Primaries::Srgb),
					max_cll: params.max_cll,
					max_fall: params.max_fall,
					mastering_luminance: params.mastering_luminance,
					mastering_primaries: params.mastering_primaries,
					white_point: params.white_point,
				};
				tracing::debug!(?desc, "Created parametric image description");

				let resource = data_init.init(image_description, ImageDescriptionUserData { desc });
				// Signal that the image description is ready.
				resource.ready(0);
			},

			wp_image_description_creator_params_v1::Request::SetTfNamed { tf } => {
				let tf = match tf.into_result() {
					Ok(wp_color_manager_v1::TransferFunction::St2084Pq) => TransferFunction::St2084Pq,
					Ok(wp_color_manager_v1::TransferFunction::Gamma22) => TransferFunction::Gamma22,
					other => {
						tracing::debug!(?other, "Unsupported transfer function, defaulting to gamma22");
						TransferFunction::Gamma22
					},
				};
				data.params.lock().unwrap().transfer_function = Some(tf);
			},

			wp_image_description_creator_params_v1::Request::SetPrimariesNamed { primaries } => {
				let p = match primaries.into_result() {
					Ok(wp_color_manager_v1::Primaries::Bt2020) => Primaries::Bt2020,
					Ok(wp_color_manager_v1::Primaries::Srgb) => Primaries::Srgb,
					other => {
						tracing::debug!(?other, "Unsupported primaries, defaulting to sRGB");
						Primaries::Srgb
					},
				};
				data.params.lock().unwrap().primaries = Some(p);
			},

			wp_image_description_creator_params_v1::Request::SetMaxCll { max_cll } => {
				tracing::debug!(max_cll, "Set max content light level");
				data.params.lock().unwrap().max_cll = Some(max_cll);
			},

			wp_image_description_creator_params_v1::Request::SetMaxFall { max_fall } => {
				tracing::debug!(max_fall, "Set max frame-average light level");
				data.params.lock().unwrap().max_fall = Some(max_fall);
			},

			wp_image_description_creator_params_v1::Request::SetMasteringLuminance { min_lum, max_lum } => {
				tracing::debug!(min_lum, max_lum, "Set mastering luminance");
				// min_lum is in 0.0001 cd/m² units; max_lum is in 1 cd/m² units.
				// Normalize both to 0.0001 cd/m² units.
				data.params.lock().unwrap().mastering_luminance = Some((min_lum, max_lum.saturating_mul(10000)));
			},

			wp_image_description_creator_params_v1::Request::SetMasteringDisplayPrimaries {
				r_x,
				r_y,
				g_x,
				g_y,
				b_x,
				b_y,
				w_x,
				w_y,
			} => {
				// Protocol sends primaries as 1/1,000,000 chromaticity (i32).
				// Convert to 0.00002 units (divide by 20) to match CTA-861.G format.
				let to_cta = |v: i32| -> u32 { (v.max(0) as u32) / 20 };
				let mut params = data.params.lock().unwrap();
				params.mastering_primaries = Some([
					(to_cta(r_x), to_cta(r_y)),
					(to_cta(g_x), to_cta(g_y)),
					(to_cta(b_x), to_cta(b_y)),
				]);
				params.white_point = Some((to_cta(w_x), to_cta(w_y)));
				tracing::debug!(
					r_x,
					r_y,
					g_x,
					g_y,
					b_x,
					b_y,
					w_x,
					w_y,
					"Set mastering display primaries"
				);
			},

			wp_image_description_creator_params_v1::Request::SetTfPower { .. }
			| wp_image_description_creator_params_v1::Request::SetPrimaries { .. }
			| wp_image_description_creator_params_v1::Request::SetLuminances { .. } => {
				tracing::trace!("Ignoring advanced parametric creator parameter");
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_v1::WpImageDescriptionV1, ImageDescriptionUserData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_v1::WpImageDescriptionV1,
		request: wp_image_description_v1::Request,
		data: &ImageDescriptionUserData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_image_description_v1::Request::Destroy => {},

			wp_image_description_v1::Request::GetInformation { information } => {
				tracing::debug!(?data.desc, "GetInformation: sending image description info");
				let info = data_init.init(information, ImageDescriptionInfoData);

				// Send parametric description events.
				match data.desc.primaries {
					Primaries::Srgb => info.primaries_named(wp_color_manager_v1::Primaries::Srgb),
					Primaries::Bt2020 => info.primaries_named(wp_color_manager_v1::Primaries::Bt2020),
				}
				match data.desc.transfer_function {
					TransferFunction::Gamma22 => {
						info.tf_named(wp_color_manager_v1::TransferFunction::Gamma22);
						// sRGB: 0.2–80 cd/m², reference white 80 cd/m².
						info.luminances(2000, 80, 80);
						info.target_luminance(2000, 80);
					},
					TransferFunction::St2084Pq => {
						info.tf_named(wp_color_manager_v1::TransferFunction::St2084Pq);
						// PQ: 0–10000 cd/m², SDR reference white 203 cd/m².
						info.luminances(0, 10000, 203);
						info.target_luminance(0, 10000);
					},
				}

				// done() is a destructor event that removes the child object from
				// the backend's map. Calling it here would panic because the
				// backend tries to set user_data on the deleted object after this
				// handler returns. Defer to after dispatch_clients.
				state.deferred_info_done.push(info);
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_info_v1 — no client requests (events only)
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_info_v1::WpImageDescriptionInfoV1, ImageDescriptionInfoData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_info_v1::WpImageDescriptionInfoV1,
		_request: wp_image_description_info_v1::Request,
		_data: &ImageDescriptionInfoData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		// wp_image_description_info_v1 has no client requests.
	}
}

// ---------------------------------------------------------------------------
// wp_color_management_output_v1 — minimal implementation
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_management_output_v1::WpColorManagementOutputV1, ColorOutputData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_management_output_v1::WpColorManagementOutputV1,
		request: wp_color_management_output_v1::Request,
		_data: &ColorOutputData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_management_output_v1::Request::Destroy => {},

			wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
				// Return a BT.2020+PQ description if HDR is active, otherwise sRGB.
				let desc = if state.color_management.as_ref().is_some_and(|cm| cm.hdr) {
					ImageDescription::bt2020_pq()
				} else {
					ImageDescription::srgb()
				};
				let resource = data_init.init(image_description, ImageDescriptionUserData { desc });
				resource.ready(0);
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_management_surface_feedback_v1 — minimal implementation
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1, ColorSurfaceFeedbackData>
	for MoonshineCompositor
{
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
		request: wp_color_management_surface_feedback_v1::Request,
		_data: &ColorSurfaceFeedbackData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_management_surface_feedback_v1::Request::Destroy => {},

			wp_color_management_surface_feedback_v1::Request::GetPreferred { image_description } => {
				// Preferred: BT.2020+PQ when HDR is active.
				let desc = if state.color_management.as_ref().is_some_and(|cm| cm.hdr) {
					ImageDescription::bt2020_pq()
				} else {
					ImageDescription::srgb()
				};
				tracing::debug!(?desc, "GetPreferred: returning image description");
				let resource = data_init.init(image_description, ImageDescriptionUserData { desc });
				resource.ready(0);
			},

			wp_color_management_surface_feedback_v1::Request::GetPreferredParametric { image_description } => {
				// Same as get_preferred for our simple case.
				let desc = if state.color_management.as_ref().is_some_and(|cm| cm.hdr) {
					ImageDescription::bt2020_pq()
				} else {
					ImageDescription::srgb()
				};
				let resource = data_init.init(image_description, ImageDescriptionUserData { desc });
				resource.ready(0);
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_creator_icc_v1 — stub (not supported)
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1, IccCreatorData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
		request: wp_image_description_creator_icc_v1::Request,
		_data: &IccCreatorData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_image_description_creator_icc_v1::Request::Create { image_description } => {
				// ICC profiles are not supported. Create a default sRGB description
				// and signal failure with the `failed` event.
				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc: ImageDescription::srgb(),
					},
				);
				resource.failed(
					wp_image_description_v1::Cause::Unsupported,
					"ICC profiles are not supported".to_string(),
				);
			},

			_ => {
				// set_icc_file — accept but ignore.
			},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_representation_manager_v1 — Global
// ---------------------------------------------------------------------------

impl GlobalDispatch<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, ()> for MoonshineCompositor {
	fn bind(
		_state: &mut Self,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<wp_color_representation_manager_v1::WpColorRepresentationManagerV1>,
		_global_data: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		let resource = data_init.init(resource, ());

		// Advertise supported alpha modes.
		resource.supported_alpha_mode(wp_color_representation_surface_v1::AlphaMode::PremultipliedElectrical);
		resource.supported_alpha_mode(wp_color_representation_surface_v1::AlphaMode::Straight);

		// Advertise identity coefficients (RGB) with full range.
		resource.supported_coefficients_and_ranges(
			wp_color_representation_surface_v1::Coefficients::Identity,
			wp_color_representation_surface_v1::Range::Full,
		);

		resource.done();
	}
}

impl Dispatch<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, ()> for MoonshineCompositor {
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
		request: wp_color_representation_manager_v1::Request,
		_data: &(),
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_representation_manager_v1::Request::Destroy => {},

			wp_color_representation_manager_v1::Request::GetSurface { id, surface } => {
				data_init.init(id, ColorRepresentationSurfaceData { surface });
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_representation_surface_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_representation_surface_v1::WpColorRepresentationSurfaceV1, ColorRepresentationSurfaceData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_color_representation_surface_v1::WpColorRepresentationSurfaceV1,
		request: wp_color_representation_surface_v1::Request,
		_data: &ColorRepresentationSurfaceData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_representation_surface_v1::Request::Destroy => {},

			wp_color_representation_surface_v1::Request::SetAlphaMode { alpha_mode } => {
				tracing::debug!(?alpha_mode, "Surface set alpha mode");
				// Accept but don't act — single fullscreen passthrough.
			},

			wp_color_representation_surface_v1::Request::SetCoefficientsAndRange { coefficients, range } => {
				tracing::debug!(?coefficients, ?range, "Surface set coefficients and range");
				// Accept but don't act — we only support identity/full range RGB.
			},

			wp_color_representation_surface_v1::Request::SetChromaLocation { chroma_location } => {
				tracing::debug!(?chroma_location, "Surface set chroma location");
				// Accept but don't act — passthrough.
			},

			_ => {},
		}
	}
}
