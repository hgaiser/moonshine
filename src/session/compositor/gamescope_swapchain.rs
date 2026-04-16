//! Swapchain protocol support for HDR passthrough.
//!
//! Implements both the `gamescope_swapchain_factory_v2` / `gamescope_swapchain`
//! protocol (for DXVK compatibility) and the `moonshine_swapchain_factory_v2` /
//! `moonshine_swapchain` protocol (for the native moonshine-wsi Vulkan layer).
//!
//! The WSI Vulkan layer uses this private protocol to communicate swapchain
//! metadata (color space, HDR info) to the compositor.  By implementing it
//! here, games using the WSI layer can declare HDR content directly to
//! Moonshine.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::color_management::{ImageDescription, Primaries, TransferFunction};
use super::protocols::gamescope_swapchain::GamescopeSwapchain;
use super::protocols::gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2;
use super::protocols::moonshine_swapchain::MoonshineSwapchain;
use super::protocols::moonshine_swapchain_factory_v2::MoonshineSwapchainFactoryV2;
use super::state::MoonshineCompositor;

// ---------------------------------------------------------------------------
// User data
// ---------------------------------------------------------------------------

/// User data for swapchain factory globals (shared by gamescope and moonshine).
pub struct SwapchainFactoryData;

/// User data for swapchain objects (shared by gamescope and moonshine).
pub struct SwapchainData {
	pub surface: WlSurface,
}

// ---------------------------------------------------------------------------
// Shared handler logic
// ---------------------------------------------------------------------------

/// Common swapchain feedback handling.
fn handle_swapchain_feedback(
	state: &mut MoonshineCompositor,
	_surface: &WlSurface,
	vk_colorspace: u32,
	vk_format: u32,
) -> (u32, u32) {
	tracing::debug!(vk_colorspace, vk_format, "swapchain_feedback");

	// NOTE: We intentionally do NOT set Bt2020Pq here based on the swapchain
	// color space alone.  DXVK with DXVK_HDR=1 creates HDR swapchains even
	// for SDR games, so the color space in the swapchain create info doesn't
	// mean the game is actually outputting PQ data.  We defer the HDR switch
	// to handle_set_hdr_metadata(), which fires only when the game calls
	// vkSetHdrMetadataEXT — a reliable signal that the game is truly doing HDR.

	// Compute the refresh cycle to return to the WSI layer.
	let refresh_ns = state
		.output
		.preferred_mode()
		.map(|m| 1_000_000_000_000u64 / m.refresh.max(1) as u64)
		.unwrap_or(11_111_111); // ~90 fps default
	let refresh_hi = (refresh_ns >> 32) as u32;
	let refresh_lo = (refresh_ns & 0xffff_ffff) as u32;
	tracing::debug!(refresh_ns, "sending refresh_cycle");
	(refresh_hi, refresh_lo)
}

/// Common override_window_content handling.
fn handle_override_window_content(state: &mut MoonshineCompositor, surface: &WlSurface, x11_window: u32) {
	tracing::debug!(x11_window, "override_window_content");
	state.override_window_surface(x11_window, surface.clone());
}

/// Common set_hdr_metadata handling.
#[allow(clippy::too_many_arguments)]
fn handle_set_hdr_metadata(
	state: &mut MoonshineCompositor,
	surface: &WlSurface,
	display_primary_red_x: u32,
	display_primary_red_y: u32,
	display_primary_green_x: u32,
	display_primary_green_y: u32,
	display_primary_blue_x: u32,
	display_primary_blue_y: u32,
	white_point_x: u32,
	white_point_y: u32,
	max_display_mastering_luminance: u32,
	min_display_mastering_luminance: u32,
	max_cll: u32,
	max_fall: u32,
) {
	tracing::debug!(
		max_cll,
		max_fall,
		max_display_mastering_luminance,
		min_display_mastering_luminance,
		"set_hdr_metadata"
	);

	// The WSI layer remaps HDR→sRGB for the ICD, but DXVK doesn't see the
	// remap and performs sRGB→PQ conversion in its swapchain blitter.  The
	// pixel data arriving at the compositor is genuinely PQ-encoded, so we
	// set Bt2020Pq here.
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
		cm.set_gamescope_current(surface, desc);
	}
}

// ===========================================================================
// gamescope_swapchain_factory_v2 — Global + Dispatch
// ===========================================================================

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
		state: &mut Self,
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
				if let Some(cm) = &mut state.color_management {
					cm.clear_gamescope_current(&surface);
				}
				data_init.init(callback, SwapchainData { surface });
			},
			Request::Destroy => {},
		}
	}
}

// ---------------------------------------------------------------------------
// gamescope_swapchain — Dispatch
// ---------------------------------------------------------------------------

/// Shared match arms for gamescope and moonshine swapchain dispatch.
///
/// Both protocols have identical request variants; only the Rust enum path
/// and the override-window-content field name differ.
macro_rules! dispatch_swapchain {
	($state:expr, $resource:expr, $data:expr, $request:expr,
	 $mod:path, $xwayland_field:ident) => {{
		use $mod as req_mod;
		match $request {
			req_mod::Request::SwapchainFeedback {
				vk_colorspace,
				vk_format,
				..
			} => {
				let (hi, lo) = handle_swapchain_feedback($state, &$data.surface, vk_colorspace, vk_format);
				$resource.refresh_cycle(hi, lo);
			},
			req_mod::Request::OverrideWindowContent {
				$xwayland_field: _,
				x11_window,
			} => {
				handle_override_window_content($state, &$data.surface, x11_window);
			},
			req_mod::Request::SetHdrMetadata {
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
				handle_set_hdr_metadata(
					$state,
					&$data.surface,
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
				);
			},
			req_mod::Request::SetPresentMode { .. }
			| req_mod::Request::SetPresentTime { .. }
			| req_mod::Request::Destroy => {},
		}
	}};
}

impl Dispatch<GamescopeSwapchain, SwapchainData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		resource: &GamescopeSwapchain,
		request: <GamescopeSwapchain as Resource>::Request,
		data: &SwapchainData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		dispatch_swapchain!(
			state,
			resource,
			data,
			request,
			super::protocols::gamescope_swapchain,
			gamescope_xwayland_server_id
		);
	}
}

// ===========================================================================
// moonshine_swapchain_factory_v2 — Global + Dispatch
// ===========================================================================

impl GlobalDispatch<MoonshineSwapchainFactoryV2, ()> for MoonshineCompositor {
	fn bind(
		_state: &mut Self,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<MoonshineSwapchainFactoryV2>,
		_global_data: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		tracing::debug!("moonshine_swapchain_factory_v2 bound");
		data_init.init(resource, SwapchainFactoryData);
	}
}

impl Dispatch<MoonshineSwapchainFactoryV2, SwapchainFactoryData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &MoonshineSwapchainFactoryV2,
		request: <MoonshineSwapchainFactoryV2 as Resource>::Request,
		_data: &SwapchainFactoryData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		use super::protocols::moonshine_swapchain_factory_v2::Request;
		match request {
			Request::CreateSwapchain { surface, callback } => {
				tracing::debug!("moonshine_swapchain_factory_v2::create_swapchain");
				if let Some(cm) = &mut state.color_management {
					cm.clear_gamescope_current(&surface);
				}
				data_init.init(callback, SwapchainData { surface });
			},
			Request::Destroy => {},
		}
	}
}

// ---------------------------------------------------------------------------
// moonshine_swapchain — Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<MoonshineSwapchain, SwapchainData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		resource: &MoonshineSwapchain,
		request: <MoonshineSwapchain as Resource>::Request,
		data: &SwapchainData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		dispatch_swapchain!(
			state,
			resource,
			data,
			request,
			super::protocols::moonshine_swapchain,
			xwayland_server_id
		);
	}
}

// ---------------------------------------------------------------------------
// Global registration
// ---------------------------------------------------------------------------

/// Registers the moonshine swapchain factory global.
///
/// Always called, even for SDR sessions — the protocol is needed for XWayland
/// bypass, refresh_cycle, and retire handling regardless of HDR support.
pub fn register_moonshine_globals(display: &DisplayHandle) {
	display.create_global::<MoonshineCompositor, MoonshineSwapchainFactoryV2, _>(1, ());
}

/// Registers the gamescope swapchain factory global (DXVK HDR-compat path).
///
/// Only registered when HDR is active, because DXVK detects HDR by probing
/// this global and we must not advertise it on SDR sessions.
pub fn register_gamescope_globals(display: &DisplayHandle) {
	display.create_global::<MoonshineCompositor, GamescopeSwapchainFactoryV2, _>(1, ());
}
