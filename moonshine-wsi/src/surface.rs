//! Surface creation intercepts.
//!
//! ## Native Wayland surfaces (`vkCreateWaylandSurfaceKHR`)
//!
//! We hook the call to record the `wl_surface` so that the swapchain hook can
//! later create a `moonshine_swapchain` protocol object for it.
//!
//! ## XWayland bypass (`vkCreateXcbSurfaceKHR` / `vkCreateXlibSurfaceKHR`)
//!
//! For XWayland windows we create a _new_ `wl_surface` on the Moonshine
//! compositor and return a Vulkan Wayland surface backed by that.  This
//! bypasses XWayland's Glamor compositing for better performance.
//!
//! If the bypass fails we fall back to the real XCB surface.

use std::marker::PhantomData;

use ash::vk::Handle as _;
use wayland_client::Proxy;

use crate::dispatch::*;
use crate::state::{
	get_wayland_connection, insert_surface, is_layer_active, remove_surface, with_instance, with_surface, InstanceKey,
	MutexExt, SurfaceData, SurfaceKey,
};
use crate::xcb::{xcb_get_window_extent, xlib_to_xcb_connection};

extern "C" {
	/// Move a Wayland proxy to a different event queue.
	///
	/// Passing NULL as the queue moves the proxy to the default queue,
	/// which is what the ICD uses for its own `wl_display_dispatch()`.
	/// This is required so the ICD receives frame callbacks and buffer
	/// release events for the surface (our private queue is never
	/// dispatched by the ICD).
	///
	/// # Safety
	/// `proxy` must be a valid `wl_proxy*` on the same `wl_display`.
	/// `queue` must be a valid `wl_event_queue*` or NULL.
	fn wl_proxy_set_queue(proxy: *mut std::ffi::c_void, queue: *mut std::ffi::c_void);
}

// Extra HDR surface formats we expose when the compositor supports HDR.
static HDR_FORMATS: &[(ash::vk::Format, ash::vk::ColorSpaceKHR)] = &[
	(
		ash::vk::Format::A2B10G10R10_UNORM_PACK32,
		ash::vk::ColorSpaceKHR::HDR10_ST2084_EXT,
	),
	(
		ash::vk::Format::A2R10G10B10_UNORM_PACK32,
		ash::vk::ColorSpaceKHR::HDR10_ST2084_EXT,
	),
	(
		ash::vk::Format::R16G16B16A16_SFLOAT,
		ash::vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
	),
];

// ---------------------------------------------------------------------------
// Native Wayland surface hook
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn create_wayland_surface(
	instance: VkInstance,
	p_create_info: *const VkWaylandSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	let instance_key = instance_key_of(instance);

	// Call the next layer.
	let result = with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.create_wayland_surface {
			next(instance, p_create_info, p_allocator, p_surface)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

	if result != VK_SUCCESS {
		return result;
	}

	// NOTE: We intentionally do NOT record the app's wl_surface here.
	// The surface in VkWaylandSurfaceCreateInfoKHR is a raw C wl_surface*
	// on the application's Wayland connection, which is different from the
	// layer's private connection to the compositor.  We cannot use it with
	// the layer's moonshine_swapchain_factory.  The swapchain code handles
	// the missing surface gracefully (ms_swapchain = None).
	// TODO: use the app's wl_display to bind the protocol on the same connection.

	VK_SUCCESS
}

// ---------------------------------------------------------------------------
// XCB (XWayland bypass) surface hook
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn create_xcb_surface(
	instance: VkInstance,
	p_create_info: *const VkXcbSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	let instance_key = instance_key_of(instance);
	let create_info = &*p_create_info;

	// Try XWayland bypass: create a wl_surface on the Moonshine compositor.
	let bypass_result = try_xwayland_bypass(
		instance,
		instance_key,
		create_info.connection,
		create_info.window,
		p_allocator,
		p_surface,
	);

	if bypass_result == VK_SUCCESS {
		crate::log_info!(
			"vkCreateXcbSurfaceKHR: XWayland bypass active (window={})",
			create_info.window
		);
		return VK_SUCCESS;
	}

	// Bypass failed — fall back to a plain XCB surface.
	crate::log_debug!("vkCreateXcbSurfaceKHR: fallback to ICD (window={})", create_info.window);
	with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.create_xcb_surface {
			next(instance, p_create_info, p_allocator, p_surface)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
}

/// Convert a libX11 `Display*` to an `xcb_connection_t*` using
/// `XGetXCBConnection` from `libX11-xcb`.
pub unsafe extern "C" fn create_xlib_surface(
	instance: VkInstance,
	p_create_info: *const VkXlibSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	let create_info = &*p_create_info;

	let xcb_connection = xlib_to_xcb_connection(create_info.dpy);
	if xcb_connection.is_null() {
		return VK_ERROR_FEATURE_NOT_PRESENT;
	}

	let xcb_info = VkXcbSurfaceCreateInfoKHR {
		s_type: ash::vk::StructureType::XCB_SURFACE_CREATE_INFO_KHR,
		p_next: std::ptr::null(),
		flags: ash::vk::XcbSurfaceCreateFlagsKHR::empty(),
		connection: xcb_connection,
		window: create_info.window as u32,
		_marker: PhantomData,
	};

	create_xcb_surface(instance, &xcb_info, p_allocator, p_surface)
}

pub unsafe extern "C" fn destroy_surface(
	instance: VkInstance,
	surface: VkSurface,
	p_allocator: *const VkAllocationCallbacks,
) {
	let instance_key = instance_key_of(instance);

	remove_surface(SurfaceKey::from_raw(surface.as_raw()));

	with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.destroy_surface {
			next(instance, surface, p_allocator);
		}
	});
}

// ---------------------------------------------------------------------------
// Surface format/capabilities hooks
// ---------------------------------------------------------------------------

/// Minimum image count the layer enforces (default 3 for smooth pipelining).
/// Can be overridden via `MOONSHINE_WSI_MIN_IMAGE_COUNT`.
fn min_image_count() -> u32 {
	use std::sync::OnceLock;
	static MIN_COUNT: OnceLock<u32> = OnceLock::new();
	*MIN_COUNT.get_or_init(|| {
		std::env::var("MOONSHINE_WSI_MIN_IMAGE_COUNT")
			.ok()
			.and_then(|v| v.parse().ok())
			.unwrap_or(3)
	})
}

/// For XWayland bypass surfaces the ICD returns extent=0xFFFFFFFF (undefined)
/// because the bare wl_surface has no role.  Override with the X11 window size
/// so DXVK/the app can create a correctly-sized swapchain.
unsafe fn override_extent_from_xcb(surface_key: SurfaceKey, caps: &mut ash::vk::SurfaceCapabilitiesKHR) {
	with_surface(surface_key, |sd| {
		if let Some(window) = sd.xcb_window {
			if let Some((w, h)) = xcb_get_window_extent(sd.xcb_connection, window) {
				caps.current_extent = ash::vk::Extent2D { width: w, height: h };
			}
		}
	});
}

pub unsafe extern "C" fn get_physical_device_surface_capabilities(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_surface_capabilities: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
	let instance_key = instance_key_of(physical_device);

	let result = with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.get_physical_device_surface_capabilities {
			next(physical_device, surface, p_surface_capabilities)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

	if result != VK_SUCCESS {
		return result;
	}

	let caps = &mut *p_surface_capabilities;
	caps.min_image_count = caps.min_image_count.max(min_image_count());
	override_extent_from_xcb(SurfaceKey::from_raw(surface.as_raw()), caps);

	VK_SUCCESS
}

pub unsafe extern "C" fn get_physical_device_surface_capabilities2(
	physical_device: VkPhysicalDevice,
	p_surface_info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	p_surface_capabilities: *mut VkSurfaceCapabilities2KHR,
) -> VkResult {
	let instance_key = instance_key_of(physical_device);

	let result = with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.get_physical_device_surface_capabilities2 {
			next(physical_device, p_surface_info, p_surface_capabilities)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

	if result != VK_SUCCESS {
		return result;
	}

	let caps = &mut (*p_surface_capabilities).surface_capabilities;
	caps.min_image_count = caps.min_image_count.max(min_image_count());
	override_extent_from_xcb(SurfaceKey::from_raw((*p_surface_info).surface.as_raw()), caps);

	VK_SUCCESS
}

pub unsafe extern "C" fn get_physical_device_surface_present_modes(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_present_mode_count: *mut u32,
	p_present_modes: *mut VkPresentModeKHR,
) -> VkResult {
	let instance_key = instance_key_of(physical_device);

	// When the frame limiter is active AND the app is frame-limiter-aware
	// (DXVK/VKD3D-Proton), restrict exposed modes to FIFO only so the app
	// can self-throttle.  Non-aware apps get transparent FIFO override via
	// SwapchainPresentModeInfoEXT in QueuePresent instead.
	if crate::state::is_forcing_fifo() && crate::state::is_frame_limiter_aware(instance_key) {
		if p_present_modes.is_null() {
			*p_present_mode_count = 1;
		} else {
			let count = (*p_present_mode_count).min(1);
			if count >= 1 {
				*p_present_modes = VkPresentModeKHR::FIFO;
			}
			*p_present_mode_count = count;
			if count < 1 {
				return VK_INCOMPLETE;
			}
		}
		return VK_SUCCESS;
	}

	with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.get_physical_device_surface_present_modes {
			next(physical_device, surface, p_present_mode_count, p_present_modes)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
}

pub unsafe extern "C" fn get_physical_device_xcb_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family_index: u32,
	connection: *mut std::ffi::c_void,
	visual_id: u32,
) -> ash::vk::Bool32 {
	let instance_key = instance_key_of(physical_device);

	// Active mode: redirect to Wayland presentation support using Moonshine's display.
	if let Some(arc) = get_wayland_connection(instance_key) {
		let display_ptr = arc.force_lock().connection.backend().display_ptr() as *mut std::ffi::c_void;
		return with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.get_physical_device_wayland_presentation_support {
				next(physical_device, queue_family_index, display_ptr)
			} else {
				ash::vk::TRUE
			}
		})
		.unwrap_or(ash::vk::FALSE);
	}

	// Degraded mode: forward to the next layer/ICD.
	with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.get_physical_device_xcb_presentation_support {
			next(physical_device, queue_family_index, connection, visual_id)
		} else {
			ash::vk::FALSE
		}
	})
	.unwrap_or(ash::vk::FALSE)
}

pub unsafe extern "C" fn get_physical_device_xlib_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family_index: u32,
	dpy: *mut std::ffi::c_void,
	visual_id: u64,
) -> ash::vk::Bool32 {
	let instance_key = instance_key_of(physical_device);

	// Active mode: delegate to XCB version (which redirects to Wayland).
	if get_wayland_connection(instance_key).is_some() {
		return get_physical_device_xcb_presentation_support(
			physical_device,
			queue_family_index,
			std::ptr::null_mut(),
			0,
		);
	}

	// Degraded mode: forward to the next layer/ICD's Xlib function.
	with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.get_physical_device_xlib_presentation_support {
			next(physical_device, queue_family_index, dpy, visual_id)
		} else {
			ash::vk::FALSE
		}
	})
	.unwrap_or(ash::vk::FALSE)
}

// ---------------------------------------------------------------------------
// Device extension enumeration
// ---------------------------------------------------------------------------

/// Extensions the layer provides even when the driver does not.
static LAYER_EXTENSIONS: &[ash::vk::ExtensionProperties] = &[
	ash::vk::ExtensionProperties {
		extension_name: ext_name(b"VK_EXT_hdr_metadata\0"),
		spec_version: 2,
	},
	ash::vk::ExtensionProperties {
		extension_name: ext_name(b"VK_GOOGLE_display_timing\0"),
		spec_version: 1,
	},
];

/// Convert a byte-string literal to a fixed-size `c_char` array at compile time.
const fn ext_name(name: &[u8]) -> [std::ffi::c_char; 256] {
	let mut buf = [0i8; 256];
	let mut i = 0;
	while i < name.len() && i < 255 {
		buf[i] = name[i] as i8;
		i += 1;
	}
	buf
}

pub unsafe extern "C" fn enumerate_device_extension_properties(
	physical_device: VkPhysicalDevice,
	p_layer_name: *const std::ffi::c_char,
	p_property_count: *mut u32,
	p_properties: *mut ash::vk::ExtensionProperties,
) -> VkResult {
	let instance_key = instance_key_of(physical_device);

	// When querying a specific layer's extensions, only return ours for our layer.
	if !p_layer_name.is_null() {
		let layer = std::ffi::CStr::from_ptr(p_layer_name);
		if layer.to_bytes() == b"VK_LAYER_MOONSHINE_wsi_x86_64" {
			if p_properties.is_null() {
				*p_property_count = LAYER_EXTENSIONS.len() as u32;
				return VK_SUCCESS;
			}
			let count = (*p_property_count as usize).min(LAYER_EXTENSIONS.len());
			std::ptr::copy_nonoverlapping(LAYER_EXTENSIONS.as_ptr(), p_properties, count);
			*p_property_count = count as u32;
			return if count < LAYER_EXTENSIONS.len() {
				VK_INCOMPLETE
			} else {
				VK_SUCCESS
			};
		}
		// Not our layer — forward.
		return with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.enumerate_device_extension_properties {
				next(physical_device, p_layer_name, p_property_count, p_properties)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);
	}

	// No layer name: append our extensions to the driver's list.
	if p_properties.is_null() {
		let result = with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.enumerate_device_extension_properties {
				next(physical_device, p_layer_name, p_property_count, p_properties)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);
		if result != VK_SUCCESS {
			return result;
		}
		*p_property_count += LAYER_EXTENSIONS.len() as u32;
		return VK_SUCCESS;
	}

	// Reserve space for our extensions.
	let caller_count = *p_property_count;
	let layer_count = LAYER_EXTENSIONS.len() as u32;
	*p_property_count = caller_count.saturating_sub(layer_count);

	let result = with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.enumerate_device_extension_properties {
			next(physical_device, p_layer_name, p_property_count, p_properties)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

	if result != VK_SUCCESS && result != VK_INCOMPLETE {
		return result;
	}

	let base_count = *p_property_count as usize;
	let remaining = (caller_count as usize).saturating_sub(base_count);
	let copy_count = remaining.min(LAYER_EXTENSIONS.len());
	for (i, ext) in LAYER_EXTENSIONS.iter().take(copy_count).enumerate() {
		*p_properties.add(base_count + i) = *ext;
	}
	*p_property_count = (base_count + copy_count) as u32;

	if copy_count < LAYER_EXTENSIONS.len() {
		VK_INCOMPLETE
	} else {
		result
	}
}

pub unsafe extern "C" fn get_physical_device_surface_formats(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_surface_format_count: *mut u32,
	p_surface_formats: *mut VkSurfaceFormatKHR,
) -> VkResult {
	let instance_key = instance_key_of(physical_device);

	let hdr_supported = get_wayland_connection(instance_key)
		.map(|arc| arc.force_lock().caps.hdr_supported)
		.unwrap_or(false);

	let call_icd = |count, buf| {
		with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.get_physical_device_surface_formats {
				next(physical_device, surface, count, buf)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
	};

	if !hdr_supported {
		return call_icd(p_surface_format_count, p_surface_formats);
	}

	append_hdr_formats(
		p_surface_format_count,
		p_surface_formats,
		call_icd,
		|ptr, offset, fmt, cs| {
			*ptr.add(offset) = VkSurfaceFormatKHR {
				format: fmt,
				color_space: cs,
			};
		},
	)
}

pub unsafe extern "C" fn get_physical_device_surface_formats2(
	physical_device: VkPhysicalDevice,
	p_surface_info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	p_surface_format_count: *mut u32,
	p_surface_formats: *mut VkSurfaceFormat2KHR,
) -> VkResult {
	let instance_key = instance_key_of(physical_device);

	let hdr_supported = get_wayland_connection(instance_key)
		.map(|arc| arc.force_lock().caps.hdr_supported)
		.unwrap_or(false);

	let call_icd = |count, buf| {
		with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.get_physical_device_surface_formats2 {
				next(physical_device, p_surface_info, count, buf)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
	};

	if !hdr_supported {
		return call_icd(p_surface_format_count, p_surface_formats);
	}

	append_hdr_formats(
		p_surface_format_count,
		p_surface_formats,
		call_icd,
		|ptr, offset, fmt, cs| {
			*ptr.add(offset) = VkSurfaceFormat2KHR {
				surface_format: VkSurfaceFormatKHR {
					format: fmt,
					color_space: cs,
				},
				..Default::default()
			};
		},
	)
}

/// Shared logic for appending HDR formats to a Vulkan enumeration buffer.
///
/// Handles the three cases: null buffer (count query), non-null buffer with
/// space, and non-null buffer without enough space (VK_INCOMPLETE).
unsafe fn append_hdr_formats<T>(
	p_count: *mut u32,
	p_buffer: *mut T,
	call_icd: impl Fn(*mut u32, *mut T) -> VkResult,
	write_element: impl Fn(*mut T, usize, ash::vk::Format, ash::vk::ColorSpaceKHR),
) -> VkResult {
	if p_buffer.is_null() {
		let result = call_icd(p_count, p_buffer);
		if result != VK_SUCCESS {
			return result;
		}
		*p_count += HDR_FORMATS.len() as u32;
		return VK_SUCCESS;
	}

	// Reserve space for HDR formats so the driver doesn't fill the whole buffer.
	let caller_count = *p_count;
	let hdr_count = HDR_FORMATS.len() as u32;
	*p_count = caller_count.saturating_sub(hdr_count);

	let result = call_icd(p_count, p_buffer);
	if result != VK_SUCCESS && result != VK_INCOMPLETE {
		return result;
	}

	// Append HDR formats after the driver's formats.
	let base_count = *p_count as usize;
	let remaining = (caller_count as usize).saturating_sub(base_count);
	let copy_count = remaining.min(HDR_FORMATS.len());
	for (i, &(fmt, cs)) in HDR_FORMATS.iter().take(copy_count).enumerate() {
		write_element(p_buffer, base_count + i, fmt, cs);
	}
	*p_count = (base_count + copy_count) as u32;

	if copy_count < HDR_FORMATS.len() {
		VK_INCOMPLETE
	} else {
		result
	}
}

// ---------------------------------------------------------------------------
// XWayland bypass implementation
// ---------------------------------------------------------------------------

/// Attempt to create a Wayland-backed Vulkan surface using a fresh `wl_surface`
/// on the Moonshine compositor, bypassing the XWayland Glamor path.
///
/// The ICD renders directly to the compositor's wl_surface, avoiding
/// XWayland's Glamor (GL) copy which would corrupt PQ-encoded HDR data
/// through sRGB linearization.
///
/// On success writes a valid `VkSurfaceKHR` into `*p_surface` and returns
/// `VK_SUCCESS`.  On any failure returns a non-SUCCESS code so the caller
/// can fall back to the normal XCB surface.
unsafe fn try_xwayland_bypass(
	instance: VkInstance,
	instance_key: InstanceKey,
	xcb_connection: *mut libc::c_void,
	xcb_window: u32,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	// Early exit if layer is degraded (no compositor connection).
	if !is_layer_active(instance_key) {
		return VK_ERROR_FEATURE_NOT_PRESENT;
	}

	// Get the layer's Wayland connection to the Moonshine compositor.
	let wl_arc = match get_wayland_connection(instance_key) {
		Some(arc) => arc,
		None => return VK_ERROR_FEATURE_NOT_PRESENT,
	};

	let wl = wl_arc.force_lock();
	if wl.dead {
		return VK_ERROR_FEATURE_NOT_PRESENT;
	}

	// Create a fresh wl_surface on the compositor.
	let wl_surface = wl.compositor.create_surface(&wl.qh, ());
	wl.connection.flush().ok();

	// Get the raw wl_display* and wl_surface* for the Vulkan call.
	let display_ptr = wl.connection.backend().display_ptr() as *mut std::ffi::c_void;
	let surface_ptr = wl_surface.id().as_ptr() as *mut std::ffi::c_void;

	// Move the wl_surface to the default event queue so the ICD's
	// wl_display_dispatch() calls can receive events (frame callbacks,
	// buffer releases, etc.) for this surface.  Without this, the surface
	// lives on our private queue and the ICD blocks forever.
	wl_proxy_set_queue(surface_ptr, std::ptr::null_mut());

	// Create a Vulkan Wayland surface backed by our bypass wl_surface.
	// The ICD will render directly to this surface, bypassing XWayland.
	let create_info = VkWaylandSurfaceCreateInfoKHR {
		s_type: ash::vk::StructureType::WAYLAND_SURFACE_CREATE_INFO_KHR,
		p_next: std::ptr::null(),
		flags: ash::vk::WaylandSurfaceCreateFlagsKHR::empty(),
		display: display_ptr,
		surface: surface_ptr,
		_marker: PhantomData,
	};

	let result = with_instance(instance_key, |data| {
		if let Some(next) = data.dispatch.create_wayland_surface {
			next(instance, &create_info, p_allocator, p_surface)
		} else {
			VK_ERROR_FEATURE_NOT_PRESENT
		}
	})
	.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

	if result != VK_SUCCESS {
		return result;
	}

	// Record the Wayland VkSurface.  The ICD renders to the wl_surface
	// directly; the xcb_window is kept for override_window_content mapping
	// and extent queries.
	crate::log_debug!("try_xwayland_bypass: created wl_surface for xcb_window={}", xcb_window);
	insert_surface(
		SurfaceKey::from_raw((*p_surface).as_raw()),
		SurfaceData {
			wl_surface,
			xcb_window: Some(xcb_window),
			xcb_connection,
		},
	);

	VK_SUCCESS
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ext_name_roundtrip() {
		let name = ext_name(b"VK_EXT_hdr_metadata\0");
		let cstr = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
		assert_eq!(cstr.to_bytes(), b"VK_EXT_hdr_metadata");
	}

	#[test]
	fn ext_name_zero_padded() {
		let name = ext_name(b"X\0");
		assert_eq!(name[0], b'X' as i8);
		assert_eq!(name[1], 0);
		assert_eq!(name[255], 0);
	}

	#[test]
	fn hdr_formats_are_non_empty() {
		assert!(!HDR_FORMATS.is_empty());
	}

	#[test]
	fn layer_extensions_are_non_empty() {
		assert!(!LAYER_EXTENSIONS.is_empty());
	}
}
