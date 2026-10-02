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
use wayland_client::protocol::wl_surface::WlSurface;

use crate::dispatch::*;
use crate::instance::connect_to_foreign_display;
use crate::state::{
	InstanceKey, MutexExt, SurfaceData, SurfaceKey, get_wayland_connection, insert_surface, is_layer_active,
	remove_surface, with_instance, with_surface,
};
use crate::swapchain::can_bypass_xwayland;
use crate::xcb::{xcb_get_window_extent, xlib_to_xcb_connection};

/// A proxy drop does not send wl_surface.destroy. Roll back only until the
/// ICD has successfully created its surface; after that, live SurfaceData owns
/// the protocol destructor (after destroying the Vulkan surface).
struct TemporarySurface<T, F: Fn(&T)> {
	surface: Option<T>,
	destroy: F,
}

impl<T, F: Fn(&T)> TemporarySurface<T, F> {
	fn new(surface: T, destroy: F) -> Self {
		Self {
			surface: Some(surface),
			destroy,
		}
	}
	fn get(&self) -> &T {
		self.surface.as_ref().expect("temporary surface still owned")
	}
	fn transfer(mut self) -> T {
		self.surface.take().expect("temporary surface still owned")
	}
}

fn construct_surface<T>(surface: T, destroy: impl Fn(&T), constructor: impl FnOnce(&T) -> VkResult) -> Option<T> {
	let temporary = TemporarySurface::new(surface, destroy);
	if constructor(temporary.get()) != VK_SUCCESS {
		return None;
	}
	Some(temporary.transfer())
}

impl<T, F: Fn(&T)> Drop for TemporarySurface<T, F> {
	fn drop(&mut self) {
		if let Some(surface) = self.surface.take() {
			(self.destroy)(&surface);
		}
	}
}

/// Whether to bind the moonshine swapchain on the application's own Wayland
/// display for native Wayland surfaces.  Opt-in until validated, since it
/// takes over presentation for native Wayland clients.
fn native_wayland_enabled() -> bool {
	use std::sync::OnceLock;
	static ENABLED: OnceLock<bool> = OnceLock::new();
	*ENABLED.get_or_init(|| {
		std::env::var("MOONSHINE_WSI_NATIVE_WAYLAND")
			.ok()
			.map(|v| v.trim() == "1")
			.unwrap_or(false)
	})
}

unsafe extern "C" {
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
	unsafe {
		let instance_key = instance_key_of(instance);
		let create_info = &*p_create_info;

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

		// The app's wl_surface lives on its own Wayland connection, so bind the
		// swapchain factory there.  We can then create a moonshine_swapchain for
		// the app's surface.  If any step fails the surface is left unrecorded
		// and the app presents through the ICD.
		if native_wayland_enabled()
			&& is_layer_active(instance_key)
			&& !(*p_surface).is_null()
			&& let Some(hdr_supported) = get_wayland_connection(instance_key).map(|a| a.force_lock().caps.hdr_supported)
			&& let Some(native) = connect_to_foreign_display(create_info.display, create_info.surface, hdr_supported)
		{
			crate::log_info!("vkCreateWaylandSurfaceKHR: native Wayland surface recorded");
			insert_surface(
				SurfaceKey::from_raw((*p_surface).as_raw()),
				SurfaceData {
					wl_surface: native.wl_surface.clone(),
					xcb_window: None,
					xcb_connection: std::ptr::null_mut(),
					fallback_surface: VkSurface::null(),
					bypass_watch: None,
					native: Some(native),
				},
			);
		}

		VK_SUCCESS
	}
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
	unsafe {
		let instance_key = instance_key_of(instance);
		let create_info = &*p_create_info;

		// Runtime toggle to disable the XWayland bypass entirely.
		if std::env::var("MOONSHINE_WSI_DISABLE_BYPASS")
			.ok()
			.map(|v| v.trim() == "1")
			.unwrap_or(false)
		{
			crate::log_info!("XWayland bypass disabled by MOONSHINE_WSI_DISABLE_BYPASS");
			return with_instance(instance_key, |data| {
				if let Some(next) = data.dispatch.create_xcb_surface {
					next(instance, p_create_info, p_allocator, p_surface)
				} else {
					VK_ERROR_FEATURE_NOT_PRESENT
				}
			})
			.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);
		}

		// Try XWayland bypass: create a wl_surface on the Moonshine compositor.
		if let Some(wl_surface) =
			try_xwayland_bypass(instance, instance_key, create_info.window, p_allocator, p_surface)
		{
			// Also create the plain XCB surface, so presentation can fall back
			// to XWayland when the bypass safety checks refuse this window
			// (launchers, Wine offscreen/GDI-blit windows, child windows, ...).
			let fallback_surface = create_plain_xcb_surface(instance_key, instance, p_create_info, p_allocator);

			if fallback_surface.is_null() {
				// A managed bypass must always have a usable fallback. Never hand
				// out a replacement that can become an unmapped black surface.
				with_instance(instance_key, |data| {
					if let Some(destroy) = data.dispatch.destroy_surface {
						destroy(instance, *p_surface, p_allocator);
					}
				});
				wl_surface.destroy();
				*p_surface = VkSurface::null();
				return VK_ERROR_INITIALIZATION_FAILED;
			}
			let bypass_watch = crate::xcb::BypassWatch::new(create_info.connection, create_info.window)
				.map(|w| std::sync::Arc::new(std::sync::Mutex::new(w)));
			if bypass_watch.is_none() {
				crate::log_debug!(
					"XWayland bypass rejected: X11 topology/event monitoring unavailable (window={})",
					create_info.window
				);
			}
			insert_surface(
				SurfaceKey::from_raw((*p_surface).as_raw()),
				SurfaceData {
					wl_surface,
					xcb_window: Some(create_info.window),
					xcb_connection: create_info.connection,
					fallback_surface,
					bypass_watch,
					native: None,
				},
			);

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
}

/// Create a plain XCB surface for the window via the next layer/ICD.
///
/// Returns a null handle if the ICD does not expose `vkCreateXcbSurfaceKHR`
/// or rejects the request.
unsafe fn create_plain_xcb_surface(
	instance_key: InstanceKey,
	instance: VkInstance,
	p_create_info: *const VkXcbSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
) -> VkSurface {
	unsafe {
		let mut surface = VkSurface::null();
		let result = with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.create_xcb_surface {
				next(instance, p_create_info, p_allocator, &mut surface)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

		if result == VK_SUCCESS {
			surface
		} else {
			VkSurface::null()
		}
	}
}

/// Returns the XCB fallback surface to present through when XWayland bypass is
/// not safe for `surface`, or `None` to use `surface` as-is.
///
/// A `None` result also means the surface is not layer-managed (e.g. a native
/// Wayland surface), in which case the app's own surface is used.
pub(crate) unsafe fn icd_fallback_surface(surface: VkSurface) -> Option<VkSurface> {
	unsafe {
		let key = SurfaceKey::from_raw(surface.as_raw());
		let fallback = with_surface(key, |sd| sd.fallback_surface)?;
		fallback_for_policy(fallback, can_bypass_xwayland(key))
	}
}

fn fallback_for_policy(fallback: VkSurface, bypass_safe: bool) -> Option<VkSurface> {
	(!fallback.is_null() && !bypass_safe).then_some(fallback)
}

/// Convert a libX11 `Display*` to an `xcb_connection_t*` using
/// `XGetXCBConnection` from `libX11-xcb`.
pub unsafe extern "C" fn create_xlib_surface(
	instance: VkInstance,
	p_create_info: *const VkXlibSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	unsafe {
		let create_info = &*p_create_info;

		let instance_key = instance_key_of(instance);
		// Conversion to XCB is private to the layer and requires that extension
		// to have been enabled downstream, even when an ICD returns a pointer.
		let can_convert = with_instance(instance_key, |d| {
			d.downstream_extensions
				.iter()
				.any(|name| name.as_c_str() == c"VK_KHR_xcb_surface")
		})
		.unwrap_or(false);
		if !can_convert {
			return with_instance(instance_key, |d| {
				d.dispatch
					.create_xlib_surface
					.map(|f| f(instance, p_create_info, p_allocator, p_surface))
			})
			.flatten()
			.unwrap_or(VK_ERROR_EXTENSION_NOT_PRESENT);
		}
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
}

pub unsafe extern "C" fn destroy_surface(
	instance: VkInstance,
	surface: VkSurface,
	p_allocator: *const VkAllocationCallbacks,
) {
	unsafe {
		let instance_key = instance_key_of(instance);
		let surface_key = SurfaceKey::from_raw(surface.as_raw());

		// Also destroy the XCB fallback surface created for this window.
		let fallback_surface = with_surface(surface_key, |sd| sd.fallback_surface);
		remove_surface(surface_key);

		with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.destroy_surface {
				if let Some(fallback) = fallback_surface
					&& !fallback.is_null()
				{
					next(instance, fallback, p_allocator);
				}
				next(instance, surface, p_allocator);
			}
		});
	}
}

// ---------------------------------------------------------------------------
// Surface format/capabilities hooks
// ---------------------------------------------------------------------------

/// Only replacement Wayland surfaces participate in legacy compatibility.
/// Native Wayland and the dynamically selected XCB fallback retain ICD limits.
pub(crate) fn is_bypass_surface(surface: VkSurface) -> bool {
	with_surface(SurfaceKey::from_raw(surface.as_raw()), |s| s.xcb_window.is_some()).unwrap_or(false)
}

unsafe fn adjust_image_count(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	caps: &mut VkSurfaceCapabilitiesKHR,
	mode_specific: bool,
) {
	unsafe {
		static MINIMUM_WARNING: std::sync::OnceLock<()> = std::sync::OnceLock::new();
		MINIMUM_WARNING.get_or_init(|| {
			if std::env::var_os("MOONSHINE_WSI_MIN_IMAGE_COUNT").is_some() {
				crate::log_warn!(
					"MOONSHINE_WSI_MIN_IMAGE_COUNT is deprecated and ignored: image-count capabilities now follow the driver"
				);
			}
		});
		let legacy_min = caps.min_image_count;
		let legacy_max = caps.max_image_count;
		let eligible = !mode_specific
			&& is_bypass_surface(surface)
			&& crate::state::maintenance_enabled_for_physical_device(physical_device);
		let fifo = if eligible && legacy_min > 3 {
			crate::image_count::query_mode(physical_device, surface, VkPresentModeKHR::FIFO)
		} else {
			None
		};
		let applied = !mode_specific && crate::image_count::compatibility_range(caps, fifo.as_ref().map(|m| &m.caps));
		crate::log_debug!(
			"surface capabilities: mode_specific={} driver minImageCount={} maxImageCount={} advertised minImageCount={} maxImageCount={} compatibility_applied={} maintenance_device={}",
			mode_specific,
			legacy_min,
			legacy_max,
			caps.min_image_count,
			caps.max_image_count,
			applied,
			eligible
		);
	}
}

/// For XWayland bypass surfaces the ICD returns extent=0xFFFFFFFF (undefined)
/// because the bare wl_surface has no role.  Override with the X11 window size
/// so DXVK/the app can create a correctly-sized swapchain.
unsafe fn override_extent_from_xcb(surface_key: SurfaceKey, caps: &mut ash::vk::SurfaceCapabilitiesKHR) {
	unsafe {
		with_surface(surface_key, |sd| {
			if let Some(window) = sd.xcb_window
				&& let Some((w, h)) = xcb_get_window_extent(sd.xcb_connection, window)
			{
				caps.current_extent = ash::vk::Extent2D { width: w, height: h };
			}
		});
	}
}

pub unsafe extern "C" fn get_physical_device_surface_capabilities(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_surface_capabilities: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
	unsafe {
		let instance_key = instance_key_of(physical_device);

		let fallback = icd_fallback_surface(surface);
		let icd_surface = fallback.unwrap_or(surface);

		let result = with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.get_physical_device_surface_capabilities {
				next(physical_device, icd_surface, p_surface_capabilities)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

		if result != VK_SUCCESS {
			return result;
		}

		let caps = &mut *p_surface_capabilities;
		adjust_image_count(physical_device, icd_surface, caps, false);
		// The ICD reports the real extent for the XCB fallback surface; only the
		// bypass surface (which has no role yet) needs the window size substituted.
		if fallback.is_none() {
			override_extent_from_xcb(SurfaceKey::from_raw(surface.as_raw()), caps);
		}

		VK_SUCCESS
	}
}

pub unsafe extern "C" fn get_physical_device_surface_capabilities2(
	physical_device: VkPhysicalDevice,
	p_surface_info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	p_surface_capabilities: *mut VkSurfaceCapabilities2KHR,
) -> VkResult {
	unsafe {
		let instance_key = instance_key_of(physical_device);

		let fallback = icd_fallback_surface((*p_surface_info).surface);
		let mut icd_surface_info = *p_surface_info;
		if let Some(fb) = fallback {
			icd_surface_info.surface = fb;
		}

		let result = with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.get_physical_device_surface_capabilities2 {
				next(physical_device, &icd_surface_info, p_surface_capabilities)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED);

		if result != VK_SUCCESS {
			return result;
		}

		let caps = &mut (*p_surface_capabilities).surface_capabilities;
		let mode_specific = find_in_chain::<ash::vk::SurfacePresentModeEXT>(
			(*p_surface_info).p_next,
			ash::vk::StructureType::SURFACE_PRESENT_MODE_EXT,
		)
		.is_some();
		adjust_image_count(physical_device, icd_surface_info.surface, caps, mode_specific);
		if fallback.is_none() {
			override_extent_from_xcb(SurfaceKey::from_raw((*p_surface_info).surface.as_raw()), caps);
		}

		VK_SUCCESS
	}
}

pub unsafe extern "C" fn get_physical_device_surface_present_modes(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_present_mode_count: *mut u32,
	p_present_modes: *mut VkPresentModeKHR,
) -> VkResult {
	unsafe {
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

		let icd_surface = icd_fallback_surface(surface).unwrap_or(surface);

		with_instance(instance_key, |data| {
			if let Some(next) = data.dispatch.get_physical_device_surface_present_modes {
				next(physical_device, icd_surface, p_present_mode_count, p_present_modes)
			} else {
				VK_ERROR_FEATURE_NOT_PRESENT
			}
		})
		.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
	}
}

pub unsafe extern "C" fn get_physical_device_xcb_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family_index: u32,
	connection: *mut std::ffi::c_void,
	visual_id: u32,
) -> ash::vk::Bool32 {
	unsafe {
		let instance_key = instance_key_of(physical_device);

		// Active mode: redirect to Wayland presentation support using Moonshine's display.
		if with_instance(instance_key, |d| {
			d.downstream_extensions
				.iter()
				.any(|name| name.as_c_str() == c"VK_KHR_wayland_surface")
		})
		.unwrap_or(false)
			&& let Some(arc) = get_wayland_connection(instance_key)
		{
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
}

pub unsafe extern "C" fn get_physical_device_xlib_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family_index: u32,
	dpy: *mut std::ffi::c_void,
	visual_id: u64,
) -> ash::vk::Bool32 {
	unsafe {
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
	let mut buf = [0 as std::ffi::c_char; 256];
	let mut i = 0;
	while i < name.len() && i < 255 {
		buf[i] = name[i] as std::ffi::c_char;
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
	unsafe {
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
}

pub unsafe extern "C" fn get_physical_device_surface_formats(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_surface_format_count: *mut u32,
	p_surface_formats: *mut VkSurfaceFormatKHR,
) -> VkResult {
	unsafe {
		let instance_key = instance_key_of(physical_device);

		let hdr_supported = get_wayland_connection(instance_key)
			.map(|arc| arc.force_lock().caps.hdr_supported)
			.unwrap_or(false);

		let fallback = icd_fallback_surface(surface);
		let icd_surface = fallback.unwrap_or(surface);

		let call_icd = |count, buf| {
			with_instance(instance_key, |data| {
				if let Some(next) = data.dispatch.get_physical_device_surface_formats {
					next(physical_device, icd_surface, count, buf)
				} else {
					VK_ERROR_FEATURE_NOT_PRESENT
				}
			})
			.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
		};

		// HDR formats only apply on the bypass path; the XCB fallback goes
		// through XWayland's Glamor compositing, which cannot carry HDR.
		if !hdr_supported || fallback.is_some() {
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
}

pub unsafe extern "C" fn get_physical_device_surface_formats2(
	physical_device: VkPhysicalDevice,
	p_surface_info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	p_surface_format_count: *mut u32,
	p_surface_formats: *mut VkSurfaceFormat2KHR,
) -> VkResult {
	unsafe {
		let instance_key = instance_key_of(physical_device);

		let hdr_supported = get_wayland_connection(instance_key)
			.map(|arc| arc.force_lock().caps.hdr_supported)
			.unwrap_or(false);

		let fallback = icd_fallback_surface((*p_surface_info).surface);
		let mut icd_surface_info = *p_surface_info;
		if let Some(fb) = fallback {
			icd_surface_info.surface = fb;
		}

		let call_icd = |count, buf| {
			with_instance(instance_key, |data| {
				if let Some(next) = data.dispatch.get_physical_device_surface_formats2 {
					next(physical_device, &icd_surface_info, count, buf)
				} else {
					VK_ERROR_FEATURE_NOT_PRESENT
				}
			})
			.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
		};

		if !hdr_supported || fallback.is_some() {
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
	unsafe {
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
	xcb_window: u32,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> Option<WlSurface> {
	unsafe {
		// Early exit if layer is degraded (no compositor connection).
		if !is_layer_active(instance_key)
			|| !with_instance(instance_key, |d| {
				[c"VK_KHR_surface", c"VK_KHR_wayland_surface", c"VK_KHR_xcb_surface"]
					.iter()
					.all(|ext| d.downstream_extensions.iter().any(|name| name.as_c_str() == *ext))
			})
			.unwrap_or(false)
		{
			return None;
		}

		// Get the layer's Wayland connection to the Moonshine compositor.
		let wl_arc = get_wayland_connection(instance_key)?;

		let wl = wl_arc.force_lock();
		if wl.dead {
			return None;
		}

		// Create a fresh wl_surface on the compositor.
		let wl_surface = construct_surface(
			wl.compositor.create_surface(&wl.qh, ()),
			|surface: &WlSurface| {
				surface.destroy();
				wl.connection.flush().ok();
			},
			|wl_surface| {
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

				with_instance(instance_key, |data| {
					if let Some(next) = data.dispatch.create_wayland_surface {
						next(instance, &create_info, p_allocator, p_surface)
					} else {
						VK_ERROR_FEATURE_NOT_PRESENT
					}
				})
				.unwrap_or(VK_ERROR_INITIALIZATION_FAILED)
			},
		)?;

		crate::log_debug!("try_xwayland_bypass: created wl_surface for xcb_window={}", xcb_window);
		Some(wl_surface)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unsafe_policy_selects_plain_xcb_surface() {
		let xcb = VkSurface::from_raw(0x1234);
		assert_eq!(fallback_for_policy(xcb, false), Some(xcb));
		assert_eq!(fallback_for_policy(xcb, true), None);
		// Native/unmanaged surfaces have no replacement to select.
		assert_eq!(fallback_for_policy(VkSurface::null(), true), None);
		assert_eq!(fallback_for_policy(VkSurface::null(), false), None);
	}

	#[test]
	fn ext_name_roundtrip() {
		let name = ext_name(b"VK_EXT_hdr_metadata\0");
		let cstr = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
		assert_eq!(cstr.to_bytes(), b"VK_EXT_hdr_metadata");
	}

	#[test]
	fn ext_name_zero_padded() {
		let name = ext_name(b"X\0");
		assert_eq!(name[0], b'X' as std::ffi::c_char);
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

#[cfg(test)]
mod protocol_lifecycle_tests {
	use super::*;
	use crate::state::WaylandState;
	use std::sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	};
	use wayland_server::protocol::{wl_compositor, wl_surface};
	use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};

	#[derive(Default)]
	struct Counts {
		creates: AtomicUsize,
		destroys: AtomicUsize,
	}
	struct Server(Arc<Counts>);
	impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Server {
		fn bind(
			_: &mut Self,
			_: &DisplayHandle,
			_: &Client,
			resource: New<wl_compositor::WlCompositor>,
			_: &(),
			init: &mut DataInit<'_, Self>,
		) {
			init.init(resource, ());
		}
	}
	impl Dispatch<wl_compositor::WlCompositor, ()> for Server {
		fn request(
			state: &mut Self,
			_: &Client,
			_: &wl_compositor::WlCompositor,
			request: wl_compositor::Request,
			_: &(),
			_: &DisplayHandle,
			init: &mut DataInit<'_, Self>,
		) {
			if let wl_compositor::Request::CreateSurface { id } = request {
				state.0.creates.fetch_add(1, Ordering::SeqCst);
				init.init(id, ());
			} else {
				panic!("unexpected compositor request");
			}
		}
	}
	impl Dispatch<wl_surface::WlSurface, ()> for Server {
		fn request(
			state: &mut Self,
			_: &Client,
			_: &wl_surface::WlSurface,
			request: wl_surface::Request,
			_: &(),
			_: &DisplayHandle,
			_: &mut DataInit<'_, Self>,
		) {
			assert!(matches!(request, wl_surface::Request::Destroy));
			state.0.destroys.fetch_add(1, Ordering::SeqCst);
		}
	}
	struct Fixture {
		connection: wayland_client::Connection,
		queue: wayland_client::EventQueue<WaylandState>,
		compositor: wayland_client::protocol::wl_compositor::WlCompositor,
		counts: Arc<Counts>,
		stop: Arc<AtomicBool>,
		server: Option<std::thread::JoinHandle<()>>,
	}
	impl Fixture {
		fn new() -> Self {
			let (client, socket) = std::os::unix::net::UnixStream::pair().unwrap();
			let counts = Arc::new(Counts::default());
			let stop = Arc::new(AtomicBool::new(false));
			let server_counts = counts.clone();
			let server_stop = stop.clone();
			let mut display = Display::<Server>::new().unwrap();
			display
				.handle()
				.create_global::<Server, wl_compositor::WlCompositor, ()>(4, ());
			display.handle().insert_client(socket, Arc::new(())).unwrap();
			let server = std::thread::spawn(move || {
				let mut state = Server(server_counts);
				let started = std::time::Instant::now();
				while !server_stop.load(Ordering::SeqCst) && started.elapsed() < std::time::Duration::from_secs(5) {
					display.dispatch_clients(&mut state).unwrap();
					display.flush_clients().unwrap();
					std::thread::sleep(std::time::Duration::from_millis(1));
				}
			});
			let connection = wayland_client::Connection::from_socket(client).unwrap();
			let (globals, queue) = wayland_client::globals::registry_queue_init::<WaylandState>(&connection).unwrap();
			let compositor = globals.bind(&queue.handle(), 1..=4, ()).unwrap();
			Self {
				connection,
				queue,
				compositor,
				counts,
				stop,
				server: Some(server),
			}
		}
		fn sync(&mut self, creates: usize, destroys: usize) {
			self.queue.roundtrip(&mut WaylandState).unwrap();
			assert_eq!(self.counts.creates.load(Ordering::SeqCst), creates);
			assert_eq!(self.counts.destroys.load(Ordering::SeqCst), destroys);
		}
	}
	impl Drop for Fixture {
		fn drop(&mut self) {
			self.stop.store(true, Ordering::SeqCst);
			self.server.take().unwrap().join().unwrap();
		}
	}
	#[test]
	fn temporary_surfaces_are_destroyed_on_every_rollback_stage_and_retry() {
		let mut fixture = Fixture::new();
		let mut count = 0;
		// Failure before flush, after flush, after queue migration, missing
		// instance/dispatch, and each failed ICD constructor result.
		for _retry in 0..20 {
			for stage in 0..7 {
				let failed = construct_surface(
					fixture.compositor.create_surface(&fixture.queue.handle(), ()),
					|surface: &WlSurface| {
						surface.destroy();
						fixture.connection.flush().unwrap();
					},
					|surface| {
						if stage > 0 {
							fixture.connection.flush().unwrap();
						}
						if stage > 1 {
							unsafe {
								wl_proxy_set_queue(surface.id().as_ptr().cast(), std::ptr::null_mut());
							}
						}
						match stage {
							3 => VK_ERROR_INITIALIZATION_FAILED,
							4 => VK_ERROR_FEATURE_NOT_PRESENT,
							5 => VK_ERROR_DEVICE_LOST,
							6 => ash::vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
							_ => ash::vk::Result::ERROR_OUT_OF_HOST_MEMORY,
						}
					},
				);
				assert!(failed.is_none());
				count += 1;
				fixture.sync(count, count);
			}
		}
	}
	#[test]
	fn successful_transfer_defers_protocol_destroy_until_live_owner_teardown() {
		let mut fixture = Fixture::new();
		for count in 1..=20 {
			let live_surface = construct_surface(
				fixture.compositor.create_surface(&fixture.queue.handle(), ()),
				|surface: &WlSurface| {
					surface.destroy();
					fixture.connection.flush().unwrap();
				},
				|surface| {
					unsafe {
						wl_proxy_set_queue(surface.id().as_ptr().cast(), std::ptr::null_mut());
					}
					VK_SUCCESS
				},
			)
			.unwrap();
			fixture.sync(count, count - 1);
			// Represents SurfaceData teardown after vkDestroySurfaceKHR.
			live_surface.destroy();
			fixture.sync(count, count);
		}
	}
}
