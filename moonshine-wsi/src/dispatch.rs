//! Raw Vulkan loader-layer dispatch types.
//!
//! The Vulkan loader dispatch mechanism requires access to a few private
//! structs that are defined in the Vulkan SDK headers (`vulkan/vk_layer.h`)
//! but not exposed by `ash`.  We redeclare them here with `repr(C)` to
//! match the C ABI.

#![allow(non_camel_case_types, dead_code)]

// Re-export commonly used ash Vulkan types.
pub use ash::vk::{
	AllocationCallbacks as VkAllocationCallbacks, Device as VkDevice, DeviceCreateInfo as VkDeviceCreateInfo,
	HdrMetadataEXT as VkHdrMetadataEXT, Instance as VkInstance, InstanceCreateInfo as VkInstanceCreateInfo,
	PhysicalDevice as VkPhysicalDevice, PhysicalDeviceSurfaceInfo2KHR as VkPhysicalDeviceSurfaceInfo2KHR,
	PresentInfoKHR as VkPresentInfoKHR, PresentModeKHR as VkPresentModeKHR, Queue as VkQueue, Result as VkResult,
	StructureType, SurfaceCapabilities2KHR as VkSurfaceCapabilities2KHR,
	SurfaceCapabilitiesKHR as VkSurfaceCapabilitiesKHR, SurfaceFormat2KHR as VkSurfaceFormat2KHR,
	SurfaceFormatKHR as VkSurfaceFormatKHR, SurfaceKHR as VkSurface,
	SwapchainCreateInfoKHR as VkSwapchainCreateInfoKHR, SwapchainKHR as VkSwapchain,
	WaylandSurfaceCreateInfoKHR as VkWaylandSurfaceCreateInfoKHR, XcbSurfaceCreateInfoKHR as VkXcbSurfaceCreateInfoKHR,
};

// ash 0.38: VkResult constants are associated consts of ash::vk::Result.
pub const VK_SUCCESS: ash::vk::Result = ash::vk::Result::SUCCESS;
pub const VK_INCOMPLETE: ash::vk::Result = ash::vk::Result::INCOMPLETE;
pub const VK_ERROR_INITIALIZATION_FAILED: ash::vk::Result = ash::vk::Result::ERROR_INITIALIZATION_FAILED;
pub const VK_ERROR_EXTENSION_NOT_PRESENT: ash::vk::Result = ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT;
pub const VK_ERROR_FEATURE_NOT_PRESENT: ash::vk::Result = ash::vk::Result::ERROR_FEATURE_NOT_PRESENT;
pub const VK_ERROR_DEVICE_LOST: ash::vk::Result = ash::vk::Result::ERROR_DEVICE_LOST;
pub const VK_ERROR_OUT_OF_DATE_KHR: ash::vk::Result = ash::vk::Result::ERROR_OUT_OF_DATE_KHR;

/// Current loader-layer interface version we support.
pub const CURRENT_LOADER_LAYER_INTERFACE_VERSION: u32 = 2;

/// Vulkan function pointer type alias.
pub type PFN_vkVoidFunction = Option<unsafe extern "C" fn()>;
pub type PFN_vkGetInstanceProcAddr = unsafe extern "C" fn(VkInstance, *const std::ffi::c_char) -> PFN_vkVoidFunction;
pub type PFN_vkGetDeviceProcAddr = unsafe extern "C" fn(VkDevice, *const std::ffi::c_char) -> PFN_vkVoidFunction;

/// Check whether an extension name is present in a raw `ppEnabledExtensionNames` slice.
///
/// # Safety
/// Each non-null pointer in `exts` must point to a valid null-terminated C string.
pub unsafe fn has_extension(exts: &[*const i8], name: &std::ffi::CStr) -> bool {
	exts.iter()
		.any(|&p| !p.is_null() && std::ffi::CStr::from_ptr(p) == name)
}

/// `XlibSurfaceCreateInfoKHR` is not in ash on all platforms; we declare a
/// minimal version here just to get a function pointer type.
#[repr(C)]
pub struct VkXlibSurfaceCreateInfoKHR {
	pub s_type: StructureType,
	pub p_next: *const std::ffi::c_void,
	pub flags: u32,
	pub dpy: *mut std::ffi::c_void, // Display*
	pub window: u64,                // Window (XID)
}

// ---------------------------------------------------------------------------
// Loader private structs (from vulkan/vk_layer.h)
// ---------------------------------------------------------------------------

/// Passed by the loader through the `pNext` chain of `VkInstanceCreateInfo`
/// during `vkCreateInstance` so the layer can retrieve the "next" proc addr.
///
/// The `u` field is a union; for `function == VK_LAYER_LINK_INFO` the active
/// member is `p_layer_info` (a pointer to [`VkLayerInstanceLink`]).
#[repr(C)]
pub struct VkLayerInstanceCreateInfo {
	pub s_type: StructureType,
	pub p_next: *const std::ffi::c_void,
	pub function: VkLayerFunction,
	/// Union — interpret as `*mut VkLayerInstanceLink` when
	/// `function == LinkInfo`.
	pub p_layer_info: *mut VkLayerInstanceLink,
}

/// Linked list node containing the next layer's proc addrs (instance level).
#[repr(C)]
pub struct VkLayerInstanceLink {
	pub p_next: *mut VkLayerInstanceLink,
	pub pfn_next_get_instance_proc_addr: PFN_vkGetInstanceProcAddr,
	pub pfn_next_get_physical_device_proc_addr: PFN_vkVoidFunction,
}

/// Passed by the loader through the `pNext` chain of `VkDeviceCreateInfo`
/// during `vkCreateDevice`.
#[repr(C)]
pub struct VkLayerDeviceCreateInfo {
	pub s_type: StructureType,
	pub p_next: *const std::ffi::c_void,
	pub function: VkLayerFunction,
	/// Union — interpret as `*mut VkLayerDeviceLink` when
	/// `function == LinkInfo`.
	pub p_layer_info: *mut VkLayerDeviceLink,
}

/// Linked list node containing the next layer's proc addrs (device level).
#[repr(C)]
pub struct VkLayerDeviceLink {
	pub p_next: *mut VkLayerDeviceLink,
	pub pfn_next_get_instance_proc_addr: PFN_vkGetInstanceProcAddr,
	pub pfn_next_get_device_proc_addr: PFN_vkGetDeviceProcAddr,
}

#[repr(u32)]
#[derive(PartialEq)]
pub enum VkLayerFunction {
	LinkInfo = 0,
	DataCallback = 1,
}

/// The header struct that `vkNegotiateLoaderLayerInterfaceVersion` fills in.
#[repr(C)]
pub struct VkNegotiateLayerInterface {
	pub s_type: u32, // VK_STRUCTURE_TYPE_NEGOTIATE_LAYER_INTERFACE
	pub p_next: *mut std::ffi::c_void,
	pub loader_layer_interface_version: u32,
	pub pfn_get_instance_proc_addr: Option<PFN_vkGetInstanceProcAddr>,
	pub pfn_get_device_proc_addr: Option<PFN_vkGetDeviceProcAddr>,
	pub pfn_get_physical_device_proc_addr: Option<PFN_vkVoidFunction>,
}

// ---------------------------------------------------------------------------
// Per-instance dispatch table
// ---------------------------------------------------------------------------

/// Dispatch functions we save from the next layer/ICD for instance-level
/// operations.
pub struct InstanceDispatch {
	pub get_instance_proc_addr: PFN_vkGetInstanceProcAddr,
	pub destroy_instance: unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks),
	pub create_device: unsafe extern "C" fn(
		VkPhysicalDevice,
		*const VkDeviceCreateInfo,
		*const VkAllocationCallbacks,
		*mut VkDevice,
	) -> VkResult,
	pub create_wayland_surface: Option<
		unsafe extern "C" fn(
			VkInstance,
			*const VkWaylandSurfaceCreateInfoKHR,
			*const VkAllocationCallbacks,
			*mut VkSurface,
		) -> VkResult,
	>,
	pub create_xcb_surface: Option<
		unsafe extern "C" fn(
			VkInstance,
			*const VkXcbSurfaceCreateInfoKHR,
			*const VkAllocationCallbacks,
			*mut VkSurface,
		) -> VkResult,
	>,
	pub destroy_surface: Option<unsafe extern "C" fn(VkInstance, VkSurface, *const VkAllocationCallbacks)>,
	pub get_physical_device_surface_formats:
		Option<unsafe extern "C" fn(VkPhysicalDevice, VkSurface, *mut u32, *mut VkSurfaceFormatKHR) -> VkResult>,
	pub get_physical_device_surface_formats2: Option<
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const VkPhysicalDeviceSurfaceInfo2KHR,
			*mut u32,
			*mut VkSurfaceFormat2KHR,
		) -> VkResult,
	>,
	pub get_physical_device_surface_capabilities:
		Option<unsafe extern "C" fn(VkPhysicalDevice, VkSurface, *mut VkSurfaceCapabilitiesKHR) -> VkResult>,
	pub get_physical_device_surface_capabilities2: Option<
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const VkPhysicalDeviceSurfaceInfo2KHR,
			*mut VkSurfaceCapabilities2KHR,
		) -> VkResult,
	>,
	pub get_physical_device_surface_present_modes:
		Option<unsafe extern "C" fn(VkPhysicalDevice, VkSurface, *mut u32, *mut VkPresentModeKHR) -> VkResult>,
	pub get_physical_device_wayland_presentation_support:
		Option<unsafe extern "C" fn(VkPhysicalDevice, u32, *mut std::ffi::c_void) -> ash::vk::Bool32>,
	pub get_physical_device_xcb_presentation_support:
		Option<unsafe extern "C" fn(VkPhysicalDevice, u32, *mut std::ffi::c_void, u32) -> ash::vk::Bool32>,
	pub get_physical_device_xlib_presentation_support:
		Option<unsafe extern "C" fn(VkPhysicalDevice, u32, *mut std::ffi::c_void, u64) -> ash::vk::Bool32>,
	pub enumerate_device_extension_properties: Option<
		unsafe extern "C" fn(
			VkPhysicalDevice,
			*const std::ffi::c_char,
			*mut u32,
			*mut ash::vk::ExtensionProperties,
		) -> VkResult,
	>,
}

// ---------------------------------------------------------------------------
// Per-device dispatch table
// ---------------------------------------------------------------------------

pub struct DeviceDispatch {
	pub get_device_proc_addr: PFN_vkGetDeviceProcAddr,
	pub destroy_device: unsafe extern "C" fn(VkDevice, *const VkAllocationCallbacks),
	pub create_swapchain: unsafe extern "C" fn(
		VkDevice,
		*const VkSwapchainCreateInfoKHR,
		*const VkAllocationCallbacks,
		*mut VkSwapchain,
	) -> VkResult,
	pub destroy_swapchain: unsafe extern "C" fn(VkDevice, VkSwapchain, *const VkAllocationCallbacks),
	pub queue_present: unsafe extern "C" fn(VkQueue, *const VkPresentInfoKHR) -> VkResult,
	pub acquire_next_image:
		unsafe extern "C" fn(VkDevice, VkSwapchain, u64, ash::vk::Semaphore, ash::vk::Fence, *mut u32) -> VkResult,
	pub set_hdr_metadata: Option<unsafe extern "C" fn(VkDevice, u32, *const VkSwapchain, *const VkHdrMetadataEXT)>,
	pub acquire_next_image2:
		Option<unsafe extern "C" fn(VkDevice, *const ash::vk::AcquireNextImageInfoKHR, *mut u32) -> VkResult>,
	pub get_refresh_cycle_duration:
		Option<unsafe extern "C" fn(VkDevice, VkSwapchain, *mut ash::vk::RefreshCycleDurationGOOGLE) -> VkResult>,
	pub get_past_presentation_timing: Option<
		unsafe extern "C" fn(VkDevice, VkSwapchain, *mut u32, *mut ash::vk::PastPresentationTimingGOOGLE) -> VkResult,
	>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extracts the loader dispatch key from a dispatchable Vulkan object.
///
/// The Vulkan loader stores a pointer to its own dispatch table as the very
/// first member of every dispatchable handle.  We use the *value* of that
/// pointer as a hash key to look up our per-instance/device state.
///
/// # Safety
///
/// `handle` must be a valid non-null dispatchable Vulkan object.
unsafe fn dispatch_key_raw<T>(handle: T) -> usize
where
	T: ash::vk::Handle,
{
	let raw = handle.as_raw();
	// The first pointer-sized word at the raw address is the dispatch table ptr.
	*(raw as *const usize)
}

use crate::state::{DeviceKey, InstanceKey};

/// Extract the instance-level dispatch key from a VkInstance or VkPhysicalDevice.
pub unsafe fn instance_key_of<T: ash::vk::Handle>(handle: T) -> InstanceKey {
	InstanceKey(dispatch_key_raw(handle))
}

/// Extract the device-level dispatch key from a VkDevice or VkQueue.
pub unsafe fn device_key_of<T: ash::vk::Handle>(handle: T) -> DeviceKey {
	DeviceKey(dispatch_key_raw(handle))
}

/// Walk the `pNext` chain of a create-info struct looking for a
/// `VkLayerInstanceCreateInfo` with `function == VkLayerFunction::LinkInfo`.
///
/// Returns a mutable pointer to the matching struct, or null if not found.
/// The pointer is mutable because the caller needs to advance the link chain.
///
/// # Safety
///
/// `p_next` must be the `pNext` chain of a valid `VkInstanceCreateInfo` or
/// `VkDeviceCreateInfo`.
pub unsafe fn find_layer_link<T>(p_next: *const std::ffi::c_void, s_type: StructureType) -> *mut T {
	let mut cursor = p_next;
	while !cursor.is_null() {
		let header = &*(cursor as *const VkBaseInStructure);
		if header.s_type == s_type {
			// Check if this is a VK_LAYER_LINK_INFO entry.
			// The `function` field is at the same offset in both
			// VkLayerInstanceCreateInfo and VkLayerDeviceCreateInfo.
			let function_ptr = (cursor as *const u8).add(std::mem::offset_of!(VkLayerInstanceCreateInfo, function))
				as *const VkLayerFunction;
			if *function_ptr == VkLayerFunction::LinkInfo {
				return cursor as *mut T;
			}
		}
		cursor = header.p_next;
	}
	std::ptr::null_mut()
}

/// Minimal header shared by all Vulkan structs (for chain walking).
#[repr(C)]
pub struct VkBaseInStructure {
	pub s_type: StructureType,
	pub p_next: *const std::ffi::c_void,
}

/// Walk a `pNext` chain looking for a struct with the given `sType`.
///
/// # Safety
///
/// `p_next` must be a valid Vulkan pNext chain (or null).  The returned
/// pointer is only valid as long as the chain it was found in.
pub unsafe fn find_in_chain<T>(p_next: *const std::ffi::c_void, s_type: StructureType) -> Option<*const T> {
	let mut cursor = p_next;
	while !cursor.is_null() {
		let header = &*(cursor as *const VkBaseInStructure);
		if header.s_type == s_type {
			return Some(cursor as *const T);
		}
		cursor = header.p_next;
	}
	None
}

/// `VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO` (loader-private value `47` from `vk_layer.h`).
pub const VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO: StructureType = StructureType::from_raw(47);
/// `VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO` (loader-private value `48` from `vk_layer.h`).
pub const VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO: StructureType = StructureType::from_raw(48);
