//! `vkCreateSwapchainKHR`, `vkDestroySwapchainKHR`, `vkQueuePresentKHR` hooks.
//!
//! On `CreateSwapchainKHR`:
//!  - Call the next layer/ICD first to get a valid swapchain handle.
//!  - Look up the `SurfaceData` for the surface.
//!  - Create a `moonshine_swapchain` protocol object via the factory.
//!  - Send `swapchain_feedback` and `set_present_mode` to the compositor.
//!  - For XWayland bypass: send `override_window_content` mapping the XCB window.
//!  - Flush the Wayland connection.
//!
//! On `QueuePresentKHR`:
//!  - Dispatch any pending Wayland events (refresh_cycle, retired, timings).
//!  - Call through to the next layer/ICD.

use std::collections::VecDeque;

use ash::vk::Handle as _;

use crate::dispatch::*;
use crate::state::{
	get_wayland_connection, insert_swapchain, is_forcing_fifo, is_frame_limiter_aware, remove_swapchain, with_device,
	with_surface, with_swapchain, MutexExt, SurfaceKey, SwapchainData, SwapchainKey,
};

pub unsafe extern "C" fn create_swapchain(
	device: VkDevice,
	p_create_info: *const VkSwapchainCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_swapchain: *mut VkSwapchain,
) -> VkResult {
	let device_key = device_key_of(device);
	let create_info = &*p_create_info;

	// Save the app's original color space before potentially remapping.
	let app_color_space = create_info.image_color_space;

	// Look up the instance so we can check Wayland connection state before
	// the ICD call (the remap must only happen when the layer is active).
	let instance_key = with_device(device_key, |d| d.instance_key);

	// The layer injects HDR color spaces (e.g. HDR10_ST2084_EXT) that the
	// ICD may not natively support for Wayland surfaces. Remap to
	// SRGB_NONLINEAR for the ICD call; the real color space is communicated
	// to the compositor via the swapchain_feedback protocol instead.
	//
	// Only remap when the layer is connected to the compositor AND the
	// compositor signals HDR support; otherwise pass the app's create-info
	// through unchanged so the ICD can handle its own color management.
	let layer_hdr_active = instance_key
		.and_then(get_wayland_connection)
		.map(|arc| arc.force_lock().caps.hdr_supported)
		.unwrap_or(false);
	let need_remap = layer_hdr_active && app_color_space != ash::vk::ColorSpaceKHR::SRGB_NONLINEAR;
	let p_create_info_for_icd;
	let mut patched_create_info;
	if need_remap {
		patched_create_info = *create_info;
		patched_create_info.image_color_space = ash::vk::ColorSpaceKHR::SRGB_NONLINEAR;
		p_create_info_for_icd = &patched_create_info as *const VkSwapchainCreateInfoKHR;
	} else {
		p_create_info_for_icd = p_create_info;
	}

	// Call the next layer/ICD first.
	let result = with_device(device_key, |data| {
		(data.dispatch.create_swapchain)(device, p_create_info_for_icd, p_allocator, p_swapchain)
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

	if result != VK_SUCCESS {
		return result;
	}

	let swapchain = *p_swapchain;
	let swapchain_key = SwapchainKey::from_raw(swapchain.as_raw());
	let surface_key = SurfaceKey::from_raw(create_info.surface.as_raw());
	// The requested minimum; the driver may allocate more images than this.
	let min_image_count = create_info.min_image_count;

	crate::log_info!(
		"vkCreateSwapchainKHR: {}x{} format={} colorspace={} mode={} images={}",
		create_info.image_extent.width,
		create_info.image_extent.height,
		create_info.image_format.as_raw(),
		app_color_space.as_raw(),
		create_info.present_mode.as_raw(),
		min_image_count,
	);

	// Look up the VkInstance key from the device (reuse the already-cached value).
	let instance_key = match instance_key {
		Some(k) => k,
		None => {
			// Still insert a minimal swapchain record.
			insert_swapchain(
				swapchain_key,
				SwapchainData {
					device_key,
					present_mode: create_info.present_mode,
					_format: create_info.image_format,
					_color_space: create_info.image_color_space,
					_image_count: min_image_count,
					_extent: create_info.image_extent,
					_surface: create_info.surface,
					ms_swapchain: None,
					refresh_cycle_ns: 0,
					retired: false,
					force_fifo_at_creation: is_forcing_fifo(),
					past_timings: VecDeque::new(),
				},
			);
			return VK_SUCCESS;
		},
	};

	// Get the Wayland connection (without holding any map lock).
	let ms_swapchain = get_wayland_connection(instance_key).and_then(|arc| {
		// Retrieve the wl_surface and xcb_window for this VkSurface.
		let (wl_surface, xcb_window) = with_surface(surface_key, |s| (s.wl_surface.clone(), s.xcb_window))?;

		let mut wl = arc.force_lock();
		// Create the protocol object; UserData = swapchain raw handle for
		// event dispatch back into SWAPCHAIN_MAP.
		let ms = wl
			.swapchain_factory
			.create_swapchain(&wl_surface, &wl.qh, swapchain_key.raw());

		// Send initial swapchain feedback with the APP's original
		// color space.  The layer remaps HDR→sRGB for the ICD, but
		// DXVK doesn't see that remap and converts sRGB→PQ in its
		// swapchain blitter.  The pixel data arriving at the
		// compositor is PQ-encoded, matching the app's requested
		// color space.
		ms.swapchain_feedback(
			min_image_count,
			create_info.image_format.as_raw() as u32,
			app_color_space.as_raw() as u32,
			create_info.composite_alpha.as_raw(),
			create_info.pre_transform.as_raw(),
			create_info.clipped,
		);

		// Tell the compositor the present mode.
		ms.set_present_mode(create_info.present_mode.as_raw() as u32);

		// Map the bypass wl_surface to the X11 window so the
		// compositor renders it in place of the XWayland surface.
		// The ICD renders directly to the wl_surface; this
		// override tells the compositor which window it belongs to.
		if let Some(xid) = xcb_window {
			crate::log_debug!("vkCreateSwapchainKHR: override_window_content x11_window={}", xid);
			ms.override_window_content(0, xid);
		}

		wl.flush();
		Some(ms)
	});

	insert_swapchain(
		swapchain_key,
		SwapchainData {
			device_key,
			present_mode: create_info.present_mode,
			_format: create_info.image_format,
			_color_space: create_info.image_color_space,
			_image_count: min_image_count,
			_extent: create_info.image_extent,
			_surface: create_info.surface,
			ms_swapchain,
			refresh_cycle_ns: 0,
			retired: false,
			force_fifo_at_creation: is_forcing_fifo(),
			past_timings: VecDeque::new(),
		},
	);

	VK_SUCCESS
}

pub unsafe extern "C" fn destroy_swapchain(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_allocator: *const VkAllocationCallbacks,
) {
	crate::log_debug!("vkDestroySwapchainKHR");

	// Drop the SwapchainData first; this Drops the MoonshineSwapchain proxy
	// which sends the destructor to the compositor.
	remove_swapchain(SwapchainKey::from_raw(swapchain.as_raw()));

	let device_key = device_key_of(device);
	with_device(device_key, |data| {
		(data.dispatch.destroy_swapchain)(device, swapchain, p_allocator);
	});
}

pub unsafe extern "C" fn queue_present(queue: VkQueue, p_present_info: *const VkPresentInfoKHR) -> VkResult {
	let queue_key = device_key_of(queue);

	let present_info = &*p_present_info;
	let swapchains = if present_info.swapchain_count > 0 {
		std::slice::from_raw_parts(present_info.p_swapchains, present_info.swapchain_count as usize)
	} else {
		&[]
	};

	let force_fifo = is_forcing_fifo();

	// Compute effective present mode for each swapchain once.
	// Use a stack buffer for the common case.
	// Most apps use 1-2 swapchains; 4 covers typical edge cases.
	const MAX_SWAPCHAINS_ON_STACK: usize = 4;
	let mut modes_stack = [ash::vk::PresentModeKHR::FIFO; MAX_SWAPCHAINS_ON_STACK];
	let mut modes_heap;
	let present_modes: &[ash::vk::PresentModeKHR] = {
		let buf = if swapchains.len() <= modes_stack.len() {
			&mut modes_stack[..swapchains.len()]
		} else {
			modes_heap = vec![ash::vk::PresentModeKHR::FIFO; swapchains.len()];
			&mut modes_heap[..]
		};
		for (mode, sw) in buf.iter_mut().zip(swapchains.iter()) {
			*mode = if force_fifo {
				ash::vk::PresentModeKHR::FIFO
			} else {
				with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| sd.present_mode)
					.unwrap_or(ash::vk::PresentModeKHR::FIFO)
			};
		}
		buf
	};

	// Dispatch pending Wayland events and send per-present mode to compositor.
	// Skip all compositor operations if the connection is dead.
	let wayland_arc = swapchains
		.first()
		.and_then(|sw| with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |d| d.device_key))
		.and_then(|dk| with_device(dk, |d| d.instance_key))
		.and_then(get_wayland_connection);

	if let Some(arc) = wayland_arc {
		let mut wl = arc.force_lock();
		if !wl.dead {
			wl.dispatch_pending();

			// Extract VkPresentTimesInfoGOOGLE from pNext chain.
			let present_times = find_in_chain::<ash::vk::PresentTimesInfoGOOGLE>(
				present_info.p_next,
				ash::vk::StructureType::PRESENT_TIMES_INFO_GOOGLE,
			);

			// Send per-swapchain present mode and timing to compositor.
			for (i, (sw, &mode)) in swapchains.iter().zip(present_modes.iter()).enumerate() {
				with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| {
					if let Some(ref ms) = sd.ms_swapchain {
						ms.set_present_mode(mode.as_raw() as u32);

						// Forward present time if available.
						if let Some(times_ptr) = present_times {
							if !(*times_ptr).p_times.is_null() && i < (*times_ptr).swapchain_count as usize {
								let time = &*(*times_ptr).p_times.add(i);
								ms.set_present_time(
									time.present_id,
									(time.desired_present_time >> 32) as u32,
									time.desired_present_time as u32,
								);
							}
						}
					}
				});
			}

			wl.flush();
		}
	}

	// Build per-present mode info if maintenance1 is available.
	let has_maintenance1 = with_device(queue_key, |d| d.has_maintenance1).unwrap_or(false);
	let mut present_mode_info;

	let effective_present_info = if has_maintenance1 && !swapchains.is_empty() {
		present_mode_info = ash::vk::SwapchainPresentModeInfoEXT::default().present_modes(present_modes);
		present_mode_info.p_next = present_info.p_next as *mut std::ffi::c_void;

		let mut modified = *present_info;
		modified.p_next = &present_mode_info as *const _ as *const std::ffi::c_void;
		modified
	} else {
		*present_info
	};

	// Forward to the next layer/ICD.
	// If the queue is somehow not in our DEVICE_MAP (which should not happen
	// once create_device always inserts DeviceData) fall back to looking up
	// the dispatch table via the first swapchain rather than returning the
	// synthetic DEVICE_LOST that would incorrectly break presentation.
	let result = with_device(queue_key, |data| {
		(data.dispatch.queue_present)(queue, &effective_present_info)
	})
	.or_else(|| {
		swapchains
			.first()
			.and_then(|sw| with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |d| d.device_key))
			.and_then(|device_key| {
				with_device(device_key, |data| {
					(data.dispatch.queue_present)(queue, &effective_present_info)
				})
			})
	})
	.unwrap_or(VK_ERROR_DEVICE_LOST);

	// If the limiter state changed since swapchain creation and the app is
	// frame-limiter-aware, force swapchain recreation so it re-queries the
	// restricted present mode list.
	let frame_limiter_aware = swapchains
		.first()
		.and_then(|sw| with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |d| d.device_key))
		.and_then(|dk| with_device(dk, |d| d.instance_key))
		.map(is_frame_limiter_aware)
		.unwrap_or(false);

	if frame_limiter_aware {
		for (i, sw) in swapchains.iter().enumerate() {
			let fifo_changed = with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| {
				sd.force_fifo_at_creation != force_fifo
			})
			.unwrap_or(false);

			if fifo_changed {
				if !present_info.p_results.is_null() {
					let results =
						std::slice::from_raw_parts_mut(present_info.p_results, present_info.swapchain_count as usize);
					if results[i] >= ash::vk::Result::SUCCESS {
						results[i] = VK_ERROR_OUT_OF_DATE_KHR;
					}
				}
				return VK_ERROR_OUT_OF_DATE_KHR;
			}
		}
	}

	result
}

// ---------------------------------------------------------------------------
// HDR metadata float-to-protocol-uint conversions
// ---------------------------------------------------------------------------

/// Convert float CIE 1931 coordinates to uint16 (units of 0.00002, so 50000 == 1.0).
fn color_xy_to_u16(v: f32) -> u32 {
	(v * 50000.0).round().clamp(0.0, 65535.0) as u32
}

/// Convert luminance in cd/m² to uint16 (1 cd/m² units).
fn nits_to_u16(v: f32) -> u32 {
	v.round().clamp(0.0, 65535.0) as u32
}

/// Convert min luminance (0.0001 cd/m² units) to uint16.
fn nits_to_u16_dark(v: f32) -> u32 {
	(v * 10000.0).round().clamp(0.0, 65535.0) as u32
}

/// Intercept `vkSetHdrMetadataEXT` to forward HDR metadata to the compositor.
pub unsafe extern "C" fn set_hdr_metadata(
	device: VkDevice,
	swapchain_count: u32,
	p_swapchains: *const VkSwapchain,
	p_metadata: *const VkHdrMetadataEXT,
) {
	let swapchains = std::slice::from_raw_parts(p_swapchains, swapchain_count as usize);
	let metadata = std::slice::from_raw_parts(p_metadata, swapchain_count as usize);

	let device_key = device_key_of(device);

	for (sw, md) in swapchains.iter().zip(metadata.iter()) {
		crate::log_debug!(
			"vkSetHdrMetadataEXT: max_lum={} min_lum={} max_cll={} max_fall={}",
			md.max_luminance,
			md.min_luminance,
			md.max_content_light_level,
			md.max_frame_average_light_level,
		);
		let sw_key = SwapchainKey::from_raw(sw.as_raw());
		let instance_key = with_swapchain(sw_key, |d| d.device_key).and_then(|dk| with_device(dk, |d| d.instance_key));

		let forwarded_to_compositor = if let Some(inst_key) = instance_key {
			if let Some(arc) = get_wayland_connection(inst_key) {
				// Look up the moonshine_swapchain protocol object and send metadata.
				let has_compositor_swapchain = with_swapchain(sw_key, |sd| sd.ms_swapchain.is_some()).unwrap_or(false);
				if has_compositor_swapchain {
					with_swapchain(sw_key, |sd| {
						if let Some(ref ms) = sd.ms_swapchain {
							ms.set_hdr_metadata(
								color_xy_to_u16(md.display_primary_red.x),
								color_xy_to_u16(md.display_primary_red.y),
								color_xy_to_u16(md.display_primary_green.x),
								color_xy_to_u16(md.display_primary_green.y),
								color_xy_to_u16(md.display_primary_blue.x),
								color_xy_to_u16(md.display_primary_blue.y),
								color_xy_to_u16(md.white_point.x),
								color_xy_to_u16(md.white_point.y),
								nits_to_u16(md.max_luminance),
								nits_to_u16_dark(md.min_luminance),
								nits_to_u16(md.max_content_light_level),
								nits_to_u16(md.max_frame_average_light_level),
							);
						}
					});
					arc.force_lock().flush();
					true
				} else {
					false
				}
			} else {
				false
			}
		} else {
			false
		};

		// In degraded mode (no compositor connection or no ms_swapchain),
		// forward to the next layer/ICD so HDR metadata is not silently dropped.
		if !forwarded_to_compositor {
			if let Some(Some(next)) = with_device(device_key, |data| data.dispatch.set_hdr_metadata) {
				next(device, 1, sw, md);
			}
		}
	}
}

/// Intercept `vkAcquireNextImageKHR` to check swapchain retirement.
pub unsafe extern "C" fn acquire_next_image(
	device: VkDevice,
	swapchain: VkSwapchain,
	timeout: u64,
	semaphore: ash::vk::Semaphore,
	fence: ash::vk::Fence,
	p_image_index: *mut u32,
) -> VkResult {
	// If the compositor has retired this swapchain, tell the app to recreate it.
	let retired = with_swapchain(SwapchainKey::from_raw(swapchain.as_raw()), |d| d.retired).unwrap_or(false);
	if retired {
		return VK_ERROR_OUT_OF_DATE_KHR;
	}

	let device_key = device_key_of(device);
	if let Some(next) = with_device(device_key, |data| data.dispatch.acquire_next_image) {
		return next(device, swapchain, timeout, semaphore, fence, p_image_index);
	}
	VK_ERROR_INITIALIZATION_FAILED
}

/// Intercept `vkAcquireNextImage2KHR` to check swapchain retirement.
pub unsafe extern "C" fn acquire_next_image2(
	device: VkDevice,
	p_acquire_info: *const ash::vk::AcquireNextImageInfoKHR,
	p_image_index: *mut u32,
) -> VkResult {
	let acquire_info = &*p_acquire_info;

	// If the compositor has retired this swapchain, tell the app to recreate it.
	let retired =
		with_swapchain(SwapchainKey::from_raw(acquire_info.swapchain.as_raw()), |d| d.retired).unwrap_or(false);
	if retired {
		return VK_ERROR_OUT_OF_DATE_KHR;
	}

	let device_key = device_key_of(device);
	if let Some(Some(next)) = with_device(device_key, |data| data.dispatch.acquire_next_image2) {
		return next(device, p_acquire_info, p_image_index);
	}
	VK_ERROR_INITIALIZATION_FAILED
}

/// Intercept `vkGetRefreshCycleDurationGOOGLE` to return the compositor's refresh cycle.
pub unsafe extern "C" fn get_refresh_cycle_duration(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_display_timing_properties: *mut ash::vk::RefreshCycleDurationGOOGLE,
) -> VkResult {
	let ns = with_swapchain(SwapchainKey::from_raw(swapchain.as_raw()), |d| d.refresh_cycle_ns).unwrap_or(0);

	if ns > 0 {
		(*p_display_timing_properties).refresh_duration = ns;
		return VK_SUCCESS;
	}

	// Fall through to the driver if we don't have compositor timing yet.
	let device_key = device_key_of(device);
	if let Some(Some(next)) = with_device(device_key, |data| data.dispatch.get_refresh_cycle_duration) {
		return next(device, swapchain, p_display_timing_properties);
	}
	VK_ERROR_INITIALIZATION_FAILED
}

/// Intercept `vkGetPastPresentationTimingGOOGLE` to return compositor timing data.
pub unsafe extern "C" fn get_past_presentation_timing(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_presentation_timing_count: *mut u32,
	p_presentation_timings: *mut ash::vk::PastPresentationTimingGOOGLE,
) -> VkResult {
	let result = with_swapchain(SwapchainKey::from_raw(swapchain.as_raw()), |d| {
		if p_presentation_timings.is_null() {
			*p_presentation_timing_count = d.past_timings.len() as u32;
			return VK_SUCCESS;
		}

		let caller_count = *p_presentation_timing_count as usize;
		let copy_count = caller_count.min(d.past_timings.len());
		for (i, t) in d.past_timings.iter().take(copy_count).enumerate() {
			*p_presentation_timings.add(i) = ash::vk::PastPresentationTimingGOOGLE {
				present_id: t.present_id,
				desired_present_time: t.desired_present_time,
				actual_present_time: t.actual_present_time,
				earliest_present_time: t.earliest_present_time,
				present_margin: t.present_margin,
			};
		}
		*p_presentation_timing_count = copy_count as u32;

		if copy_count < d.past_timings.len() {
			VK_INCOMPLETE
		} else {
			VK_SUCCESS
		}
	});

	if let Some(r) = result {
		return r;
	}

	// Unknown swapchain; fall through to driver.
	let device_key = device_key_of(device);
	with_device(device_key, |data| {
		if let Some(next) = data.dispatch.get_past_presentation_timing {
			next(device, swapchain, p_presentation_timing_count, p_presentation_timings)
		} else {
			VK_ERROR_INITIALIZATION_FAILED
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn color_xy_zero() {
		assert_eq!(color_xy_to_u16(0.0), 0);
	}

	#[test]
	fn color_xy_one() {
		assert_eq!(color_xy_to_u16(1.0), 50000);
	}

	#[test]
	fn color_xy_bt2020_red() {
		// BT.2020 red primary: (0.708, 0.292)
		assert_eq!(color_xy_to_u16(0.708), 35400);
		assert_eq!(color_xy_to_u16(0.292), 14600);
	}

	#[test]
	fn color_xy_clamps_negative() {
		assert_eq!(color_xy_to_u16(-1.0), 0);
	}

	#[test]
	fn color_xy_clamps_overflow() {
		assert_eq!(color_xy_to_u16(2.0), 65535);
	}

	#[test]
	fn nits_zero() {
		assert_eq!(nits_to_u16(0.0), 0);
	}

	#[test]
	fn nits_1000() {
		assert_eq!(nits_to_u16(1000.0), 1000);
	}

	#[test]
	fn nits_clamps_large() {
		assert_eq!(nits_to_u16(100000.0), 65535);
	}

	#[test]
	fn nits_dark_zero() {
		assert_eq!(nits_to_u16_dark(0.0), 0);
	}

	#[test]
	fn nits_dark_one() {
		// 1.0 cd/m² × 10000 = 10000
		assert_eq!(nits_to_u16_dark(1.0), 10000);
	}

	#[test]
	fn nits_dark_typical_min() {
		// 0.005 cd/m² (typical OLED) × 10000 = 50
		assert_eq!(nits_to_u16_dark(0.005), 50);
	}
}
