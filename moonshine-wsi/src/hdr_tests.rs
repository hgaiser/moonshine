use std::cell::RefCell;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use ash::vk::{self, Handle};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry::WlRegistry};

use crate::dispatch::*;
use crate::state::*;

#[derive(Default)]
struct DriverCalls {
	surfaces: Vec<VkSurface>,
	colors: Vec<i32>,
	fail_create: bool,
}
thread_local! { static CALLS: RefCell<DriverCalls> = RefCell::default(); }

unsafe extern "C" fn get_instance_proc(_: VkInstance, _: *const std::ffi::c_char) -> PFN_vkVoidFunction {
	None
}
unsafe extern "C" fn get_device_proc(_: VkDevice, _: *const std::ffi::c_char) -> PFN_vkVoidFunction {
	None
}
unsafe extern "C" fn destroy_instance(_: VkInstance, _: *const VkAllocationCallbacks) {}
unsafe extern "C" fn destroy_device(_: VkDevice, _: *const VkAllocationCallbacks) {}
unsafe extern "C" fn create_device(
	_: VkPhysicalDevice,
	_: *const VkDeviceCreateInfo,
	_: *const VkAllocationCallbacks,
	_: *mut VkDevice,
) -> VkResult {
	VK_ERROR_INITIALIZATION_FAILED
}
unsafe extern "C" fn formats(
	_: VkPhysicalDevice,
	surface: VkSurface,
	count: *mut u32,
	buf: *mut VkSurfaceFormatKHR,
) -> VkResult {
	CALLS.with_borrow_mut(|c| c.surfaces.push(surface));
	unsafe {
		if !buf.is_null() {
			if *count == 0 {
				return VK_INCOMPLETE;
			}
			*buf = VkSurfaceFormatKHR {
				format: vk::Format::B8G8R8A8_UNORM,
				color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
			};
		}
		*count = 1;
	}
	VK_SUCCESS
}
unsafe extern "C" fn formats2(
	device: VkPhysicalDevice,
	info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	count: *mut u32,
	buf: *mut VkSurfaceFormat2KHR,
) -> VkResult {
	unsafe {
		formats(
			device,
			(*info).surface,
			count,
			if buf.is_null() {
				std::ptr::null_mut()
			} else {
				&mut (*buf).surface_format
			},
		)
	}
}
unsafe extern "C" fn create_swapchain(
	_: VkDevice,
	info: *const VkSwapchainCreateInfoKHR,
	_: *const VkAllocationCallbacks,
	result: *mut VkSwapchain,
) -> VkResult {
	unsafe {
		CALLS.with_borrow_mut(|c| {
			c.surfaces.push((*info).surface);
			c.colors.push((*info).image_color_space.as_raw());
			if c.fail_create {
				return vk::Result::ERROR_FORMAT_NOT_SUPPORTED;
			}
			*result = VkSwapchain::from_raw((*info).surface.as_raw());
			VK_SUCCESS
		})
	}
}
unsafe extern "C" fn destroy_swapchain(_: VkDevice, _: VkSwapchain, _: *const VkAllocationCallbacks) {}
impl Dispatch<WlRegistry, ()> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlRegistry,
		_: wayland_client::protocol::wl_registry::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

struct Fixture {
	dispatch: Box<usize>,
	connection: Arc<Mutex<WaylandConnection>>,
	peer: UnixStream,
	surface: VkSurface,
	fallback: VkSurface,
}
impl Fixture {
	fn new() -> Self {
		let mut dispatch = Box::new(0usize);
		*dispatch = (&*dispatch as *const usize) as usize;
		let key = *dispatch;
		let (socket, peer) = UnixStream::pair().unwrap();
		peer.set_nonblocking(true).unwrap();
		let connection = Connection::from_socket(socket).unwrap();
		let event_queue = connection.new_event_queue::<WaylandState>();
		let qh = event_queue.handle();
		// These proxies only send requests into the socket pair. No GPU or
		// compositor is needed to observe which feedback the layer emits.
		let registry = connection.display().get_registry(&qh, ());
		let compositor = registry.bind(1, 4, &qh, ());
		let swapchain_factory = registry.bind(2, 1, &qh, ());
		let connection = Arc::new(Mutex::new(WaylandConnection {
			connection,
			compositor,
			swapchain_factory,
			caps: CompositorCaps {
				_compositor_version: 4,
				_factory_version: 1,
				hdr_supported: true,
			},
			event_queue,
			qh,
			dead: false,
		}));
		insert_instance(
			InstanceKey(key),
			InstanceData {
				dispatch: InstanceDispatch {
					get_instance_proc_addr: get_instance_proc,
					destroy_instance,
					create_device,
					create_wayland_surface: None,
					create_xcb_surface: None,
					destroy_surface: None,
					get_physical_device_surface_formats: Some(formats),
					get_physical_device_surface_formats2: Some(formats2),
					get_physical_device_surface_capabilities: None,
					get_physical_device_surface_capabilities2: None,
					get_physical_device_surface_present_modes: None,
					get_physical_device_wayland_presentation_support: None,
					get_physical_device_xcb_presentation_support: None,
					get_physical_device_xlib_presentation_support: None,
					enumerate_device_extension_properties: None,
				},
				status: LayerStatus::Active,
				wayland: Some(connection.clone()),
				frame_limiter_aware: false,
			},
		);
		insert_device(
			DeviceKey(key),
			DeviceData {
				dispatch: DeviceDispatch {
					get_device_proc_addr: get_device_proc,
					destroy_device,
					create_swapchain: Some(create_swapchain),
					destroy_swapchain: Some(destroy_swapchain),
					queue_present: None,
					acquire_next_image: None,
					set_hdr_metadata: None,
					acquire_next_image2: None,
					get_refresh_cycle_duration: None,
					get_past_presentation_timing: None,
				},
				instance_key: InstanceKey(key),
				has_maintenance1: false,
			},
		);
		CALLS.with_borrow_mut(|c| *c = DriverCalls::default());
		Self {
			dispatch,
			connection,
			peer,
			surface: VkSurface::from_raw(key as u64),
			fallback: VkSurface::from_raw(key as u64 + 1),
		}
	}
	fn track(&self, native: bool, fallback: VkSurface) {
		let wl = self.connection.force_lock();
		let wl_surface = wl.compositor.create_surface(&wl.qh, ());
		insert_surface(
			SurfaceKey::from_raw(self.surface.as_raw()),
			SurfaceData {
				wl_surface: wl_surface.clone(),
				xcb_window: None,
				xcb_connection: std::ptr::null_mut(),
				fallback_surface: fallback,
				native: native.then(|| NativeWaylandSurface {
					connection: self.connection.clone(),
					wl_surface,
				}),
			},
		);
	}
	fn physical_device(&self) -> VkPhysicalDevice {
		VkPhysicalDevice::from_raw((&*self.dispatch as *const usize) as u64)
	}
	fn device(&self) -> VkDevice {
		VkDevice::from_raw(self.physical_device().as_raw())
	}
	fn query(&self, injected: bool, expected_surface: VkSurface) {
		unsafe {
			let mut count = 0;
			assert_eq!(
				crate::surface::get_physical_device_surface_formats(
					self.physical_device(),
					self.surface,
					&mut count,
					std::ptr::null_mut()
				),
				VK_SUCCESS
			);
			assert_eq!(count, if injected { 4 } else { 1 });
			let mut formats = vec![VkSurfaceFormatKHR::default(); count as usize];
			assert_eq!(
				crate::surface::get_physical_device_surface_formats(
					self.physical_device(),
					self.surface,
					&mut count,
					formats.as_mut_ptr()
				),
				VK_SUCCESS
			);
			let info = VkPhysicalDeviceSurfaceInfo2KHR::default().surface(self.surface);
			let mut count2 = 0;
			assert_eq!(
				crate::surface::get_physical_device_surface_formats2(
					self.physical_device(),
					&info,
					&mut count2,
					std::ptr::null_mut()
				),
				VK_SUCCESS
			);
			assert_eq!(count2, count);
			let mut formats2 = vec![VkSurfaceFormat2KHR::default(); count2 as usize];
			assert_eq!(
				crate::surface::get_physical_device_surface_formats2(
					self.physical_device(),
					&info,
					&mut count2,
					formats2.as_mut_ptr()
				),
				VK_SUCCESS
			);
			assert_eq!(formats[0].color_space.as_raw(), 0);
			for (a, b) in formats.iter().zip(formats2) {
				assert_eq!(a.format.as_raw(), b.surface_format.format.as_raw());
				assert_eq!(a.color_space.as_raw(), b.surface_format.color_space.as_raw());
			}
			if injected {
				assert!(
					formats
						.iter()
						.any(|f| f.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT)
				);
			}
		}
		CALLS.with_borrow(|c| assert!(c.surfaces.iter().all(|s| *s == expected_surface)));
		CALLS.with_borrow_mut(|c| c.surfaces.clear());
	}
	fn create(&self, color: vk::ColorSpaceKHR) -> (VkResult, VkSwapchain) {
		let info = VkSwapchainCreateInfoKHR::default()
			.surface(self.surface)
			.image_color_space(color);
		let mut swapchain = VkSwapchain::null();
		let result =
			unsafe { crate::swapchain::create_swapchain(self.device(), &info, std::ptr::null(), &mut swapchain) };
		assert_eq!(info.image_color_space.as_raw(), color.as_raw());
		(result, swapchain)
	}
}
impl Drop for Fixture {
	fn drop(&mut self) {
		remove_swapchain(SwapchainKey::from_raw(self.surface.as_raw()));
		remove_swapchain(SwapchainKey::from_raw(self.fallback.as_raw()));
		remove_surface(SurfaceKey::from_raw(self.surface.as_raw()));
		remove_device(DeviceKey(*self.dispatch));
		remove_instance(InstanceKey(*self.dispatch));
	}
}

#[test]
fn untracked_surface_keeps_driver_color_space() {
	let f = Fixture::new();
	f.query(false, f.surface);
	assert_eq!(f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0, VK_SUCCESS);
	CALLS.with_borrow(|c| assert_eq!(c.colors, [1000104008]));
}
#[test]
fn fallback_keeps_driver_color_space() {
	let f = Fixture::new();
	f.track(false, f.fallback);
	f.query(false, f.fallback);
	assert_eq!(f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0, VK_SUCCESS);
	CALLS.with_borrow(|c| {
		assert_eq!(c.surfaces, [f.fallback]);
		assert_eq!(c.colors, [1000104008]);
	});
}
#[test]
fn denied_bypass_without_fallback_does_not_offer_hdr() {
	let f = Fixture::new();
	f.track(false, VkSurface::null());
	f.query(false, f.surface);
	assert_eq!(f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0, VK_SUCCESS);
	CALLS.with_borrow(|c| assert_eq!(c.colors, [1000104008]));
}
#[test]
fn bypass_remaps_driver_and_preserves_feedback_color_space() {
	for color in [
		vk::ColorSpaceKHR::HDR10_ST2084_EXT,
		vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
	] {
		let mut f = Fixture::new();
		f.track(true, VkSurface::null());
		f.query(true, f.surface);
		let (result, swapchain) = f.create(color);
		assert_eq!(result, VK_SUCCESS);
		let id = with_swapchain(SwapchainKey::from_raw(swapchain.as_raw()), |s| {
			use wayland_client::Proxy;
			s.ms_swapchain.as_ref().unwrap().id().protocol_id()
		})
		.unwrap();
		CALLS.with_borrow(|c| assert_eq!(c.colors, [0]));
		let mut bytes = Vec::new();
		let error = f.peer.read_to_end(&mut bytes).unwrap_err();
		assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
		let mut offset = 0;
		let mut feedback = None;
		while offset + 8 <= bytes.len() {
			let object = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
			let header = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
			let size = (header >> 16) as usize;
			assert!(size >= 8 && offset + size <= bytes.len());
			// moonshine-swapchain.xml: request 2 is swapchain_feedback;
			// its third uint is the application's Vulkan color space.
			if object == id && header & 0xffff == 2 {
				feedback = Some(u32::from_ne_bytes(bytes[offset + 16..offset + 20].try_into().unwrap()));
			}
			offset += size;
		}
		assert_eq!(feedback, Some(color.as_raw() as u32));
	}
}
#[test]
fn recreation_rechecks_route_after_enumeration() {
	let f = Fixture::new();
	f.track(true, VkSurface::null());
	f.query(true, f.surface);
	f.track(false, f.fallback);
	assert_eq!(f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0, VK_SUCCESS);
	CALLS.with_borrow(|c| {
		assert_eq!(c.surfaces, [f.fallback]);
		assert_eq!(c.colors, [1000104008]);
	});
}
#[test]
fn sdr_and_driver_failures_are_preserved() {
	let f = Fixture::new();
	f.track(true, VkSurface::null());
	f.connection.force_lock().caps.hdr_supported = false;
	f.query(false, f.surface);
	assert_eq!(f.create(vk::ColorSpaceKHR::SRGB_NONLINEAR).0, VK_SUCCESS);
	CALLS.with_borrow(|c| assert_eq!(c.colors, [0]));
	CALLS.with_borrow_mut(|c| c.fail_create = true);
	assert_eq!(
		f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0,
		vk::Result::ERROR_FORMAT_NOT_SUPPORTED
	);
}
#[test]
fn dead_connection_does_not_offer_or_remap_hdr() {
	let f = Fixture::new();
	f.track(true, VkSurface::null());
	f.connection.force_lock().dead = true;
	f.query(false, f.surface);
	assert_eq!(f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0, VK_SUCCESS);
	CALLS.with_borrow(|c| assert_eq!(c.colors, [1000104008]));
}

#[test]
fn native_surface_uses_its_own_connection_capabilities() {
	let f = Fixture::new();
	f.track(true, VkSurface::null());
	let other = Fixture::new();
	other.connection.force_lock().caps.hdr_supported = false;
	let mut instance = remove_instance(InstanceKey(*f.dispatch)).unwrap();
	instance.wayland = Some(other.connection.clone());
	insert_instance(InstanceKey(*f.dispatch), instance);
	f.query(true, f.surface);
	assert_eq!(f.create(vk::ColorSpaceKHR::HDR10_ST2084_EXT).0, VK_SUCCESS);
	CALLS.with_borrow(|c| assert_eq!(c.colors, [0]));
}
