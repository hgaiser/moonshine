//! Moonshine swapchain Vulkan layer.
//!
//! This layer intercepts Vulkan presentation calls and routes game frames to
//! the Moonshine compositor via Wayland.  It replaces the forked
//! `VkLayer_FROG_gamescope_wsi` C++ layer.
//!
//! # Activation
//!
//! The implicit layer is enabled when `ENABLE_MOONSHINE_WSI=1` is set in the
//! process environment, matching the `enable_environment` entry in the layer's
//! JSON manifest.
//!
//! `MOONSHINE_WAYLAND_DISPLAY` must also be set to the Wayland socket name
//! that the Moonshine compositor is listening on; without it the layer starts
//! in degraded mode (no compositor connection, no bypass/HDR).
//!
//! `MOONSHINE_HDR=1` is set by the compositor when the session is HDR-capable;
//! the layer uses it to decide whether to advertise HDR surface formats and
//! remap color spaces for the ICD.
//!
//! # Architecture
//!
//! - [`instance`] — per-VkInstance state, Wayland connection setup.
//! - [`device`]   — per-VkDevice state, extension injection.
//! - [`surface`]  — per-VkSurfaceKHR state, XWayland bypass.
//! - [`swapchain`]— per-VkSwapchainKHR state, swapchain feedback protocol.
//! - [`dispatch`] — raw Vulkan loader dispatch table types and helpers.
//! - [`state`]    — global maps keyed by Vulkan handle.

#![allow(non_upper_case_globals)]
#![allow(clippy::missing_transmute_annotations)]
#![allow(clippy::missing_safety_doc)]

mod device;
mod dispatch;
mod instance;
mod log;
mod state;
mod surface;
mod swapchain;
mod xcb;

use ash::vk::Handle as _;
use dispatch::*;

/// The protocol bindings generated inline by wayland-scanner from
/// `protocols/moonshine-swapchain.xml`.
pub(crate) mod proto {
	// `use wayland_client;` puts `wayland_client` in this module's namespace
	// so the generated sub-modules can reference it via `super::wayland_client`.
	#[allow(clippy::single_component_path_imports)]
	use wayland_client;
	use wayland_client::protocol::*;

	pub mod __interfaces {
		use wayland_client::protocol::__interfaces::*;
		wayland_scanner::generate_interfaces!("protocols/moonshine-swapchain.xml");
	}
	use self::__interfaces::*;

	wayland_scanner::generate_client_code!("protocols/moonshine-swapchain.xml");
}

// ---------------------------------------------------------------------------
// C-ABI entry points required by the Vulkan loader
// ---------------------------------------------------------------------------

/// Called by the Vulkan loader to negotiate the layer interface version.
///
/// We support version 2 of the loader-layer interface, which provides
/// `pfnGetInstanceProcAddr` and `pfnGetDeviceProcAddr`.
#[no_mangle]
pub unsafe extern "C" fn vkNegotiateLoaderLayerInterfaceVersion(
	p_version_struct: *mut VkNegotiateLayerInterface,
) -> VkResult {
	if p_version_struct.is_null() {
		return VK_ERROR_INITIALIZATION_FAILED;
	}

	let version_struct = &mut *p_version_struct;

	// We support interface versions 2.
	if version_struct.loader_layer_interface_version > CURRENT_LOADER_LAYER_INTERFACE_VERSION {
		version_struct.loader_layer_interface_version = CURRENT_LOADER_LAYER_INTERFACE_VERSION;
	}

	if version_struct.loader_layer_interface_version >= 2 {
		version_struct.pfn_get_instance_proc_addr = Some(moonshine_vk_get_instance_proc_addr);
		version_struct.pfn_get_device_proc_addr = Some(moonshine_vk_get_device_proc_addr);
		version_struct.pfn_get_physical_device_proc_addr = None;
	}

	VK_SUCCESS
}

/// The layer's `vkGetInstanceProcAddr`.  Returns function pointers for
/// functions this layer intercepts; returns `None` for all others so the
/// loader chains to the next layer/driver.
#[no_mangle]
pub unsafe extern "C" fn moonshine_vk_get_instance_proc_addr(
	instance: VkInstance,
	p_name: *const std::ffi::c_char,
) -> PFN_vkVoidFunction {
	if p_name.is_null() {
		return None;
	}

	let name = std::ffi::CStr::from_ptr(p_name);

	// Functions that are always available (instance == VK_NULL_HANDLE).
	match name.to_bytes() {
		b"vkNegotiateLoaderLayerInterfaceVersion" => {
			return Some(std::mem::transmute::<
				unsafe extern "C" fn(*mut VkNegotiateLayerInterface) -> VkResult,
				_,
			>(vkNegotiateLoaderLayerInterfaceVersion));
		},
		b"vkGetInstanceProcAddr" => {
			return Some(std::mem::transmute::<
				unsafe extern "C" fn(VkInstance, *const std::ffi::c_char) -> PFN_vkVoidFunction,
				_,
			>(moonshine_vk_get_instance_proc_addr));
		},
		b"vkCreateInstance" => {
			return Some(std::mem::transmute::<
				unsafe extern "C" fn(
					*const VkInstanceCreateInfo,
					*const VkAllocationCallbacks,
					*mut VkInstance,
				) -> VkResult,
				_,
			>(instance::create_instance));
		},
		b"vkDestroyInstance" => {
			return Some(std::mem::transmute::<
				unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks),
				_,
			>(instance::destroy_instance));
		},
		_ => {},
	}

	// For other functions, only intercept if this instance is tracked by this
	// layer; if the instance was not created through our create_instance, fall
	// through to the next layer/driver without intercepting anything.
	if !instance.is_null() {
		if let Some(pfn) = state::with_instance(instance_key_of(instance), |data| data.dispatch.get_instance_proc_addr)
		{
			let intercepted = get_instance_intercept(name);
			if intercepted.is_some() {
				return intercepted;
			}

			// Fall through: ask the next layer.
			return pfn(instance, p_name);
		}
	}

	None
}

/// The layer's `vkGetDeviceProcAddr`.
#[no_mangle]
pub unsafe extern "C" fn moonshine_vk_get_device_proc_addr(
	device: VkDevice,
	p_name: *const std::ffi::c_char,
) -> PFN_vkVoidFunction {
	if p_name.is_null() || device.is_null() {
		return None;
	}

	let name = std::ffi::CStr::from_ptr(p_name);

	// Only intercept if this device is tracked by this layer.  If create_device
	// was not intercepted (e.g. the layer was inactive) there is no dispatch
	// table and any intercept we return would crash or misbehave.
	if let Some(pfn) = state::with_device(device_key_of(device), |data| data.dispatch.get_device_proc_addr) {
		let intercepted = get_device_intercept(name);
		if intercepted.is_some() {
			return intercepted;
		}

		// Fall through to next layer/driver.
		return pfn(device, p_name);
	}

	None
}

// ---------------------------------------------------------------------------
// Dispatch tables for intercepted functions
// ---------------------------------------------------------------------------

unsafe fn get_instance_intercept(name: &std::ffi::CStr) -> PFN_vkVoidFunction {
	macro_rules! intercept {
		($name:literal, $fn:expr, $ty:ty) => {
			if name.to_bytes() == $name {
				return Some(std::mem::transmute::<$ty, _>($fn));
			}
		};
	}

	intercept!(
		b"vkCreateDevice",
		device::create_device,
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const VkDeviceCreateInfo,
			*const VkAllocationCallbacks,
			*mut VkDevice,
		) -> VkResult
	);
	intercept!(
		b"vkCreateWaylandSurfaceKHR",
		surface::create_wayland_surface,
		unsafe extern "C" fn(
			VkInstance,
			*const VkWaylandSurfaceCreateInfoKHR,
			*const VkAllocationCallbacks,
			*mut VkSurface,
		) -> VkResult
	);
	intercept!(
		b"vkCreateXcbSurfaceKHR",
		surface::create_xcb_surface,
		unsafe extern "C" fn(
			VkInstance,
			*const VkXcbSurfaceCreateInfoKHR,
			*const VkAllocationCallbacks,
			*mut VkSurface,
		) -> VkResult
	);
	intercept!(
		b"vkCreateXlibSurfaceKHR",
		surface::create_xlib_surface,
		unsafe extern "C" fn(
			VkInstance,
			*const VkXlibSurfaceCreateInfoKHR,
			*const VkAllocationCallbacks,
			*mut VkSurface,
		) -> VkResult
	);
	intercept!(
		b"vkDestroySurfaceKHR",
		surface::destroy_surface,
		unsafe extern "C" fn(VkInstance, VkSurface, *const VkAllocationCallbacks)
	);
	intercept!(
		b"vkGetPhysicalDeviceSurfaceFormatsKHR",
		surface::get_physical_device_surface_formats,
		unsafe extern "C" fn(VkPhysicalDevice, VkSurface, *mut u32, *mut VkSurfaceFormatKHR) -> VkResult
	);
	intercept!(
		b"vkGetPhysicalDeviceSurfaceFormats2KHR",
		surface::get_physical_device_surface_formats2,
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const VkPhysicalDeviceSurfaceInfo2KHR,
			*mut u32,
			*mut VkSurfaceFormat2KHR,
		) -> VkResult
	);
	intercept!(
		b"vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
		surface::get_physical_device_surface_capabilities,
		unsafe extern "C" fn(VkPhysicalDevice, VkSurface, *mut VkSurfaceCapabilitiesKHR) -> VkResult
	);
	intercept!(
		b"vkGetPhysicalDeviceSurfaceCapabilities2KHR",
		surface::get_physical_device_surface_capabilities2,
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const VkPhysicalDeviceSurfaceInfo2KHR,
			*mut ash::vk::SurfaceCapabilities2KHR,
		) -> VkResult
	);
	intercept!(
		b"vkGetPhysicalDeviceSurfacePresentModesKHR",
		surface::get_physical_device_surface_present_modes,
		unsafe extern "C" fn(VkPhysicalDevice, VkSurface, *mut u32, *mut ash::vk::PresentModeKHR) -> VkResult
	);
	intercept!(
		b"vkGetPhysicalDeviceXcbPresentationSupportKHR",
		surface::get_physical_device_xcb_presentation_support,
		unsafe extern "C" fn(VkPhysicalDevice, u32, *mut std::ffi::c_void, u32) -> ash::vk::Bool32
	);
	intercept!(
		b"vkGetPhysicalDeviceXlibPresentationSupportKHR",
		surface::get_physical_device_xlib_presentation_support,
		unsafe extern "C" fn(VkPhysicalDevice, u32, *mut std::ffi::c_void, u64) -> ash::vk::Bool32
	);
	intercept!(
		b"vkEnumerateDeviceExtensionProperties",
		surface::enumerate_device_extension_properties,
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const std::ffi::c_char,
			*mut u32,
			*mut ash::vk::ExtensionProperties,
		) -> VkResult
	);

	// Device-level functions also returned from vkGetInstanceProcAddr.
	// Required because some Vulkan wrappers (e.g. winevulkan) resolve
	// device functions via vkGetInstanceProcAddr.
	let device = get_device_intercept(name);
	if device.is_some() {
		return device;
	}

	None
}

unsafe fn get_device_intercept(name: &std::ffi::CStr) -> PFN_vkVoidFunction {
	macro_rules! intercept {
		($name:literal, $fn:expr, $ty:ty) => {
			if name.to_bytes() == $name {
				return Some(std::mem::transmute::<$ty, _>($fn));
			}
		};
	}

	intercept!(
		b"vkGetDeviceProcAddr",
		moonshine_vk_get_device_proc_addr,
		unsafe extern "C" fn(VkDevice, *const std::ffi::c_char) -> PFN_vkVoidFunction
	);
	intercept!(
		b"vkDestroyDevice",
		device::destroy_device,
		unsafe extern "C" fn(VkDevice, *const VkAllocationCallbacks)
	);
	intercept!(
		b"vkCreateSwapchainKHR",
		swapchain::create_swapchain,
		unsafe extern "C" fn(
			VkDevice,
			*const VkSwapchainCreateInfoKHR,
			*const VkAllocationCallbacks,
			*mut VkSwapchain,
		) -> VkResult
	);
	intercept!(
		b"vkDestroySwapchainKHR",
		swapchain::destroy_swapchain,
		unsafe extern "C" fn(VkDevice, VkSwapchain, *const VkAllocationCallbacks)
	);
	intercept!(
		b"vkQueuePresentKHR",
		swapchain::queue_present,
		unsafe extern "C" fn(VkQueue, *const VkPresentInfoKHR) -> VkResult
	);
	intercept!(
		b"vkSetHdrMetadataEXT",
		swapchain::set_hdr_metadata,
		unsafe extern "C" fn(VkDevice, u32, *const VkSwapchain, *const VkHdrMetadataEXT)
	);
	intercept!(
		b"vkAcquireNextImageKHR",
		swapchain::acquire_next_image,
		unsafe extern "C" fn(VkDevice, VkSwapchain, u64, ash::vk::Semaphore, ash::vk::Fence, *mut u32) -> VkResult
	);
	intercept!(
		b"vkAcquireNextImage2KHR",
		swapchain::acquire_next_image2,
		unsafe extern "C" fn(VkDevice, *const ash::vk::AcquireNextImageInfoKHR, *mut u32) -> VkResult
	);
	intercept!(
		b"vkGetRefreshCycleDurationGOOGLE",
		swapchain::get_refresh_cycle_duration,
		unsafe extern "C" fn(VkDevice, VkSwapchain, *mut ash::vk::RefreshCycleDurationGOOGLE) -> VkResult
	);
	intercept!(
		b"vkGetPastPresentationTimingGOOGLE",
		swapchain::get_past_presentation_timing,
		unsafe extern "C" fn(VkDevice, VkSwapchain, *mut u32, *mut ash::vk::PastPresentationTimingGOOGLE) -> VkResult
	);

	None
}
