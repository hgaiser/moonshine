//! Driver-backed image-count negotiation for XWayland bypass surfaces.

use crate::dispatch::*;
use crate::state::{LayerStatus, with_instance};
use ash::vk;

pub(crate) fn supports_count(caps: &vk::SurfaceCapabilitiesKHR, count: u32) -> bool {
	count >= caps.min_image_count && (caps.max_image_count == 0 || count <= caps.max_image_count)
}

/// Only relax the conservative legacy minimum for driver-supported counts.
/// Ordinary surfaces (including two-image surfaces) retain the driver's range.
pub(crate) fn compatibility_range(
	legacy: &mut vk::SurfaceCapabilitiesKHR,
	fifo: Option<&vk::SurfaceCapabilitiesKHR>,
) -> bool {
	if legacy.min_image_count > 3
		&& (legacy.max_image_count == 0 || legacy.max_image_count >= 3)
		&& fifo.is_some_and(|caps| {
			supports_count(caps, 3) && (caps.max_image_count == 0 || caps.max_image_count >= legacy.min_image_count - 1)
		}) {
		legacy.min_image_count = 3;
		true
	} else {
		false
	}
}

/// A fallback is permitted only for a request newly admitted by the legacy
/// compatibility range (normally three below a legacy minimum of four).
/// Explicit application mode lists are handled by the caller and never rewritten.
pub(crate) fn select_mode(
	count: u32,
	requested: vk::PresentModeKHR,
	legacy: &vk::SurfaceCapabilitiesKHR,
	requested_caps: Option<&vk::SurfaceCapabilitiesKHR>,
	fifo_caps: Option<&vk::SurfaceCapabilitiesKHR>,
) -> Option<vk::PresentModeKHR> {
	if requested_caps.is_some_and(|caps| supports_count(caps, count)) {
		Some(requested)
	} else if requested_caps.is_some()
		&& count >= 3
		&& count < legacy.min_image_count
		&& (legacy.max_image_count == 0 || legacy.max_image_count >= count)
		&& fifo_caps.is_some_and(|caps| supports_count(caps, count))
	{
		Some(vk::PresentModeKHR::FIFO)
	} else {
		None
	}
}

/// Enumerate through the next layer, never through our extension-advertising hook.
pub(crate) unsafe fn device_supports_maintenance(physical_device: VkPhysicalDevice) -> bool {
	unsafe {
		let Some((enabled, enumerate, features)) = with_instance(instance_key_of(physical_device), |d| {
			(
				d.surface_maintenance1 && d.status == LayerStatus::Active,
				d.dispatch.enumerate_device_extension_properties,
				d.dispatch.get_physical_device_features2,
			)
		}) else {
			return false;
		};
		if !enabled {
			return false;
		}
		let (Some(enumerate), Some(features)) = (enumerate, features) else {
			return false;
		};
		let mut count = 0;
		if enumerate(physical_device, std::ptr::null(), &mut count, std::ptr::null_mut()) != VK_SUCCESS {
			return false;
		}
		let mut props = vec![vk::ExtensionProperties::default(); count as usize];
		if enumerate(physical_device, std::ptr::null(), &mut count, props.as_mut_ptr()) != VK_SUCCESS {
			return false;
		}
		if !props
			.iter()
			.take(count as usize)
			.any(|p| std::ffi::CStr::from_ptr(p.extension_name.as_ptr()) == vk::EXT_SWAPCHAIN_MAINTENANCE1_NAME)
		{
			return false;
		}
		let mut maintenance = vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default();
		let mut info = vk::PhysicalDeviceFeatures2 {
			p_next: (&mut maintenance as *mut vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT).cast(),
			..Default::default()
		};
		features(physical_device, &mut info);
		maintenance.swapchain_maintenance1 == vk::TRUE
	}
}

pub(crate) struct ModeCapabilities {
	pub caps: vk::SurfaceCapabilitiesKHR,
	pub compatible: Vec<vk::PresentModeKHR>,
}

/// Choose and declare exactly the modes whose queried ranges accept the count.
/// The layer's existing FIFO limiter may request FIFO at creation if the driver
/// cannot switch dynamically. Compatibility fallback applies only to bypass.
pub(crate) fn swapchain_modes(
	count: u32,
	requested: VkPresentModeKHR,
	legacy: &VkSurfaceCapabilitiesKHR,
	requested_caps: Option<&ModeCapabilities>,
	fifo: Option<&ModeCapabilities>,
	bypass: bool,
	force_fifo: bool,
) -> Option<(VkPresentModeKHR, Vec<VkPresentModeKHR>)> {
	let conservative = VkSurfaceCapabilitiesKHR {
		min_image_count: 0,
		..*legacy
	};
	let mode = select_mode(
		count,
		requested,
		if bypass { legacy } else { &conservative },
		requested_caps.map(|m| &m.caps),
		fifo.map(|m| &m.caps),
	)?;
	let selected = if mode == requested { requested_caps } else { fifo }?;
	let fifo_valid = fifo.is_some_and(|m| supports_count(&m.caps, count));
	let dynamic_fifo = selected.compatible.contains(&VkPresentModeKHR::FIFO) && fifo_valid;
	if force_fifo && mode != VkPresentModeKHR::FIFO && fifo_valid && !dynamic_fifo {
		return Some((VkPresentModeKHR::FIFO, vec![VkPresentModeKHR::FIFO]));
	}
	let mut modes = vec![mode];
	if mode != VkPresentModeKHR::FIFO && dynamic_fifo {
		modes.push(VkPresentModeKHR::FIFO);
	}
	Some((mode, modes))
}

/// Stack-backed input/output chains live until both synchronous ICD calls end.
/// `mode` must be a mode returned by the ICD (FIFO is always supported).
pub(crate) unsafe fn query_mode(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	mode: vk::PresentModeKHR,
) -> Option<ModeCapabilities> {
	unsafe {
		let key = instance_key_of(physical_device);
		let next = with_instance(key, |d| {
			if d.surface_maintenance1 && d.status == LayerStatus::Active {
				d.dispatch.get_physical_device_surface_capabilities2
			} else {
				None
			}
		})
		.flatten()?;
		if mode != VkPresentModeKHR::FIFO {
			let enumerate = with_instance(key, |d| d.dispatch.get_physical_device_surface_present_modes).flatten()?;
			let mut count = 0;
			if enumerate(physical_device, surface, &mut count, std::ptr::null_mut()) != VK_SUCCESS {
				return None;
			}
			let mut modes = vec![VkPresentModeKHR::FIFO; count as usize];
			if enumerate(physical_device, surface, &mut count, modes.as_mut_ptr()) != VK_SUCCESS
				|| !modes.iter().take(count as usize).any(|m| *m == mode)
			{
				return None;
			}
		}
		let mode_info = vk::SurfacePresentModeEXT::default().present_mode(mode);
		let info = vk::PhysicalDeviceSurfaceInfo2KHR {
			surface,
			p_next: (&mode_info as *const vk::SurfacePresentModeEXT).cast(),
			..Default::default()
		};
		let mut compatibility = vk::SurfacePresentModeCompatibilityEXT::default();
		let mut output = vk::SurfaceCapabilities2KHR {
			p_next: (&mut compatibility as *mut vk::SurfacePresentModeCompatibilityEXT).cast(),
			..Default::default()
		};
		let result = next(physical_device, &info, &mut output);
		if result != VK_SUCCESS {
			crate::log_debug!(
				"per-mode capabilities: mode={} query failed result={}",
				mode.as_raw(),
				result.as_raw()
			);
			return None;
		}

		let mut compatible = vec![vk::PresentModeKHR::FIFO; compatibility.present_mode_count as usize];
		compatibility.p_present_modes = compatible.as_mut_ptr();
		// Keep the output chain and its backing array alive through the ICD call.
		let mut output = vk::SurfaceCapabilities2KHR {
			p_next: (&mut compatibility as *mut vk::SurfacePresentModeCompatibilityEXT).cast(),
			..Default::default()
		};
		if next(physical_device, &info, &mut output) != VK_SUCCESS {
			return None;
		}
		compatible.truncate(compatibility.present_mode_count as usize);
		let caps = output.surface_capabilities;
		crate::log_debug!(
			"per-mode capabilities: mode={} minImageCount={} maxImageCount={}",
			mode.as_raw(),
			caps.min_image_count,
			caps.max_image_count
		);
		Some(ModeCapabilities { caps, compatible })
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn caps(min: u32, max: u32) -> vk::SurfaceCapabilitiesKHR {
		vk::SurfaceCapabilitiesKHR {
			min_image_count: min,
			max_image_count: max,
			..Default::default()
		}
	}
	#[test]
	fn conservative_legacy_fifo_three() {
		let mut legacy = caps(4, 0);
		assert!(compatibility_range(&mut legacy, Some(&caps(3, 0))));
		assert_eq!((legacy.min_image_count, legacy.max_image_count), (3, 0));
	}
	#[test]
	fn real_four_image_requirement_preserved() {
		let mut legacy = caps(4, 0);
		assert!(!compatibility_range(&mut legacy, Some(&caps(4, 0))));
		assert_eq!(legacy.min_image_count, 4);
	}
	#[test]
	fn bounded_maximum_excludes_three() {
		assert!(!supports_count(&caps(2, 2), 3));
		assert!(!compatibility_range(&mut caps(4, 0), Some(&caps(2, 2))));
	}
	#[test]
	fn ordinary_two_image_surface_preserved() {
		let mut legacy = caps(2, 0);
		assert!(!compatibility_range(&mut legacy, Some(&caps(3, 0))));
		assert_eq!(legacy.min_image_count, 2);
	}
	#[test]
	fn missing_extensions_or_failed_query_preserves_legacy() {
		let mut legacy = caps(4, 0);
		assert!(!compatibility_range(&mut legacy, None));
		assert_eq!(legacy.min_image_count, 4);
		assert!(select_mode(3, vk::PresentModeKHR::MAILBOX, &legacy, None, None).is_none());
	}
	#[test]
	fn higher_legacy_minimum_cannot_create_a_gap_in_advertised_range() {
		assert!(!compatibility_range(&mut caps(5, 0), Some(&caps(3, 3))));
		let mut legacy = caps(5, 0);
		assert!(compatibility_range(&mut legacy, Some(&caps(3, 4))));
		assert!(
			select_mode(
				4,
				VkPresentModeKHR::MAILBOX,
				&caps(5, 0),
				Some(&caps(5, 0)),
				Some(&caps(3, 4))
			) == Some(VkPresentModeKHR::FIFO)
		);
	}

	#[test]
	fn requested_mode_retained_when_supported() {
		assert!(
			select_mode(
				3,
				vk::PresentModeKHR::IMMEDIATE,
				&caps(4, 0),
				Some(&caps(3, 0)),
				Some(&caps(3, 0))
			) == Some(vk::PresentModeKHR::IMMEDIATE)
		);
	}
	#[test]
	fn fifo_fallback_only_for_newly_admitted_three() {
		assert!(
			select_mode(
				3,
				vk::PresentModeKHR::MAILBOX,
				&caps(4, 0),
				Some(&caps(4, 0)),
				Some(&caps(3, 0))
			) == Some(vk::PresentModeKHR::FIFO)
		);
		assert!(
			select_mode(
				2,
				vk::PresentModeKHR::MAILBOX,
				&caps(4, 0),
				Some(&caps(4, 0)),
				Some(&caps(2, 0))
			)
			.is_none()
		);
		assert!(
			select_mode(
				3,
				vk::PresentModeKHR::MAILBOX,
				&caps(2, 0),
				Some(&caps(4, 0)),
				Some(&caps(3, 0))
			)
			.is_none()
		);
	}
}

#[cfg(test)]
mod mode_tests {
	use super::*;
	fn mode(min: u32, max: u32, compatible_fifo: bool) -> ModeCapabilities {
		ModeCapabilities {
			caps: VkSurfaceCapabilitiesKHR {
				min_image_count: min,
				max_image_count: max,
				..Default::default()
			},
			compatible: if compatible_fifo {
				vec![VkPresentModeKHR::FIFO]
			} else {
				Vec::new()
			},
		}
	}
	#[test]
	fn three_image_creation_declares_same_requested_mode() {
		let legacy = mode(4, 0, false);
		let requested = mode(3, 0, false);
		let (effective, declared) = swapchain_modes(
			3,
			VkPresentModeKHR::IMMEDIATE,
			&legacy.caps,
			Some(&requested),
			Some(&mode(3, 0, true)),
			true,
			false,
		)
		.unwrap();
		assert!(effective == VkPresentModeKHR::IMMEDIATE);
		assert!(declared == [effective]);
	}
	#[test]
	fn compatibility_fallback_declares_fifo_only() {
		let legacy = mode(4, 0, false);
		let (effective, declared) = swapchain_modes(
			3,
			VkPresentModeKHR::MAILBOX,
			&legacy.caps,
			Some(&mode(4, 0, false)),
			Some(&mode(3, 0, true)),
			true,
			false,
		)
		.unwrap();
		assert!(effective == VkPresentModeKHR::FIFO);
		assert!(declared == [VkPresentModeKHR::FIFO]);
		assert!(
			swapchain_modes(
				3,
				VkPresentModeKHR::MAILBOX,
				&legacy.caps,
				Some(&mode(4, 0, false)),
				Some(&mode(3, 0, true)),
				false,
				false
			)
			.is_none()
		);
	}
	#[test]
	fn fifo_switching_requires_compatibility_and_valid_count() {
		let legacy = mode(4, 0, false);
		let requested = mode(3, 0, true);
		let (_, declared) = swapchain_modes(
			3,
			VkPresentModeKHR::IMMEDIATE,
			&legacy.caps,
			Some(&requested),
			Some(&mode(3, 0, true)),
			true,
			false,
		)
		.unwrap();
		assert!(declared == [VkPresentModeKHR::IMMEDIATE, VkPresentModeKHR::FIFO]);
		let (_, declared) = swapchain_modes(
			3,
			VkPresentModeKHR::IMMEDIATE,
			&legacy.caps,
			Some(&requested),
			Some(&mode(4, 0, true)),
			true,
			false,
		)
		.unwrap();
		assert!(declared == [VkPresentModeKHR::IMMEDIATE]);
	}
	#[test]
	fn limiter_uses_fifo_creation_when_dynamic_switching_is_unavailable() {
		let legacy = mode(4, 0, false);
		let (effective, declared) = swapchain_modes(
			3,
			VkPresentModeKHR::IMMEDIATE,
			&legacy.caps,
			Some(&mode(3, 0, false)),
			Some(&mode(3, 0, true)),
			true,
			true,
		)
		.unwrap();
		assert!(effective == VkPresentModeKHR::FIFO);
		assert!(declared == [VkPresentModeKHR::FIFO]);
	}
}

#[cfg(test)]
mod driver_tests {
	use super::*;
	use crate::state::{InstanceData, InstanceKey, insert_instance, remove_instance};
	use ash::vk::Handle;
	use std::sync::atomic::{AtomicUsize, Ordering};

	#[repr(C)]
	struct Driver {
		dispatch_key: usize,
		extension: bool,
		feature: bool,
		calls: AtomicUsize,
	}
	struct Fixture(Box<Driver>);
	impl Fixture {
		fn new(enabled: bool, extension: bool, feature: bool) -> Self {
			let mut driver = Box::new(Driver {
				dispatch_key: 0,
				extension,
				feature,
				calls: AtomicUsize::new(0),
			});
			driver.dispatch_key = (&*driver as *const Driver) as usize;
			insert_instance(
				InstanceKey(driver.dispatch_key),
				InstanceData {
					dispatch: InstanceDispatch {
						get_instance_proc_addr: get_proc,
						get_physical_device_features2: Some(features),
						destroy_instance: destroy,
						create_device: create,
						create_wayland_surface: None,
						create_xcb_surface: None,
						create_xlib_surface: None,
						destroy_surface: None,
						get_physical_device_surface_formats: None,
						get_physical_device_surface_formats2: None,
						get_physical_device_surface_capabilities: None,
						get_physical_device_surface_capabilities2: Some(capabilities),
						get_physical_device_surface_present_modes: None,
						get_physical_device_wayland_presentation_support: None,
						get_physical_device_xcb_presentation_support: None,
						get_physical_device_xlib_presentation_support: None,
						enumerate_device_extension_properties: Some(extensions),
					},
					status: LayerStatus::Active,
					wayland: None,
					frame_limiter_aware: false,
					surface_maintenance1: enabled,
					app_extensions: Vec::new(),
					downstream_extensions: Vec::new(),
				},
			);
			Self(driver)
		}
		fn physical(&self) -> VkPhysicalDevice {
			VkPhysicalDevice::from_raw((&*self.0 as *const Driver) as u64)
		}
	}
	impl Drop for Fixture {
		fn drop(&mut self) {
			remove_instance(InstanceKey(self.0.dispatch_key));
		}
	}
	unsafe extern "C" fn get_proc(_: VkInstance, _: *const std::ffi::c_char) -> PFN_vkVoidFunction {
		None
	}
	unsafe extern "C" fn destroy(_: VkInstance, _: *const VkAllocationCallbacks) {}
	unsafe extern "C" fn create(
		_: VkPhysicalDevice,
		_: *const VkDeviceCreateInfo,
		_: *const VkAllocationCallbacks,
		_: *mut VkDevice,
	) -> VkResult {
		VK_ERROR_INITIALIZATION_FAILED
	}
	unsafe extern "C" fn extensions(
		pd: VkPhysicalDevice,
		_: *const std::ffi::c_char,
		count: *mut u32,
		props: *mut vk::ExtensionProperties,
	) -> VkResult {
		unsafe {
			let d = &*(pd.as_raw() as *const Driver);
			*count = u32::from(d.extension);
			if d.extension && !props.is_null() {
				*props = vk::ExtensionProperties::default();
				let name = vk::EXT_SWAPCHAIN_MAINTENANCE1_NAME.to_bytes_with_nul();
				std::ptr::copy_nonoverlapping(name.as_ptr().cast(), (*props).extension_name.as_mut_ptr(), name.len());
			}
			VK_SUCCESS
		}
	}
	unsafe extern "system" fn features(pd: VkPhysicalDevice, info: *mut vk::PhysicalDeviceFeatures2) {
		unsafe {
			let d = &*(pd.as_raw() as *const Driver);
			d.calls.fetch_add(1, Ordering::Relaxed);
			assert!((*info).s_type == vk::StructureType::PHYSICAL_DEVICE_FEATURES_2);
			let maintenance = (*info).p_next as *mut vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT;
			assert!((*maintenance).s_type == vk::StructureType::PHYSICAL_DEVICE_SWAPCHAIN_MAINTENANCE_1_FEATURES_EXT);
			(*maintenance).swapchain_maintenance1 = u32::from(d.feature);
		}
	}
	unsafe extern "C" fn capabilities(
		pd: VkPhysicalDevice,
		info: *const VkPhysicalDeviceSurfaceInfo2KHR,
		output: *mut VkSurfaceCapabilities2KHR,
	) -> VkResult {
		unsafe {
			let d = &*(pd.as_raw() as *const Driver);
			d.calls.fetch_add(1, Ordering::Relaxed);
			assert!((*info).s_type == vk::StructureType::PHYSICAL_DEVICE_SURFACE_INFO_2_KHR);
			let mode = (*info).p_next as *const vk::SurfacePresentModeEXT;
			assert!((*mode).s_type == vk::StructureType::SURFACE_PRESENT_MODE_EXT);
			assert!((*mode).present_mode == VkPresentModeKHR::FIFO);
			assert!((*output).s_type == vk::StructureType::SURFACE_CAPABILITIES_2_KHR);
			(*output).surface_capabilities.min_image_count = 3;
			let compat = (*output).p_next as *mut vk::SurfacePresentModeCompatibilityEXT;
			assert!((*compat).s_type == vk::StructureType::SURFACE_PRESENT_MODE_COMPATIBILITY_EXT);
			if !(*compat).p_present_modes.is_null() {
				assert!((*compat).present_mode_count >= 1);
				*(*compat).p_present_modes = VkPresentModeKHR::FIFO;
			}
			(*compat).present_mode_count = 1;
			VK_SUCCESS
		}
	}
	#[test]
	fn unavailable_instance_extension_never_calls_icd() {
		let f = Fixture::new(false, true, true);
		unsafe {
			assert!(!device_supports_maintenance(f.physical()));
			assert!(query_mode(f.physical(), VkSurface::null(), VkPresentModeKHR::FIFO).is_none());
		}
		assert_eq!(f.0.calls.load(Ordering::Relaxed), 0);
	}
	#[test]
	fn unavailable_device_extension_never_queries_feature() {
		let f = Fixture::new(true, false, true);
		assert!(!unsafe { device_supports_maintenance(f.physical()) });
		assert_eq!(f.0.calls.load(Ordering::Relaxed), 0);
	}
	#[test]
	fn advertised_extension_without_feature_is_insufficient() {
		let f = Fixture::new(true, true, false);
		assert!(!unsafe { device_supports_maintenance(f.physical()) });
		assert_eq!(f.0.calls.load(Ordering::Relaxed), 1);
	}
	#[test]
	fn private_capabilities2_enablement_does_not_expose_app_entrypoint() {
		let f = Fixture::new(true, true, true);
		let instance = VkInstance::from_raw(f.physical().as_raw());
		assert!(
			unsafe {
				crate::moonshine_vk_get_instance_proc_addr(
					instance,
					c"vkGetPhysicalDeviceSurfaceCapabilities2KHR".as_ptr(),
				)
			}
			.is_none()
		);
		use crate::state::RwLockExt;
		crate::state::INSTANCE_MAP
			.get()
			.unwrap()
			.force_write()
			.get_mut(&InstanceKey(f.0.dispatch_key))
			.unwrap()
			.app_extensions
			.push(c"VK_KHR_get_surface_capabilities2".to_owned());
		assert!(
			unsafe {
				crate::moonshine_vk_get_instance_proc_addr(
					instance,
					c"vkGetPhysicalDeviceSurfaceCapabilities2KHR".as_ptr(),
				)
			}
			.is_some()
		);
	}

	#[test]
	fn mode_query_passes_valid_live_chains_and_uses_driver_results() {
		let f = Fixture::new(true, true, true);
		assert!(unsafe { device_supports_maintenance(f.physical()) });
		let mode = unsafe { query_mode(f.physical(), VkSurface::null(), VkPresentModeKHR::FIFO) }.unwrap();
		assert_eq!(mode.caps.min_image_count, 3);
		assert!(mode.compatible == [VkPresentModeKHR::FIFO]);
		assert_eq!(f.0.calls.load(Ordering::Relaxed), 3);
	}
}
