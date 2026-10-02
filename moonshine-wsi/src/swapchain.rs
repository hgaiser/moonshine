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
	MutexExt, SurfaceKey, SwapchainData, SwapchainKey, get_wayland_connection, insert_swapchain, is_forcing_fifo,
	is_frame_limiter_aware, remove_swapchain, with_device, with_surface, with_swapchain, with_swapchain_mut,
};
use crate::surface::icd_fallback_surface;
use crate::xcb::{
	XCB_ATOM_WM_CLASS, xcb_get_largest_obscuring_child, xcb_get_window_attributes, xcb_get_window_property_u32,
	xcb_get_window_rect, xcb_query_tree_window, xcb_window_has_property,
};

pub unsafe extern "C" fn create_swapchain(
	device: VkDevice,
	p_create_info: *const VkSwapchainCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_swapchain: *mut VkSwapchain,
) -> VkResult {
	unsafe {
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

		// Compute surface_key early for bypass checks.
		let surface_key = SurfaceKey::from_raw(create_info.surface.as_raw());

		// Determine whether XWayland bypass is allowed for this surface.  When it
		// is not, present through the plain XCB fallback surface rather than the
		// un-mapped Wayland bypass surface.
		let fallback_surface = icd_fallback_surface(create_info.surface);
		let bypass_allowed = fallback_surface.is_none() && with_surface(surface_key, |_| ()).is_some();
		let need_remap =
			layer_hdr_active && bypass_allowed && app_color_space != ash::vk::ColorSpaceKHR::SRGB_NONLINEAR;
		let icd_surface = fallback_surface.unwrap_or(create_info.surface);
		let need_surface_patch = icd_surface.as_raw() != create_info.surface.as_raw();

		let device_info = with_device(device_key, |d| (d.physical_device, d.has_maintenance1));
		let mut effective_mode = create_info.present_mode;
		let app_modes = find_in_chain::<ash::vk::SwapchainPresentModesCreateInfoEXT>(
			create_info.p_next,
			ash::vk::StructureType::SWAPCHAIN_PRESENT_MODES_CREATE_INFO_EXT,
		);
		let mut declared_present_modes = if let Some(p) = app_modes {
			if (*p).present_mode_count == 0 {
				Vec::new()
			} else {
				std::slice::from_raw_parts((*p).p_present_modes, (*p).present_mode_count as usize).to_vec()
			}
		} else {
			Vec::new()
		};
		if app_modes.is_none()
			&& let Some((physical_device, true)) = device_info
		{
			let managed = with_surface(surface_key, |_| ()).is_some();
			if managed {
				let mut legacy = VkSurfaceCapabilitiesKHR::default();
				let legacy_result = crate::state::with_instance(instance_key_of(physical_device), |d| {
					d.dispatch.get_physical_device_surface_capabilities
				})
				.flatten()
				.map(|f| f(physical_device, icd_surface, &mut legacy));
				let requested = crate::image_count::query_mode(physical_device, icd_surface, create_info.present_mode);
				let fifo = crate::image_count::query_mode(physical_device, icd_surface, VkPresentModeKHR::FIFO);
				if legacy_result == Some(VK_SUCCESS)
					&& let Some((mode, modes)) = crate::image_count::swapchain_modes(
						create_info.min_image_count,
						create_info.present_mode,
						&legacy,
						requested.as_ref(),
						fifo.as_ref(),
						!need_surface_patch && crate::surface::is_bypass_surface(create_info.surface),
						is_forcing_fifo(),
					) {
					effective_mode = mode;
					declared_present_modes = modes;
				}
			}
		}
		let mut mode_info =
			ash::vk::SwapchainPresentModesCreateInfoEXT::default().present_modes(&declared_present_modes);
		mode_info.p_next = create_info.p_next;
		let mut patched_create_info = *create_info;
		patched_create_info.surface = icd_surface;
		if !create_info.old_swapchain.is_null()
			&& with_swapchain(SwapchainKey::from_raw(create_info.old_swapchain.as_raw()), |s| {
				s.is_bypassing_xwayland
			})
			.is_some_and(|old_bypass| old_bypass != bypass_allowed)
		{
			// oldSwapchain must belong to the same ICD surface. The application
			// still owns/destroys its old handle after this independent creation.
			retire_swapchain(SwapchainKey::from_raw(create_info.old_swapchain.as_raw()));
			patched_create_info.old_swapchain = VkSwapchain::null();
		}
		patched_create_info.present_mode = effective_mode;
		if need_remap {
			patched_create_info.image_color_space = ash::vk::ColorSpaceKHR::SRGB_NONLINEAR;
		}
		if app_modes.is_none() && !declared_present_modes.is_empty() {
			patched_create_info.p_next = (&mode_info as *const ash::vk::SwapchainPresentModesCreateInfoEXT).cast();
		}
		let p_create_info_for_icd = &patched_create_info;
		crate::log_debug!(
			"swapchain negotiation: requested minImageCount={} requested_mode={} effective_mode={} declared_modes={} mode_changed={}",
			create_info.min_image_count,
			create_info.present_mode.as_raw(),
			effective_mode.as_raw(),
			declared_present_modes.len(),
			effective_mode != create_info.present_mode
		);

		// Call the next layer/ICD first.
		let result = with_device(device_key, |data| data.dispatch.create_swapchain)
			.and_then(|f| f.map(|fn_ptr| fn_ptr(device, p_create_info_for_icd, p_allocator, p_swapchain)))
			.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

		if result != VK_SUCCESS {
			crate::log_debug!(
				"vkCreateSwapchainKHR failed: result={} requested minImageCount={} effective_mode={}",
				result.as_raw(),
				create_info.min_image_count,
				effective_mode.as_raw()
			);
			return result;
		}

		let swapchain = *p_swapchain;
		let swapchain_key = SwapchainKey::from_raw(swapchain.as_raw());
		// The requested minimum; the driver may allocate more images than this.
		let min_image_count = create_info.min_image_count;
		let image_count = with_device(device_key, |d| d.dispatch.get_swapchain_images)
			.flatten()
			.and_then(|f| {
				let mut count = 0;
				if f(device, swapchain, &mut count, std::ptr::null_mut()) == VK_SUCCESS {
					Some(count)
				} else {
					None
				}
			});
		crate::log_debug!(
			"swapchain allocation: requested minImageCount={} actual allocated image count={}",
			min_image_count,
			image_count.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
		);

		crate::log_info!(
			"vkCreateSwapchainKHR: {}x{} format={} colorspace={} requested_mode={} effective_mode={} requested_minImageCount={} allocated_images={:?} bypass={}",
			create_info.image_extent.width,
			create_info.image_extent.height,
			create_info.image_format.as_raw(),
			app_color_space.as_raw(),
			create_info.present_mode.as_raw(),
			effective_mode.as_raw(),
			min_image_count,
			image_count,
			bypass_allowed,
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
						present_mode: effective_mode,
						icd_present_mode: effective_mode,
						declared_present_modes,
						_format: create_info.image_format,
						_color_space: create_info.image_color_space,
						_image_count: image_count,
						_extent: create_info.image_extent,
						_surface: create_info.surface,
						ms_swapchain: None,
						refresh_cycle_ns: 0,
						retired: false,
						force_fifo_at_creation: is_forcing_fifo(),
						is_bypassing_xwayland: false,
						past_timings: VecDeque::new(),
					},
				);
				return VK_SUCCESS;
			},
		};

		// Get the Wayland connection (without holding any map lock).
		// Native Wayland surfaces are bound on the app's own connection, XCB
		// bypass surfaces use the layer's private connection.  Only bind a
		// compositor swapchain when presenting through the bypass surface; the
		// XCB fallback is presented by the ICD/XWayland directly.
		let native = with_surface(surface_key, |s| s.native.clone()).flatten();
		let ms_swapchain = if !bypass_allowed {
			None
		} else if let Some(native) = native {
			let mut wl = native.connection.force_lock();
			if wl.dead {
				None
			} else {
				let ms = wl
					.swapchain_factory
					.create_swapchain(&native.wl_surface, &wl.qh, swapchain_key.raw());

				ms.swapchain_feedback(
					image_count.unwrap_or(0),
					create_info.image_format.as_raw() as u32,
					app_color_space.as_raw() as u32,
					create_info.composite_alpha.as_raw(),
					create_info.pre_transform.as_raw(),
					create_info.clipped,
				);
				ms.set_present_mode(effective_mode.as_raw() as u32);
				wl.flush();
				Some(ms)
			}
		} else {
			get_wayland_connection(instance_key).and_then(|arc| {
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
					image_count.unwrap_or(0),
					create_info.image_format.as_raw() as u32,
					app_color_space.as_raw() as u32,
					create_info.composite_alpha.as_raw(),
					create_info.pre_transform.as_raw(),
					create_info.clipped,
				);

				// Tell the compositor the present mode.
				ms.set_present_mode(effective_mode.as_raw() as u32);

				// Map the bypass wl_surface to the X11 window so the
				// compositor renders it in place of the XWayland surface.
				if let Some(xid) = xcb_window {
					crate::log_debug!("vkCreateSwapchainKHR: override_window_content x11_window={}", xid);
					ms.override_window_content(0, xid);
				}

				wl.flush();
				Some(ms)
			})
		};

		insert_swapchain(
			swapchain_key,
			SwapchainData {
				device_key,
				present_mode: effective_mode,
				icd_present_mode: effective_mode,
				declared_present_modes,
				_format: create_info.image_format,
				_color_space: create_info.image_color_space,
				_image_count: image_count,
				_extent: create_info.image_extent,
				_surface: create_info.surface,
				ms_swapchain,
				refresh_cycle_ns: 0,
				retired: false,
				force_fifo_at_creation: is_forcing_fifo(),
				is_bypassing_xwayland: bypass_allowed,
				past_timings: VecDeque::new(),
			},
		);

		VK_SUCCESS
	}
}

/// Permanently invalidate an unsafe chain and unmap its replacement. Even if
/// topology becomes safe again before recreation, its override is now gone.
fn retire_swapchain(key: SwapchainKey) {
	let retired = with_swapchain_mut(key, |sd| {
		sd.retired = true;
		(sd.ms_swapchain.take(), sd.device_key, sd._surface)
	});
	if let Some((Some(ms), device_key, surface)) = retired {
		ms.destroy();
		let native = with_surface(SurfaceKey::from_raw(surface.as_raw()), |s| s.native.clone()).flatten();
		if let Some(native) = native {
			native.connection.force_lock().flush();
		} else if let Some(arc) = with_device(device_key, |d| d.instance_key).and_then(get_wayland_connection) {
			arc.force_lock().flush();
		}
	}
}

pub unsafe extern "C" fn destroy_swapchain(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_allocator: *const VkAllocationCallbacks,
) {
	unsafe {
		crate::log_debug!("vkDestroySwapchainKHR");

		// Send the protocol destructor explicitly; dropping a client proxy alone
		// does not remove the compositor's replacement binding.
		if let Some(data) = remove_swapchain(SwapchainKey::from_raw(swapchain.as_raw())) {
			if let Some(ms) = data.ms_swapchain {
				ms.destroy();
			}
			let native = with_surface(SurfaceKey::from_raw(data._surface.as_raw()), |s| s.native.clone()).flatten();
			if let Some(native) = native {
				native.connection.force_lock().flush();
			} else if let Some(arc) = with_device(data.device_key, |d| d.instance_key).and_then(get_wayland_connection)
			{
				arc.force_lock().flush();
			}
		}

		let device_key = device_key_of(device);
		with_device(device_key, |data| {
			if let Some(f) = data.dispatch.destroy_swapchain {
				f(device, swapchain, p_allocator);
			}
		});
	}
}

pub unsafe extern "C" fn queue_present(queue: VkQueue, p_present_info: *const VkPresentInfoKHR) -> VkResult {
	unsafe { queue_present_with_fifo(queue, p_present_info, is_forcing_fifo()) }
}

unsafe fn queue_present_with_fifo(
	queue: VkQueue,
	p_present_info: *const VkPresentInfoKHR,
	force_fifo: bool,
) -> VkResult {
	unsafe {
		let queue_key = device_key_of(queue);

		let present_info = &*p_present_info;
		let swapchains = if present_info.swapchain_count > 0 {
			std::slice::from_raw_parts(present_info.p_swapchains, present_info.swapchain_count as usize)
		} else {
			&[]
		};

		// Respect an application's mode chain and never prepend a duplicate.
		let app_mode_info = find_in_chain::<ash::vk::SwapchainPresentModeInfoEXT>(
			present_info.p_next,
			ash::vk::StructureType::SWAPCHAIN_PRESENT_MODE_INFO_EXT,
		);
		let app_modes = app_mode_info.map(|p| {
			if (*p).swapchain_count == 0 {
				&[][..]
			} else {
				std::slice::from_raw_parts((*p).p_present_modes, (*p).swapchain_count as usize)
			}
		});
		let has_maintenance1 = with_device(queue_key, |d| d.has_maintenance1).unwrap_or(false);
		// A batch can be patched only if every swapchain declared modes.
		let can_inject_modes = has_maintenance1
			&& app_mode_info.is_none()
			&& !swapchains.is_empty()
			&& swapchains.iter().all(|sw| {
				with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| {
					!sd.declared_present_modes.is_empty()
				})
				.unwrap_or(false)
			});
		// Keep the common one/two-swapchain presentation path allocation-free.
		const MAX_SWAPCHAINS_ON_STACK: usize = 4;
		let mut modes_stack = [VkPresentModeKHR::FIFO; MAX_SWAPCHAINS_ON_STACK];
		let mut modes_heap;
		let present_modes: &[VkPresentModeKHR] = {
			let buf = if swapchains.len() <= modes_stack.len() {
				&mut modes_stack[..swapchains.len()]
			} else {
				modes_heap = vec![VkPresentModeKHR::FIFO; swapchains.len()];
				&mut modes_heap[..]
			};
			for (i, (mode, sw)) in buf.iter_mut().zip(swapchains.iter()).enumerate() {
				let key = SwapchainKey::from_raw(sw.as_raw());
				*mode = if let Some(mode) = app_modes.and_then(|m| m.get(i)).copied() {
					mode
				} else {
					with_swapchain(key, |sd| {
						if can_inject_modes && force_fifo && sd.declared_present_modes.contains(&VkPresentModeKHR::FIFO)
						{
							VkPresentModeKHR::FIFO
						} else if can_inject_modes {
							sd.present_mode
						} else {
							sd.icd_present_mode
						}
					})
					.unwrap_or(VkPresentModeKHR::FIFO)
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
							if let Some(times_ptr) = present_times
								&& !(*times_ptr).p_times.is_null()
								&& i < (*times_ptr).swapchain_count as usize
							{
								let time = &*(*times_ptr).p_times.add(i);
								ms.set_present_time(
									time.present_id,
									(time.desired_present_time >> 32) as u32,
									time.desired_present_time as u32,
								);
							}
						}
					});
				}

				wl.flush();
			}
		}

		// Native Wayland swapchains live on the app's own connection; dispatch
		// their pending events too.  This never reads the socket (the app does),
		// so it cannot block or race the application's event loop.
		for sw in swapchains {
			let native = with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| sd._surface)
				.and_then(|surf| with_surface(SurfaceKey::from_raw(surf.as_raw()), |s| s.native.clone()).flatten());
			if let Some(native) = native {
				let mut wl = native.connection.force_lock();
				if !wl.dead {
					wl.dispatch_pending();
				}
			}
		}

		// Build per-present mode info if maintenance1 is available.
		let mut present_mode_info;

		let effective_present_info = if can_inject_modes {
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
		let result = with_device(queue_key, |data| data.dispatch.queue_present)
			.and_then(|f| f.map(|fn_ptr| fn_ptr(queue, &effective_present_info)))
			.or_else(|| {
				swapchains
					.first()
					.and_then(|sw| with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |d| d.device_key))
					.and_then(|device_key| {
						with_device(device_key, |data| data.dispatch.queue_present)
							.and_then(|f| f.map(|fn_ptr| fn_ptr(queue, &effective_present_info)))
					})
			})
			.unwrap_or(VK_ERROR_DEVICE_LOST);
		// Track successful mode changes so a later unpatched batch retains the
		// same ICD/compositor semantics. The ICD keeps its last submitted mode.
		if result == VK_SUCCESS || result == VK_SUBOPTIMAL_KHR {
			for (i, sw) in swapchains.iter().enumerate() {
				let succeeded = present_info.p_results.is_null()
					|| *present_info.p_results.add(i) == VK_SUCCESS
					|| *present_info.p_results.add(i) == VK_SUBOPTIMAL_KHR;
				if succeeded {
					with_swapchain_mut(SwapchainKey::from_raw(sw.as_raw()), |sd| {
						sd.icd_present_mode = present_modes[i];
						if app_modes.is_some() {
							sd.present_mode = present_modes[i];
						}
					});
				}
			}
		}

		// Capture driver precedence before writing any synthetic per-chain status.
		let driver_results = if present_info.p_results.is_null() {
			None
		} else {
			Some(std::slice::from_raw_parts_mut(present_info.p_results, swapchains.len()))
		};
		let mut outcomes = PresentOutcomes::new(result, driver_results);

		// Recreate for limiter changes when the engine re-queries mode lists,
		// or when the ICD cannot safely switch the declared modes dynamically.
		let frame_limiter_aware = swapchains
			.first()
			.and_then(|sw| with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |d| d.device_key))
			.and_then(|dk| with_device(dk, |d| d.instance_key))
			.map(is_frame_limiter_aware)
			.unwrap_or(false);

		if frame_limiter_aware
			|| !can_inject_modes
			|| swapchains.iter().any(|sw| {
				with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| {
					!sd.declared_present_modes.contains(&VkPresentModeKHR::FIFO)
						|| sd.present_mode == VkPresentModeKHR::FIFO
				})
				.unwrap_or(false)
			}) {
			for (i, sw) in swapchains.iter().enumerate() {
				let fifo_changed = with_swapchain(SwapchainKey::from_raw(sw.as_raw()), |sd| {
					sd.force_fifo_at_creation != force_fifo
				})
				.unwrap_or(false);

				if fifo_changed {
					outcomes.request(i, VK_ERROR_OUT_OF_DATE_KHR);
				}
			}
		}

		// Re-evaluate XWayland bypass safety for each swapchain.  When it
		// changes, nudge the app to recreate the swapchain: OUT_OF_DATE when
		// bypass is no longer safe, SUBOPTIMAL when it becomes safe again.
		for (i, sw) in swapchains.iter().enumerate() {
			let sw_key = SwapchainKey::from_raw(sw.as_raw());
			let (was_bypassing, retired) =
				with_swapchain(sw_key, |sd| (sd.is_bypassing_xwayland, sd.retired)).unwrap_or((false, false));

			let Some(surface) = with_swapchain(sw_key, |sd| sd._surface) else {
				continue;
			};

			let now_allowed = can_bypass_xwayland(SurfaceKey::from_raw(surface.as_raw()));
			let Some(transition) = bypass_transition(was_bypassing, now_allowed, retired) else {
				continue;
			};
			if transition == VK_ERROR_OUT_OF_DATE_KHR {
				retire_swapchain(sw_key);
			}
			outcomes.request(i, transition);
		}

		outcomes.result()
	}
}

/// Driver failures win over policy hints, including failures in mixed batches.
/// Only SUCCESS/SUBOPTIMAL are eligible for a synthetic recreation request.
struct PresentOutcomes<'a> {
	driver: ash::vk::Result,
	policy: ash::vk::Result,
	per_chain: Option<&'a mut [ash::vk::Result]>,
}

impl<'a> PresentOutcomes<'a> {
	fn new(driver: ash::vk::Result, per_chain: Option<&'a mut [ash::vk::Result]>) -> Self {
		let driver = if driver.as_raw() < 0 {
			driver
		} else {
			per_chain
				.as_deref()
				.and_then(|results| results.iter().copied().find(|r| r.as_raw() < 0))
				.unwrap_or(driver)
		};
		Self {
			driver,
			policy: VK_SUCCESS,
			per_chain,
		}
	}

	fn request(&mut self, index: usize, hint: ash::vk::Result) {
		if let Some(results) = self.per_chain.as_deref_mut() {
			if !matches!(results[index], VK_SUCCESS | VK_SUBOPTIMAL_KHR) {
				return;
			}
			results[index] = hint;
		}
		if hint == VK_ERROR_OUT_OF_DATE_KHR || self.policy == VK_SUCCESS {
			self.policy = hint;
		}
	}

	fn result(&self) -> ash::vk::Result {
		if matches!(self.driver, VK_SUCCESS | VK_SUBOPTIMAL_KHR) && self.policy != VK_SUCCESS {
			self.policy
		} else {
			self.driver
		}
	}
}

// ---------------------------------------------------------------------------
// XWayland bypass safety checks
// ---------------------------------------------------------------------------

/// A rejection reason is cached with the surface and logged on policy changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BypassReject {
	WineNoFlip,
	WineOffscreen,
	Size,
	Position,
	Obscuring,
	Unavailable,
}

impl BypassReject {
	pub(crate) fn message(self) -> &'static str {
		match self {
			Self::WineNoFlip => "_WINE_ALLOW_FLIP=0",
			Self::WineOffscreen => "Wine offscreen presentation parent",
			Self::Size => "geometry size mismatch",
			Self::Position => "geometry position mismatch",
			Self::Obscuring => "obscuring child",
			Self::Unavailable => "X11 topology/event monitoring unavailable",
		}
	}
}

type WindowRect = (i32, i32, u32, u32);

/// All negative safety conditions precede the positive top-level shortcut.
fn bypass_policy(
	allow_flip: Option<u32>,
	offscreen_parent: bool,
	is_toplevel: bool,
	child: WindowRect,
	top: WindowRect,
	obscuring: bool,
) -> Result<(), BypassReject> {
	if allow_flip == Some(0) {
		return Err(BypassReject::WineNoFlip);
	}
	if offscreen_parent {
		return Err(BypassReject::WineOffscreen);
	}
	if obscuring {
		return Err(BypassReject::Obscuring);
	}
	if is_toplevel {
		return Ok(());
	}
	if child.2.abs_diff(top.2) > 2 || child.3.abs_diff(top.3) > 2 {
		return Err(BypassReject::Size);
	}
	if child.0.abs_diff(top.0) > 1 || child.1.abs_diff(top.1) > 1 {
		return Err(BypassReject::Position);
	}
	Ok(())
}

/// Queries run only at surface setup or after an X11 change event, never on
/// every frame. Wine flags and dummy topology apply even to a top-level XID.
pub(crate) unsafe fn query_bypass_policy(connection: *mut libc::c_void, window: u32) -> Result<(), BypassReject> {
	unsafe {
		let allow_flip = xcb_get_window_property_u32(connection, window, "_WINE_ALLOW_FLIP");
		if allow_flip == Some(0) {
			return Err(BypassReject::WineNoFlip);
		}
		let (root, parent, _) = xcb_query_tree_window(connection, window).ok_or(BypassReject::Unavailable)?;
		let offscreen_parent = if parent != root && parent != 0 {
			let rect = xcb_get_window_rect(connection, parent).ok_or(BypassReject::Unavailable)?;
			let (_, redirect) = xcb_get_window_attributes(connection, parent).ok_or(BypassReject::Unavailable)?;
			rect.2 == 1 && rect.3 == 1 && redirect && !xcb_window_has_property(connection, parent, XCB_ATOM_WM_CLASS)
		} else {
			false
		};
		if offscreen_parent {
			return Err(BypassReject::WineOffscreen);
		}
		let (_, _, width, height) = xcb_get_window_rect(connection, window).ok_or(BypassReject::Unavailable)?;
		let mut offset = (0i32, 0i32);
		let mut current = window;
		let mut content_child = None;
		let mut visited = std::collections::HashSet::new();
		let (toplevel, top_width, top_height) = loop {
			if !visited.insert(current) {
				return Err(BypassReject::Unavailable);
			}
			let (x, y, w, h) = xcb_get_window_rect(connection, current).ok_or(BypassReject::Unavailable)?;
			let obscuring = xcb_get_largest_obscuring_child(connection, current, content_child)
				.ok_or(BypassReject::Unavailable)?
				.is_some_and(|(w, h)| w > 1 && h > 1);
			if obscuring {
				return Err(BypassReject::Obscuring);
			}
			let (root, parent, _) = xcb_query_tree_window(connection, current).ok_or(BypassReject::Unavailable)?;
			if parent == root || parent == 0 {
				break (current, w, h);
			}
			// Sum child-relative offsets up to (but not including) the top-level.
			offset.0 += i32::from(x);
			offset.1 += i32::from(y);
			content_child = Some(current);
			current = parent;
		};
		bypass_policy(
			allow_flip,
			offscreen_parent,
			window == toplevel,
			(offset.0, offset.1, width, height),
			(0, 0, top_width, top_height),
			false,
		)
	}
}

pub(crate) unsafe fn can_bypass_xwayland(surface_key: SurfaceKey) -> bool {
	let data = with_surface(surface_key, |s| (s.native.is_some(), s.bypass_watch.clone()));
	match data {
		Some((true, _)) => true,
		Some((false, Some(watch))) => watch.force_lock().allowed(),
		_ => false,
	}
}

/// Actual presentation path is immutable for a swapchain. Unsafe bypass keeps
/// reporting OUT_OF_DATE until replaced; never mark a Wayland chain as XCB.
fn bypass_transition(was_bypassing: bool, now_allowed: bool, retired: bool) -> Option<VkResult> {
	if retired {
		return Some(VK_ERROR_OUT_OF_DATE_KHR);
	}
	match (was_bypassing, now_allowed) {
		(true, false) => Some(VK_ERROR_OUT_OF_DATE_KHR),
		(false, true) => Some(VK_SUBOPTIMAL_KHR),
		_ => None,
	}
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
	unsafe {
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
			let instance_key =
				with_swapchain(sw_key, |d| d.device_key).and_then(|dk| with_device(dk, |d| d.instance_key));

			let forwarded_to_compositor = if let Some(inst_key) = instance_key {
				if let Some(arc) = get_wayland_connection(inst_key) {
					// Look up the moonshine_swapchain protocol object and send metadata.
					let has_compositor_swapchain =
						with_swapchain(sw_key, |sd| sd.ms_swapchain.is_some()).unwrap_or(false);
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
			if !forwarded_to_compositor
				&& let Some(Some(next)) = with_device(device_key, |data| data.dispatch.set_hdr_metadata)
			{
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
	unsafe {
		// If the compositor has retired this swapchain, tell the app to recreate it.
		let retired = with_swapchain(SwapchainKey::from_raw(swapchain.as_raw()), |d| d.retired).unwrap_or(false);
		if retired {
			return VK_ERROR_OUT_OF_DATE_KHR;
		}

		let device_key = device_key_of(device);
		if let Some(Some(next)) = with_device(device_key, |data| data.dispatch.acquire_next_image) {
			return next(device, swapchain, timeout, semaphore, fence, p_image_index);
		}
		VK_ERROR_INITIALIZATION_FAILED
	}
}

/// Intercept `vkAcquireNextImage2KHR` to check swapchain retirement.
pub unsafe extern "C" fn acquire_next_image2(
	device: VkDevice,
	p_acquire_info: *const ash::vk::AcquireNextImageInfoKHR,
	p_image_index: *mut u32,
) -> VkResult {
	unsafe {
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
}

/// Intercept `vkGetRefreshCycleDurationGOOGLE` to return the compositor's refresh cycle.
pub unsafe extern "C" fn get_refresh_cycle_duration(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_display_timing_properties: *mut ash::vk::RefreshCycleDurationGOOGLE,
) -> VkResult {
	unsafe {
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
}

/// Intercept `vkGetPastPresentationTimingGOOGLE` to return compositor timing data.
pub unsafe extern "C" fn get_past_presentation_timing(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_presentation_timing_count: *mut u32,
	p_presentation_timings: *mut ash::vk::PastPresentationTimingGOOGLE,
) -> VkResult {
	unsafe {
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
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wine_safety_precedes_top_level_acceptance() {
		let rect = (0, 0, 1920, 1080);
		assert_eq!(
			bypass_policy(Some(0), false, true, rect, rect, false),
			Err(BypassReject::WineNoFlip)
		);
		assert_eq!(bypass_policy(Some(1), false, true, rect, rect, false), Ok(()));
		assert_eq!(bypass_policy(None, false, true, rect, rect, false), Ok(()));
		for flag in [None, Some(0), Some(1)] {
			assert!(bypass_policy(flag, true, true, rect, rect, false).is_err());
		}
		assert_eq!(
			bypass_policy(None, false, true, rect, rect, true),
			Err(BypassReject::Obscuring)
		);
	}

	#[test]
	fn child_geometry_tolerances_and_obscuring_are_preserved() {
		let top = (0, 0, 1920, 1080);
		assert_eq!(
			bypass_policy(None, false, false, (1, -1, 1918, 1082), top, false),
			Ok(())
		);
		assert_eq!(
			bypass_policy(None, false, false, (0, 0, 1917, 1080), top, false),
			Err(BypassReject::Size)
		);
		assert_eq!(
			bypass_policy(None, false, false, (0, 0, 1920, 1083), top, false),
			Err(BypassReject::Size)
		);
		assert_eq!(
			bypass_policy(None, false, false, (2, 0, 1920, 1080), top, false),
			Err(BypassReject::Position)
		);
		assert_eq!(
			bypass_policy(None, false, false, (0, -2, 1920, 1080), top, false),
			Err(BypassReject::Position)
		);
		assert_eq!(
			bypass_policy(None, false, false, top, top, true),
			Err(BypassReject::Obscuring)
		);
	}

	#[test]
	fn actual_swapchain_path_remains_fixed_until_recreation() {
		assert_eq!(bypass_transition(true, true, false), None);
		assert_eq!(bypass_transition(false, false, false), None);
		assert_eq!(bypass_transition(true, true, true), Some(VK_ERROR_OUT_OF_DATE_KHR));
		// Repeated presents cannot "convert" the old ICD swapchain by changing a flag.
		for _ in 0..3 {
			assert_eq!(bypass_transition(true, false, false), Some(VK_ERROR_OUT_OF_DATE_KHR));
			assert_eq!(bypass_transition(false, true, false), Some(VK_SUBOPTIMAL_KHR));
		}
	}

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

#[cfg(test)]
mod present_driver_tests {
	use super::*;
	use crate::state::{
		DeviceData, DeviceKey, InstanceKey, SwapchainData, insert_device, insert_swapchain, remove_device,
		remove_swapchain,
	};
	use std::collections::VecDeque;

	#[repr(C)]
	struct Driver {
		key: usize,
		aggregate: VkResult,
		per_chain: [VkResult; 3],
		calls: usize,
		modes: Vec<VkPresentModeKHR>,
	}
	unsafe extern "C" fn present(queue: VkQueue, info: *const VkPresentInfoKHR) -> VkResult {
		unsafe {
			let driver = &mut *(queue.as_raw() as *mut Driver);
			let info = &*info;
			driver.calls += 1;
			if !info.p_results.is_null() {
				std::ptr::copy_nonoverlapping(driver.per_chain.as_ptr(), info.p_results, info.swapchain_count as usize);
			}
			if let Some(modes) = find_in_chain::<ash::vk::SwapchainPresentModeInfoEXT>(
				info.p_next,
				ash::vk::StructureType::SWAPCHAIN_PRESENT_MODE_INFO_EXT,
			) {
				driver.modes =
					std::slice::from_raw_parts((*modes).p_present_modes, (*modes).swapchain_count as usize).to_vec();
			}
			driver.aggregate
		}
	}
	unsafe extern "C" fn get_proc(_: VkDevice, _: *const std::ffi::c_char) -> PFN_vkVoidFunction {
		None
	}
	unsafe extern "C" fn destroy(_: VkDevice, _: *const VkAllocationCallbacks) {}

	struct Fixture {
		driver: Box<Driver>,
		chains: [VkSwapchain; 3],
	}
	impl Fixture {
		fn new(aggregate: VkResult, per_chain: [VkResult; 3], dynamic: bool, at_creation: bool) -> Self {
			let mut driver = Box::new(Driver {
				key: 0,
				aggregate,
				per_chain,
				calls: 0,
				modes: Vec::new(),
			});
			driver.key = (&*driver as *const Driver) as usize;
			let key = DeviceKey(driver.key);
			insert_device(
				key,
				DeviceData {
					dispatch: DeviceDispatch {
						get_device_proc_addr: get_proc,
						destroy_device: destroy,
						queue_present: Some(present),
						create_swapchain: None,
						get_swapchain_images: None,
						destroy_swapchain: None,
						acquire_next_image: None,
						set_hdr_metadata: None,
						acquire_next_image2: None,
						get_refresh_cycle_duration: None,
						get_past_presentation_timing: None,
					},
					instance_key: InstanceKey(driver.key),
					has_maintenance1: dynamic,
					physical_device: VkPhysicalDevice::null(),
				},
			);
			let chains = std::array::from_fn(|i| VkSwapchain::from_raw(driver.key as u64 + i as u64 + 1));
			for chain in chains {
				insert_swapchain(
					SwapchainKey::from_raw(chain.as_raw()),
					SwapchainData {
						device_key: key,
						present_mode: VkPresentModeKHR::IMMEDIATE,
						icd_present_mode: VkPresentModeKHR::IMMEDIATE,
						declared_present_modes: if dynamic {
							vec![VkPresentModeKHR::IMMEDIATE, VkPresentModeKHR::FIFO]
						} else {
							vec![]
						},
						_format: ash::vk::Format::UNDEFINED,
						_color_space: ash::vk::ColorSpaceKHR::SRGB_NONLINEAR,
						_image_count: Some(3),
						_extent: ash::vk::Extent2D::default(),
						_surface: VkSurface::null(),
						ms_swapchain: None,
						refresh_cycle_ns: 0,
						retired: false,
						force_fifo_at_creation: at_creation,
						is_bypassing_xwayland: false,
						past_timings: VecDeque::new(),
					},
				);
			}
			Self { driver, chains }
		}
		fn present(&mut self, force: bool, results: Option<&mut [VkResult; 3]>) -> VkResult {
			let indices = [0; 3];
			let info = VkPresentInfoKHR {
				swapchain_count: 3,
				p_swapchains: self.chains.as_ptr(),
				p_image_indices: indices.as_ptr(),
				p_results: results.map_or(std::ptr::null_mut(), |r| r.as_mut_ptr()),
				..Default::default()
			};
			unsafe { queue_present_with_fifo(VkQueue::from_raw(self.driver.key as u64), &info, force) }
		}
	}
	impl Drop for Fixture {
		fn drop(&mut self) {
			for chain in self.chains {
				remove_swapchain(SwapchainKey::from_raw(chain.as_raw()));
			}
			remove_device(DeviceKey(self.driver.key));
		}
	}
	#[test]
	fn limiter_toggles_preserve_icd_failures_with_and_without_results() {
		for status in [
			VK_SUCCESS,
			VK_SUBOPTIMAL_KHR,
			VK_ERROR_DEVICE_LOST,
			ash::vk::Result::ERROR_OUT_OF_HOST_MEMORY,
			ash::vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
		] {
			for force in [false, true] {
				for per_chain in [false, true] {
					let mut fixture = Fixture::new(status, [status; 3], false, !force);
					let mut results = [VK_SUCCESS; 3];
					let result = fixture.present(force, per_chain.then_some(&mut results));
					let expected = if status.as_raw() < 0 {
						status
					} else {
						VK_ERROR_OUT_OF_DATE_KHR
					};
					assert_eq!(result, expected);
					if per_chain {
						assert_eq!(results, [expected; 3]);
					}
					assert_eq!(fixture.driver.calls, 1);
				}
			}
		}
	}
	#[test]
	fn mixed_results_keep_each_driver_failure_and_update_all_eligible_chains() {
		let failure = ash::vk::Result::ERROR_OUT_OF_DEVICE_MEMORY;
		for aggregate in [VK_SUCCESS, VK_SUBOPTIMAL_KHR, VK_ERROR_DEVICE_LOST] {
			let mut fixture = Fixture::new(aggregate, [VK_SUCCESS, failure, VK_SUBOPTIMAL_KHR], false, false);
			let mut results = [VK_SUCCESS; 3];
			assert_eq!(
				fixture.present(true, Some(&mut results)),
				if aggregate.as_raw() < 0 { aggregate } else { failure }
			);
			assert_eq!(results, [VK_ERROR_OUT_OF_DATE_KHR, failure, VK_ERROR_OUT_OF_DATE_KHR]);
		}
	}
	#[test]
	fn dynamic_fifo_toggles_keep_modes_and_results_without_recreation() {
		let mut fixture = Fixture::new(VK_SUCCESS, [VK_SUCCESS; 3], true, false);
		for force in [true, false, true, false] {
			let mut results = [VK_SUCCESS; 3];
			assert_eq!(fixture.present(force, Some(&mut results)), VK_SUCCESS);
			assert_eq!(results, [VK_SUCCESS; 3]);
			assert!(
				fixture.driver.modes
					== vec![
						if force {
							VkPresentModeKHR::FIFO
						} else {
							VkPresentModeKHR::IMMEDIATE
						};
						3
					]
			);
		}
	}
	#[test]
	fn bypass_and_limiter_hints_share_driver_precedence() {
		let mut per_chain = [VK_SUCCESS, VK_SUBOPTIMAL_KHR, VK_ERROR_DEVICE_LOST];
		let mut outcomes = PresentOutcomes::new(VK_SUCCESS, Some(&mut per_chain));
		outcomes.request(0, VK_SUBOPTIMAL_KHR);
		outcomes.request(1, VK_ERROR_OUT_OF_DATE_KHR);
		outcomes.request(2, VK_ERROR_OUT_OF_DATE_KHR);
		assert_eq!(outcomes.result(), VK_ERROR_DEVICE_LOST);
		assert_eq!(
			per_chain,
			[VK_SUBOPTIMAL_KHR, VK_ERROR_OUT_OF_DATE_KHR, VK_ERROR_DEVICE_LOST]
		);
		let mut outcomes = PresentOutcomes::new(VK_SUBOPTIMAL_KHR, None);
		outcomes.request(0, VK_ERROR_OUT_OF_DATE_KHR);
		outcomes.request(1, VK_SUBOPTIMAL_KHR);
		assert_eq!(outcomes.result(), VK_ERROR_OUT_OF_DATE_KHR);
	}
}
