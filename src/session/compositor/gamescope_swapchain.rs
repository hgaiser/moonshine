//! Minimal `gamescope_swapchain_factory_v2` / `gamescope_swapchain` protocol
//! support for HDR passthrough without running a nested gamescope instance.
//!
//! The gamescope WSI Vulkan layer uses this private protocol to communicate
//! swapchain metadata (color space, HDR info) to the compositor.  By
//! implementing it here, games using the WSI layer can declare HDR content
//! directly to Moonshine.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::color_management::{ImageDescription, Primaries, TransferFunction};
use super::protocols::gamescope_swapchain::GamescopeSwapchain;
use super::protocols::gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2;
use super::state::MoonshineCompositor;

/// VK_COLOR_SPACE_HDR10_ST2084_EXT
const VK_COLOR_SPACE_HDR10_ST2084_EXT: u32 = 1000104008;

// ---------------------------------------------------------------------------
// User data
// ---------------------------------------------------------------------------

/// User data for `gamescope_swapchain_factory_v2`.
pub struct SwapchainFactoryData;

/// User data for `gamescope_swapchain`.
pub struct SwapchainData {
	pub surface: WlSurface,
}

// ---------------------------------------------------------------------------
// gamescope_swapchain_factory_v2 — Global
// ---------------------------------------------------------------------------

impl GlobalDispatch<GamescopeSwapchainFactoryV2, ()> for MoonshineCompositor {
	fn bind(
		_state: &mut Self,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<GamescopeSwapchainFactoryV2>,
		_global_data: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		tracing::debug!("gamescope_swapchain_factory_v2 bound");
		data_init.init(resource, SwapchainFactoryData);
	}
}

// ---------------------------------------------------------------------------
// gamescope_swapchain_factory_v2 — Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<GamescopeSwapchainFactoryV2, SwapchainFactoryData> for MoonshineCompositor {
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &GamescopeSwapchainFactoryV2,
		request: <GamescopeSwapchainFactoryV2 as Resource>::Request,
		_data: &SwapchainFactoryData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		use super::protocols::gamescope_swapchain_factory_v2::Request;
		match request {
			Request::CreateSwapchain { surface, callback } => {
				tracing::debug!("gamescope_swapchain_factory_v2::create_swapchain");
				data_init.init(callback, SwapchainData { surface });
			},
			Request::Destroy => {},
		}
	}
}

// ---------------------------------------------------------------------------
// gamescope_swapchain — Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<GamescopeSwapchain, SwapchainData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &GamescopeSwapchain,
		request: <GamescopeSwapchain as Resource>::Request,
		data: &SwapchainData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		use super::protocols::gamescope_swapchain::Request;
		match request {
			Request::SwapchainFeedback {
				vk_colorspace,
				vk_format,
				..
			} => {
				tracing::debug!(vk_colorspace, vk_format, "gamescope_swapchain::swapchain_feedback");

				if let Some(cm) = &mut state.color_management {
					if vk_colorspace == VK_COLOR_SPACE_HDR10_ST2084_EXT {
						tracing::debug!("Setting surface to BT.2020 + PQ via gamescope swapchain feedback");
						cm.set_pending(&data.surface, ImageDescription::bt2020_pq());
					} else {
						tracing::debug!("Setting surface to sRGB via gamescope swapchain feedback");
						cm.set_pending(&data.surface, ImageDescription::srgb());
					}
				}
			},
			Request::OverrideWindowContent {
				gamescope_xwayland_server_id: _,
				x11_window,
			} => {
				tracing::debug!(x11_window, "gamescope_swapchain::override_window_content");
				// The WSI layer has created a separate wl_surface to bypass
				// XWayland rendering.  We need to tell the compositor to use
				// this surface for the X11 window.
				//
				// For Moonshine's single-window-fullscreen model, we map the
				// override surface in the space the same way we'd map any
				// toplevel, so it appears as the game's visible surface.
				state.override_window_surface(x11_window, data.surface.clone());
			},
			Request::SetHdrMetadata {
				display_primary_red_x,
				display_primary_red_y,
				display_primary_green_x,
				display_primary_green_y,
				display_primary_blue_x,
				display_primary_blue_y,
				white_point_x,
				white_point_y,
				max_display_mastering_luminance,
				min_display_mastering_luminance,
				max_cll,
				max_fall,
			} => {
				tracing::debug!(
					max_cll,
					max_fall,
					max_display_mastering_luminance,
					min_display_mastering_luminance,
					"gamescope_swapchain::set_hdr_metadata"
				);

				if let Some(cm) = &mut state.color_management {
					let desc = ImageDescription {
						transfer_function: TransferFunction::St2084Pq,
						primaries: Primaries::Bt2020,
						max_cll: Some(max_cll),
						max_fall: Some(max_fall),
						// max is in 1 cd/m², min is in 0.0001 cd/m²; normalize both to 0.0001 cd/m² units.
						mastering_luminance: Some((
							min_display_mastering_luminance,
							max_display_mastering_luminance.saturating_mul(10000),
						)),
						mastering_primaries: Some([
							(display_primary_red_x, display_primary_red_y),
							(display_primary_green_x, display_primary_green_y),
							(display_primary_blue_x, display_primary_blue_y),
						]),
						white_point: Some((white_point_x, white_point_y)),
					};
					cm.set_pending(&data.surface, desc);
				}
			},
			Request::SetPresentMode { .. } | Request::SetPresentTime { .. } | Request::Destroy => {},
		}
	}
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Registers the gamescope swapchain factory global.
pub fn register_globals(display: &DisplayHandle) {
	display.create_global::<MoonshineCompositor, GamescopeSwapchainFactoryV2, _>(1, ());
}
