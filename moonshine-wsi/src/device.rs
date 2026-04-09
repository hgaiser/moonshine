//! `vkCreateDevice` / `vkDestroyDevice` intercepts.

use crate::dispatch::*;
use crate::state::{insert_device, DeviceData};

pub unsafe extern "C" fn create_device(
	physical_device: VkPhysicalDevice,
	p_create_info: *const VkDeviceCreateInfo,
	p_allocator: *const VkAllocationCallbacks,
	p_device: *mut VkDevice,
) -> VkResult {
	// Locate the loader link info in the pNext chain.
	let chain_info = find_layer_link::<VkLayerDeviceCreateInfo>(
		(*p_create_info).p_next,
		VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO,
	);
	if chain_info.is_null() {
		return VK_ERROR_INITIALIZATION_FAILED;
	}

	let link = (*chain_info).p_layer_info;
	let next_get_device_proc_addr = (*link).pfn_next_get_device_proc_addr;
	let next_get_instance_proc_addr = (*link).pfn_next_get_instance_proc_addr;
	// Advance the chain for the next layer.
	(*chain_info).p_layer_info = (*link).p_next;

	// Inject VK_EXT_swapchain_maintenance1 if not already enabled.
	let create_info = &*p_create_info;
	let mut exts: Vec<*const i8> = std::slice::from_raw_parts(
		create_info.pp_enabled_extension_names,
		create_info.enabled_extension_count as usize,
	)
	.to_vec();

	let maintenance1_ext = ash::vk::EXT_SWAPCHAIN_MAINTENANCE1_NAME;
	let has_maintenance1_ext = has_extension(&exts, maintenance1_ext);
	if !has_maintenance1_ext {
		exts.push(maintenance1_ext.as_ptr());
	}

	// Force-enable the swapchainMaintenance1 feature via pNext chain, but only
	// if the application hasn't already included the struct; injecting a
	// duplicate would create a cycle and trigger undefined behaviour in drivers
	// that walk the chain.
	let app_has_maintenance1_features = find_in_chain::<ash::vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT>(
		create_info.p_next,
		ash::vk::StructureType::PHYSICAL_DEVICE_SWAPCHAIN_MAINTENANCE_1_FEATURES_EXT,
	)
	.is_some();

	let mut maintenance1_features =
		ash::vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default().swapchain_maintenance1(true);
	maintenance1_features.p_next = create_info.p_next as *mut std::ffi::c_void;

	let mut modified_create_info = *create_info;
	modified_create_info.enabled_extension_count = exts.len() as u32;
	modified_create_info.pp_enabled_extension_names = exts.as_ptr();
	if !app_has_maintenance1_features {
		modified_create_info.p_next = &maintenance1_features as *const _ as *const std::ffi::c_void;
	}

	// Call through to the next layer/ICD.
	let next_create_device: unsafe extern "C" fn(
		VkPhysicalDevice,
		*const VkDeviceCreateInfo,
		*const VkAllocationCallbacks,
		*mut VkDevice,
	) -> VkResult = {
		let name = c"vkCreateDevice";
		let pfn = next_get_instance_proc_addr(VkInstance::null(), name.as_ptr());
		std::mem::transmute(pfn.expect("next layer must provide vkCreateDevice"))
	};

	let result = next_create_device(physical_device, &modified_create_info, p_allocator, p_device);

	// If device creation fails (e.g. extension or feature bit unsupported), retry
	// with the app's original unmodified create info, removing both our extension
	// injection and the forced feature struct.
	let (result, has_maintenance1) = if result != VK_SUCCESS {
		crate::log_warn!("VK_EXT_swapchain_maintenance1 not supported, retrying without");
		let result = next_create_device(physical_device, p_create_info, p_allocator, p_device);
		(result, false)
	} else {
		(result, true)
	};

	if result != VK_SUCCESS {
		return result;
	}

	let device = *p_device;
	let key = device_key_of(device);

	// Find the owning instance key.  We look it up from the physical device.
	let instance_key = instance_key_of(physical_device);

	let dispatch = build_device_dispatch(device, next_get_device_proc_addr);

	crate::log_debug!("vkCreateDevice (maintenance1={})", has_maintenance1);

	insert_device(
		key,
		DeviceData {
			dispatch,
			instance_key,
			has_maintenance1,
		},
	);

	VK_SUCCESS
}

pub unsafe extern "C" fn destroy_device(device: VkDevice, p_allocator: *const VkAllocationCallbacks) {
	let key = device_key_of(device);
	crate::log_debug!("vkDestroyDevice");

	// Remove and extract the function pointer in a single lock acquisition.
	let data = crate::state::remove_device(key);

	if let Some(d) = data {
		(d.dispatch.destroy_device)(device, p_allocator);
	}
}

unsafe fn build_device_dispatch(
	device: VkDevice,
	next_get_device_proc_addr: PFN_vkGetDeviceProcAddr,
) -> DeviceDispatch {
	macro_rules! load {
		($name:literal) => {{
			let pfn = next_get_device_proc_addr(device, concat!($name, "\0").as_ptr() as *const i8);
			std::mem::transmute(pfn.expect(concat!("failed to load ", $name)))
		}};
		(opt: $name:literal) => {{
			let pfn = next_get_device_proc_addr(device, concat!($name, "\0").as_ptr() as *const i8);
			pfn.map(|p| std::mem::transmute(p))
		}};
	}

	DeviceDispatch {
		get_device_proc_addr: next_get_device_proc_addr,
		destroy_device: load!("vkDestroyDevice"),
		create_swapchain: load!("vkCreateSwapchainKHR"),
		destroy_swapchain: load!("vkDestroySwapchainKHR"),
		queue_present: load!("vkQueuePresentKHR"),
		acquire_next_image: load!("vkAcquireNextImageKHR"),
		set_hdr_metadata: load!(opt: "vkSetHdrMetadataEXT"),
		acquire_next_image2: load!(opt: "vkAcquireNextImage2KHR"),
		get_refresh_cycle_duration: load!(opt: "vkGetRefreshCycleDurationGOOGLE"),
		get_past_presentation_timing: load!(opt: "vkGetPastPresentationTimingGOOGLE"),
	}
}
