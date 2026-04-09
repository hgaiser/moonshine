//! `vkCreateInstance` / `vkDestroyInstance` intercepts.
//!
//! On `CreateInstance` we:
//!  1. Inject `VK_KHR_wayland_surface` and `VK_KHR_xcb_surface` if absent.
//!  2. Forward to the next layer/ICD.
//!  3. Connect to the Moonshine Wayland compositor.
//!  4. Scan the compositor global registry for `wl_compositor` and
//!     `moonshine_swapchain_factory_v2`.
//!  5. Store everything in `INSTANCE_MAP`.

use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

use wayland_client::{globals::registry_queue_init, protocol::wl_compositor::WlCompositor, Connection, Proxy};

use crate::dispatch::*;
use crate::proto::moonshine_swapchain_factory_v2::MoonshineSwapchainFactoryV2;
use crate::state::{
	insert_instance, remove_instance, CompositorCaps, InstanceData, LayerStatus, WaylandConnection, WaylandState,
};

/// Returns the value of `MOONSHINE_WAYLAND_DISPLAY`, or `None` if unset.
fn moonshine_wayland_display() -> Option<CString> {
	let val = std::env::var("MOONSHINE_WAYLAND_DISPLAY").ok()?;
	if val.is_empty() {
		return None;
	}
	CString::new(val).ok()
}

pub unsafe extern "C" fn create_instance(
	p_create_info: *const VkInstanceCreateInfo,
	p_allocator: *const VkAllocationCallbacks,
	p_instance: *mut VkInstance,
) -> VkResult {
	// Advance the layer chain (shared by both the active and pass-through paths).
	let chain_info = find_layer_link::<VkLayerInstanceCreateInfo>(
		(*p_create_info).p_next,
		VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO,
	);
	if chain_info.is_null() {
		return VK_ERROR_INITIALIZATION_FAILED;
	}

	let link = (*chain_info).p_layer_info;
	let next_get_proc_addr = (*link).pfn_next_get_instance_proc_addr;
	(*chain_info).p_layer_info = (*link).p_next;

	// Check whether the layer should operate in active or degraded mode.
	// Even when degraded we must insert InstanceData so that intercepted entry
	// points (returned by vkGetInstanceProcAddr) can forward through a valid
	// dispatch table rather than crashing or returning wrong errors.
	let moonshine_display = moonshine_wayland_display();
	let is_active = moonshine_display.is_some();
	if !is_active {
		crate::log_debug!("MOONSHINE_WAYLAND_DISPLAY not set — registering layer instance as degraded");
	}

	// Only inject the extra WSI extensions when active.  In the degraded path
	// the application's original create-info is passed through unchanged.
	let create_info = &*p_create_info;
	let mut exts: Vec<*const i8> = std::slice::from_raw_parts(
		create_info.pp_enabled_extension_names,
		create_info.enabled_extension_count as usize,
	)
	.to_vec();
	let mut modified_create_info = *create_info;
	if is_active {
		let wayland_ext = c"VK_KHR_wayland_surface";
		let xcb_ext = c"VK_KHR_xcb_surface";
		let surface_ext = c"VK_KHR_surface";

		if !has_extension(&exts, wayland_ext) {
			exts.push(wayland_ext.as_ptr());
		}
		if !has_extension(&exts, xcb_ext) {
			exts.push(xcb_ext.as_ptr());
		}
		if !has_extension(&exts, surface_ext) {
			exts.push(surface_ext.as_ptr());
		}

		modified_create_info.enabled_extension_count = exts.len() as u32;
		modified_create_info.pp_enabled_extension_names = exts.as_ptr();
	}

	// Call the next layer/ICD's vkCreateInstance.
	let next_create_instance = load_next_create_instance(next_get_proc_addr);
	let mut result = next_create_instance(&modified_create_info, p_allocator, p_instance);

	// If the ICD rejected one of our injected extensions, retry with the
	// original (unmodified) create-info and fall back to degraded mode so
	// the layer is still present in the call chain with a valid dispatch table.
	let fell_back = if result == VK_ERROR_EXTENSION_NOT_PRESENT && is_active {
		crate::log_warn!(
			"vkCreateInstance failed (VK_ERROR_EXTENSION_NOT_PRESENT); retrying without \
			 WSI extension injection — layer will run in degraded mode"
		);
		result = next_create_instance(p_create_info, p_allocator, p_instance);
		true
	} else {
		false
	};

	if result != VK_SUCCESS {
		return result;
	}

	let instance = *p_instance;
	let key = instance_key_of(instance);

	// Build our dispatch table from the next layer (needed even when degraded
	// so we can forward calls without a dispatch table lookup failure).
	let dispatch = build_instance_dispatch(instance, next_get_proc_addr);

	let (status, wayland, frame_limiter_aware) = if fell_back {
		// Extension injection failed; the instance was created without our WSI
		// extensions so we operate purely as a passthrough.
		crate::log_debug!("Layer initialized (status=degraded; WSI extension injection failed)");
		(LayerStatus::Degraded, None, false)
	} else if let Some(ref moonshine_display_ref) = moonshine_display {
		// Connect to the Moonshine Wayland compositor.
		let wayland = connect_to_compositor(moonshine_display_ref);

		// Detect frame-limiter-aware engines.
		let frame_limiter_aware = detect_frame_limiter_aware(create_info.p_application_info);

		let status = if wayland.is_some() {
			LayerStatus::Active
		} else {
			LayerStatus::Degraded
		};

		crate::log_info!(
			"Layer initialized (display={}, status={}, frame_limiter_aware={})",
			moonshine_display_ref.to_string_lossy(),
			if status == LayerStatus::Active {
				"active"
			} else {
				"degraded"
			},
			frame_limiter_aware,
		);

		(status, wayland, frame_limiter_aware)
	} else {
		crate::log_debug!("Layer initialized (status=degraded; MOONSHINE_WAYLAND_DISPLAY unset)");
		(LayerStatus::Degraded, None, false)
	};

	insert_instance(
		key,
		InstanceData {
			dispatch,
			status,
			wayland,
			frame_limiter_aware,
		},
	);

	VK_SUCCESS
}

pub unsafe extern "C" fn destroy_instance(instance: VkInstance, p_allocator: *const VkAllocationCallbacks) {
	let key = instance_key_of(instance);
	crate::log_debug!("vkDestroyInstance");

	// Remove and extract the function pointer in a single lock acquisition.
	let data = remove_instance(key);

	if let Some(d) = data {
		(d.dispatch.destroy_instance)(instance, p_allocator);
	}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn load_next_create_instance(
	next_get_proc_addr: PFN_vkGetInstanceProcAddr,
) -> unsafe extern "C" fn(*const VkInstanceCreateInfo, *const VkAllocationCallbacks, *mut VkInstance) -> VkResult {
	let name = c"vkCreateInstance";
	let pfn = next_get_proc_addr(VkInstance::null(), name.as_ptr());
	std::mem::transmute(pfn.expect("next layer must provide vkCreateInstance"))
}

/// Detect whether the application is frame-limiter-aware.
///
/// First checks `MOONSHINE_WSI_FRAME_LIMITER_AWARE` env var, then falls back
/// to engine name/version auto-detection (DXVK ≥ 2.3, vkd3d ≥ 2.12).
unsafe fn detect_frame_limiter_aware(p_application_info: *const ash::vk::ApplicationInfo) -> bool {
	// Env var takes priority.
	if let Ok(val) = std::env::var("MOONSHINE_WSI_FRAME_LIMITER_AWARE") {
		if !val.is_empty() {
			return val.parse::<i32>().unwrap_or(0) != 0;
		}
	}

	if p_application_info.is_null() {
		return false;
	}

	let app_info = &*p_application_info;
	if app_info.p_engine_name.is_null() {
		return false;
	}

	let engine = CStr::from_ptr(app_info.p_engine_name);
	let version = app_info.engine_version;

	// Minimum versions that support VK_GOOGLE_display_timing.
	const VKD3D_MIN: u32 = ash::vk::make_api_version(0, 2, 12, 0);
	const DXVK_MIN: u32 = ash::vk::make_api_version(0, 2, 3, 0);

	(engine.to_bytes() == b"vkd3d" && version >= VKD3D_MIN) || (engine.to_bytes() == b"DXVK" && version >= DXVK_MIN)
}

unsafe fn build_instance_dispatch(
	instance: VkInstance,
	next_get_proc_addr: PFN_vkGetInstanceProcAddr,
) -> InstanceDispatch {
	macro_rules! load {
		($name:literal) => {{
			let pfn = next_get_proc_addr(instance, concat!($name, "\0").as_ptr() as *const i8);
			std::mem::transmute(pfn.expect(concat!("failed to load ", $name)))
		}};
		(opt: $name:literal) => {{
			let pfn = next_get_proc_addr(instance, concat!($name, "\0").as_ptr() as *const i8);
			pfn.map(|f| std::mem::transmute(f))
		}};
	}

	InstanceDispatch {
		get_instance_proc_addr: next_get_proc_addr,
		destroy_instance: load!("vkDestroyInstance"),
		create_device: load!("vkCreateDevice"),
		create_wayland_surface: load!(opt: "vkCreateWaylandSurfaceKHR"),
		create_xcb_surface: load!(opt: "vkCreateXcbSurfaceKHR"),
		destroy_surface: load!(opt: "vkDestroySurfaceKHR"),
		get_physical_device_surface_formats: load!(opt: "vkGetPhysicalDeviceSurfaceFormatsKHR"),
		get_physical_device_surface_formats2: load!(opt: "vkGetPhysicalDeviceSurfaceFormats2KHR"),
		get_physical_device_surface_capabilities: load!(opt: "vkGetPhysicalDeviceSurfaceCapabilitiesKHR"),
		get_physical_device_surface_capabilities2: load!(opt: "vkGetPhysicalDeviceSurfaceCapabilities2KHR"),
		get_physical_device_surface_present_modes: load!(opt: "vkGetPhysicalDeviceSurfacePresentModesKHR"),
		get_physical_device_wayland_presentation_support: load!(opt: "vkGetPhysicalDeviceWaylandPresentationSupportKHR"),
		get_physical_device_xcb_presentation_support: load!(opt: "vkGetPhysicalDeviceXcbPresentationSupportKHR"),
		get_physical_device_xlib_presentation_support: load!(opt: "vkGetPhysicalDeviceXlibPresentationSupportKHR"),
		enumerate_device_extension_properties: load!(opt: "vkEnumerateDeviceExtensionProperties"),
	}
}

/// Attempt to connect to the Moonshine compositor on MOONSHINE_WAYLAND_DISPLAY.
fn connect_to_compositor(display_name: &CStr) -> Option<Arc<Mutex<WaylandConnection>>> {
	let name_str = display_name.to_string_lossy();

	// Build the socket path: absolute or relative to XDG_RUNTIME_DIR.
	let socket_path = {
		if name_str.starts_with('/') {
			name_str.into_owned()
		} else {
			let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
			format!("{}/{}", runtime_dir, name_str)
		}
	};

	let stream = std::os::unix::net::UnixStream::connect(&socket_path).ok()?;
	let connection = Connection::from_socket(stream).ok()?;

	let (globals, event_queue) = registry_queue_init::<WaylandState>(&connection).ok()?;
	let qh = event_queue.handle();

	let compositor: WlCompositor = globals.bind(&qh, 4..=5, ()).ok()?;
	let factory: MoonshineSwapchainFactoryV2 = globals.bind(&qh, 1..=1, ()).ok()?;

	Some(Arc::new(Mutex::new(WaylandConnection {
		connection,
		compositor: compositor.clone(),
		swapchain_factory: factory.clone(),
		caps: CompositorCaps {
			_compositor_version: compositor.version(),
			_factory_version: factory.version(),
			// Use the MOONSHINE_HDR env var (set by the compositor for HDR
			// sessions) rather than hard-coding true.  The factory global is
			// always registered now (including SDR sessions), so presence of
			// the global alone no longer implies HDR capability.
			hdr_supported: std::env::var("MOONSHINE_HDR").map(|v| v == "1").unwrap_or(false),
		},
		event_queue,
		qh,
		dead: false,
	})))
}
