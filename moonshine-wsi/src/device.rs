//! `vkCreateDevice` / `vkDestroyDevice` intercepts.

use crate::dispatch::*;
use crate::state::{DeviceData, insert_device};

/// Preserve an application's existing feature choice; inject only when absent.
fn maintenance_plan(supported: bool, app_feature: Option<bool>) -> (bool, bool) {
	(
		supported && app_feature.is_none(),
		supported && app_feature.unwrap_or(true),
	)
}

pub unsafe extern "C" fn create_device(
	physical_device: VkPhysicalDevice,
	p_create_info: *const VkDeviceCreateInfo,
	p_allocator: *const VkAllocationCallbacks,
	p_device: *mut VkDevice,
) -> VkResult {
	unsafe {
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
		// Advance the chain for the next layer.
		let next_layer_info = (*link).p_next;
		(*chain_info).p_layer_info = next_layer_info;

		let create_info = &*p_create_info;
		let mut exts = extension_names(
			create_info.pp_enabled_extension_names,
			create_info.enabled_extension_count,
		);
		let maintenance1_ext = ash::vk::EXT_SWAPCHAIN_MAINTENANCE1_NAME;
		let app_features = find_in_chain::<ash::vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT>(
			create_info.p_next,
			ash::vk::StructureType::PHYSICAL_DEVICE_SWAPCHAIN_MAINTENANCE_1_FEATURES_EXT,
		);
		let supported = has_extension(&exts, c"VK_KHR_swapchain")
			&& crate::image_count::device_supports_maintenance(physical_device);
		let (inject_feature, has_maintenance1) = maintenance_plan(
			supported,
			app_features.map(|p| (*p).swapchain_maintenance1 == ash::vk::TRUE),
		);
		if has_maintenance1 && !has_extension(&exts, maintenance1_ext) {
			exts.push(maintenance1_ext.as_ptr());
		}
		let mut maintenance1_features =
			ash::vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default().swapchain_maintenance1(true);
		maintenance1_features.p_next = create_info.p_next as *mut std::ffi::c_void;
		let mut modified_create_info = *create_info;
		modified_create_info.enabled_extension_count = exts.len() as u32;
		modified_create_info.pp_enabled_extension_names = exts.as_ptr();
		if inject_feature {
			modified_create_info.p_next = &maintenance1_features as *const _ as *const std::ffi::c_void;
		}

		// Call through to the next layer/ICD.
		let Some(next_create_device) =
			crate::state::with_instance(instance_key_of(physical_device), |d| d.dispatch.create_device)
		else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};

		let result = next_create_device(physical_device, &modified_create_info, p_allocator, p_device);

		// Do not retry arbitrary device errors or mask application feature errors.
		// Only extension/feature rejection of our own injection may be retried.
		let injected = inject_feature
			|| (has_maintenance1
				&& !has_extension(
					&extension_names(
						create_info.pp_enabled_extension_names,
						create_info.enabled_extension_count,
					),
					maintenance1_ext,
				));
		let (result, has_maintenance1) =
			if injected && (result == VK_ERROR_EXTENSION_NOT_PRESENT || result == VK_ERROR_FEATURE_NOT_PRESENT) {
				crate::log_warn!("swapchain maintenance injection rejected; retrying original device create-info");
				// Downstream layers advance this same loader link on each call.
				(*chain_info).p_layer_info = next_layer_info;
				(
					next_create_device(physical_device, p_create_info, p_allocator, p_device),
					false,
				)
			} else {
				(result, has_maintenance1)
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
				physical_device,
			},
		);

		VK_SUCCESS
	}
}

pub unsafe extern "C" fn destroy_device(device: VkDevice, p_allocator: *const VkAllocationCallbacks) {
	unsafe {
		let key = device_key_of(device);
		crate::log_debug!("vkDestroyDevice");

		// Remove and extract the function pointer in a single lock acquisition.
		let data = crate::state::remove_device(key);

		if let Some(d) = data {
			(d.dispatch.destroy_device)(device, p_allocator);
		}
	}
}

unsafe fn build_device_dispatch(
	device: VkDevice,
	next_get_device_proc_addr: PFN_vkGetDeviceProcAddr,
) -> DeviceDispatch {
	unsafe {
		macro_rules! load {
			($name:literal) => {{
				let pfn = next_get_device_proc_addr(device, concat!($name, "\0").as_ptr() as *const std::ffi::c_char);
				std::mem::transmute(pfn.expect(concat!("failed to load ", $name)))
			}};
			(opt: $name:literal) => {{
				let pfn = next_get_device_proc_addr(device, concat!($name, "\0").as_ptr() as *const std::ffi::c_char);
				pfn.map(|p| std::mem::transmute(p))
			}};
		}

		let dispatch = DeviceDispatch {
			get_device_proc_addr: next_get_device_proc_addr,
			destroy_device: load!("vkDestroyDevice"),
			create_swapchain: load!(opt: "vkCreateSwapchainKHR"),
			get_swapchain_images: load!(opt: "vkGetSwapchainImagesKHR"),
			destroy_swapchain: load!(opt: "vkDestroySwapchainKHR"),
			queue_present: load!(opt: "vkQueuePresentKHR"),
			acquire_next_image: load!(opt: "vkAcquireNextImageKHR"),
			set_hdr_metadata: load!(opt: "vkSetHdrMetadataEXT"),
			acquire_next_image2: load!(opt: "vkAcquireNextImage2KHR"),
			get_refresh_cycle_duration: load!(opt: "vkGetRefreshCycleDurationGOOGLE"),
			get_past_presentation_timing: load!(opt: "vkGetPastPresentationTimingGOOGLE"),
		};

		if dispatch.create_swapchain.is_none() {
			crate::log_warn!(
				"vkCreateSwapchainKHR not available — layer will run in degraded mode \
			(app likely not using VK_KHR_swapchain or layer chain is broken)"
			);
		}

		dispatch
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn maintenance_injection_respects_support_and_explicit_features() {
		assert_eq!(maintenance_plan(false, None), (false, false));
		assert_eq!(maintenance_plan(false, Some(true)), (false, false));
		assert_eq!(maintenance_plan(true, None), (true, true));
		assert_eq!(maintenance_plan(true, Some(false)), (false, false));
		assert_eq!(maintenance_plan(true, Some(true)), (false, true));
	}
}
