//! Global state maps: Vulkan handle → layer-private data.

use std::collections::{HashMap, VecDeque};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Extension trait that recovers from poisoned mutexes instead of panicking.
pub(crate) trait MutexExt<T> {
	fn force_lock(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
	fn force_lock(&self) -> MutexGuard<'_, T> {
		self.lock().unwrap_or_else(|e| {
			use std::sync::atomic::{AtomicBool, Ordering};
			static WARNED: AtomicBool = AtomicBool::new(false);
			if !WARNED.swap(true, Ordering::Relaxed) {
				crate::log_warn!("Mutex poisoned (thread panicked), proceeding anyway");
			}
			e.into_inner()
		})
	}
}

/// Extension trait that recovers from poisoned RwLocks instead of panicking.
pub(crate) trait RwLockExt<T> {
	fn force_read(&self) -> RwLockReadGuard<'_, T>;
	fn force_write(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
	fn force_read(&self) -> RwLockReadGuard<'_, T> {
		self.read().unwrap_or_else(|e| {
			use std::sync::atomic::{AtomicBool, Ordering};
			static WARNED: AtomicBool = AtomicBool::new(false);
			if !WARNED.swap(true, Ordering::Relaxed) {
				crate::log_warn!("RwLock poisoned (thread panicked), proceeding anyway");
			}
			e.into_inner()
		})
	}

	fn force_write(&self) -> RwLockWriteGuard<'_, T> {
		self.write().unwrap_or_else(|e| {
			use std::sync::atomic::{AtomicBool, Ordering};
			static WARNED: AtomicBool = AtomicBool::new(false);
			if !WARNED.swap(true, Ordering::Relaxed) {
				crate::log_warn!("RwLock poisoned (thread panicked), proceeding anyway");
			}
			e.into_inner()
		})
	}
}

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

// ---------------------------------------------------------------------------
// MOONSHINE_LIMITER_FILE support
// ---------------------------------------------------------------------------

/// Cached file descriptor for `MOONSHINE_LIMITER_FILE`.
static LIMITER_FD: OnceLock<RawFd> = OnceLock::new();

/// Read the current frame limiter override value.
///
/// Opens `MOONSHINE_LIMITER_FILE` on first call (caching the fd), then reads
/// a `u32` at offset 0 via `pread` on every subsequent call.  Returns `0` if
/// the env var is unset or the file cannot be read.
pub fn frame_limiter_override() -> u32 {
	let fd = *LIMITER_FD.get_or_init(|| {
		std::env::var_os("MOONSHINE_LIMITER_FILE")
			.and_then(|path| {
				let c_path = std::ffi::CString::new(path.as_encoded_bytes()).ok()?;
				let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
				if fd < 0 {
					None
				} else {
					Some(fd)
				}
			})
			.unwrap_or(-1)
	});

	if fd < 0 {
		return 0;
	}

	let mut value: u32 = 0;
	unsafe {
		libc::pread(
			fd,
			&mut value as *mut u32 as *mut libc::c_void,
			std::mem::size_of::<u32>(),
			0,
		);
	}
	value
}

/// Returns `true` when the limiter file contains `1` (force FIFO mode).
pub fn is_forcing_fifo() -> bool {
	frame_limiter_override() == 1
}

// ---------------------------------------------------------------------------
// Type-safe map keys
// ---------------------------------------------------------------------------

/// Dispatch-table key for the instance map (from VkInstance / VkPhysicalDevice).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceKey(pub(crate) usize);

/// Dispatch-table key for the device map (from VkDevice / VkQueue).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceKey(pub(crate) usize);

/// Raw-handle key for the surface map (from VkSurfaceKHR).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceKey(u64);

impl SurfaceKey {
	pub fn from_raw(raw: u64) -> Self {
		Self(raw)
	}
}

/// Raw-handle key for the swapchain map (from VkSwapchainKHR).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SwapchainKey(u64);

impl SwapchainKey {
	pub fn from_raw(raw: u64) -> Self {
		Self(raw)
	}
	pub fn raw(self) -> u64 {
		self.0
	}
}

use crate::dispatch::{DeviceDispatch, InstanceDispatch, VkSurface};
use crate::proto::moonshine_swapchain::MoonshineSwapchain;
use crate::proto::moonshine_swapchain_factory_v2::MoonshineSwapchainFactoryV2;

// ---------------------------------------------------------------------------
// Wayland dispatch state
// ---------------------------------------------------------------------------

/// Stateless dispatch stand-in.  All event handling writes directly into
/// `SWAPCHAIN_MAP` using the VkSwapchainKHR raw handle stored as `UserData`.
pub struct WaylandState;

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlRegistry,
		_: wayland_client::protocol::wl_registry::Event,
		_: &GlobalListContents,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<WlCompositor, ()> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlCompositor,
		_: wayland_client::protocol::wl_compositor::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<MoonshineSwapchainFactoryV2, ()> for WaylandState {
	fn event(
		_: &mut Self,
		_: &MoonshineSwapchainFactoryV2,
		_: crate::proto::moonshine_swapchain_factory_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<WlSurface, ()> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlSurface,
		_: wayland_client::protocol::wl_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

/// `UserData` for `MoonshineSwapchain` objects = the `VkSwapchainKHR` raw handle.
impl Dispatch<MoonshineSwapchain, u64> for WaylandState {
	fn event(
		_state: &mut Self,
		_proxy: &MoonshineSwapchain,
		event: crate::proto::moonshine_swapchain::Event,
		swapchain_key: &u64,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		use crate::proto::moonshine_swapchain::Event;
		let key = SwapchainKey::from_raw(*swapchain_key);
		match event {
			Event::RefreshCycle {
				refresh_cycle_hi,
				refresh_cycle_lo,
			} => {
				let ns = ((refresh_cycle_hi as u64) << 32) | (refresh_cycle_lo as u64);
				with_swapchain_mut(key, |d| d.refresh_cycle_ns = ns);
			},
			Event::Retired => {
				with_swapchain_mut(key, |d| d.retired = true);
			},
			Event::PastPresentTiming {
				present_id,
				desired_present_time_hi,
				desired_present_time_lo,
				actual_present_time_hi,
				actual_present_time_lo,
				earliest_present_time_hi,
				earliest_present_time_lo,
				present_margin_hi,
				present_margin_lo,
			} => {
				let timing = PastPresentTiming {
					present_id,
					desired_present_time: ((desired_present_time_hi as u64) << 32) | (desired_present_time_lo as u64),
					actual_present_time: ((actual_present_time_hi as u64) << 32) | (actual_present_time_lo as u64),
					earliest_present_time: ((earliest_present_time_hi as u64) << 32)
						| (earliest_present_time_lo as u64),
					present_margin: ((present_margin_hi as u64) << 32) | (present_margin_lo as u64),
				};
				with_swapchain_mut(key, |d| {
					if d.past_timings.len() >= 16 {
						d.past_timings.pop_front();
					}
					d.past_timings.push_back(timing);
				});
			},
		}
	}
}

// ---------------------------------------------------------------------------
// Per-instance state
// ---------------------------------------------------------------------------

/// Whether the layer is fully active or operating in degraded (passthrough)
/// mode.  Exposed so intercepts can fast-path when no compositor is available.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LayerStatus {
	/// Compositor connected — full interception active.
	Active,
	/// No compositor found at instance creation time — passthrough mode.
	Degraded,
}

pub struct InstanceData {
	/// Next layer/ICD dispatch functions.
	pub dispatch: InstanceDispatch,
	/// Whether the layer has a live compositor connection.
	pub status: LayerStatus,
	/// Connection to the Moonshine Wayland compositor.
	/// Wrapped in Arc<Mutex> so that vkQueuePresentKHR can dispatch Wayland
	/// events without holding the INSTANCE_MAP lock.
	pub wayland: Option<Arc<Mutex<WaylandConnection>>>,
	/// Whether the application opted into frame-limiter awareness (via env var
	/// or engine auto-detection of DXVK ≥ 2.3 / vkd3d ≥ 2.12).
	pub frame_limiter_aware: bool,
}

/// Capabilities negotiated with the compositor at connection time.
///
/// The version fields are stored for diagnostics and gating future features.
pub struct CompositorCaps {
	/// The bound version of `wl_compositor`.
	pub _compositor_version: u32,
	/// The bound version of `moonshine_swapchain_factory_v2`.
	pub _factory_version: u32,
	/// Whether the compositor supports HDR output.
	pub hdr_supported: bool,
}

/// Live connection to the Moonshine compositor Wayland socket.
pub struct WaylandConnection {
	pub connection: Connection,
	pub compositor: WlCompositor,
	pub swapchain_factory: MoonshineSwapchainFactoryV2,
	pub caps: CompositorCaps,
	pub event_queue: EventQueue<WaylandState>,
	pub qh: QueueHandle<WaylandState>,
	/// Set when a dispatch or flush error is detected, indicating the
	/// compositor connection is lost.
	pub dead: bool,
}

impl WaylandConnection {
	/// Dispatch any pending Wayland events without blocking.
	/// Returns `false` if the connection is dead.
	pub fn dispatch_pending(&mut self) -> bool {
		if self.dead {
			return false;
		}
		if let Err(e) = self.event_queue.dispatch_pending(&mut WaylandState) {
			crate::log_warn!("Wayland dispatch error: {e}");
			self.dead = true;
			return false;
		}
		true
	}

	/// Flush outgoing Wayland requests.
	/// Returns `false` if the connection is dead.
	pub fn flush(&mut self) -> bool {
		if self.dead {
			return false;
		}
		if let Err(e) = self.connection.flush() {
			crate::log_warn!("Wayland flush error: {e}");
			self.dead = true;
			return false;
		}
		true
	}
}

// ---------------------------------------------------------------------------
// Per-device state
// ---------------------------------------------------------------------------

pub struct DeviceData {
	pub dispatch: DeviceDispatch,
	/// Key of the owning VkInstance (for looking up instance data in
	/// swapchain/present hooks that only receive a VkDevice).
	pub instance_key: InstanceKey,
	/// Whether `VK_EXT_swapchain_maintenance1` was successfully enabled.
	pub has_maintenance1: bool,
}

// ---------------------------------------------------------------------------
// Per-surface state
// ---------------------------------------------------------------------------

pub struct SurfaceData {
	/// The `wl_surface` on the Moonshine compositor associated with this
	/// tracked Vulkan surface.
	///
	/// Only XWayland bypass surfaces are tracked; native Wayland surfaces
	/// pass through to the ICD without an entry in `SURFACE_MAP`.
	pub wl_surface: WlSurface,
	/// For XWayland bypass: the original XCB window ID.
	pub xcb_window: Option<u32>,
	/// For XWayland bypass: the opaque XCB connection pointer for live
	/// geometry queries in the capabilities hook.
	pub xcb_connection: *mut libc::c_void,
}

// SAFETY: The raw xcb_connection pointer is process-global and thread-safe
// (XCB connections are fully thread-safe by design).
unsafe impl Send for SurfaceData {}
unsafe impl Sync for SurfaceData {}

// ---------------------------------------------------------------------------
// Per-swapchain state
// ---------------------------------------------------------------------------

pub struct PastPresentTiming {
	pub present_id: u32,
	pub desired_present_time: u64,
	pub actual_present_time: u64,
	pub earliest_present_time: u64,
	pub present_margin: u64,
}

pub struct SwapchainData {
	/// Dispatch key of the owning VkDevice.
	pub device_key: DeviceKey,
	pub present_mode: ash::vk::PresentModeKHR,
	// Stored at creation time for diagnostics; not yet read at runtime.
	pub _format: ash::vk::Format,
	pub _color_space: ash::vk::ColorSpaceKHR,
	pub _image_count: u32,
	pub _extent: ash::vk::Extent2D,
	pub _surface: VkSurface,
	/// The `moonshine_swapchain` protocol object.
	pub ms_swapchain: Option<MoonshineSwapchain>,
	/// Compositor refresh period in nanoseconds (updated by refresh_cycle event).
	pub refresh_cycle_ns: u64,
	/// Set to true when the compositor fires the `retired` event.
	pub retired: bool,
	/// Whether `is_forcing_fifo()` was true at swapchain creation time.
	pub force_fifo_at_creation: bool,
	/// Ring buffer of past presentation timings.
	pub past_timings: VecDeque<PastPresentTiming>,
}

// ---------------------------------------------------------------------------
// Global maps
//
// All maps use RwLock for concurrent read access (most operations are reads).
// Write locks are only needed for insert/remove/mutate operations.
//
// Lock ordering (if ever holding multiple locks):
//   INSTANCE_MAP → DEVICE_MAP → SURFACE_MAP → SWAPCHAIN_MAP
//
// Currently every access acquires exactly one map lock at a time via the
// `with_*` helpers, so ordering is not yet critical. This comment exists to
// prevent future regressions.
// ---------------------------------------------------------------------------

pub static INSTANCE_MAP: OnceLock<RwLock<HashMap<InstanceKey, InstanceData>>> = OnceLock::new();
pub static DEVICE_MAP: OnceLock<RwLock<HashMap<DeviceKey, DeviceData>>> = OnceLock::new();
pub static SURFACE_MAP: OnceLock<RwLock<HashMap<SurfaceKey, SurfaceData>>> = OnceLock::new();
pub static SWAPCHAIN_MAP: OnceLock<RwLock<HashMap<SwapchainKey, SwapchainData>>> = OnceLock::new();

fn instance_map() -> &'static RwLock<HashMap<InstanceKey, InstanceData>> {
	INSTANCE_MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

fn device_map() -> &'static RwLock<HashMap<DeviceKey, DeviceData>> {
	DEVICE_MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

fn surface_map() -> &'static RwLock<HashMap<SurfaceKey, SurfaceData>> {
	SURFACE_MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

fn swapchain_map() -> &'static RwLock<HashMap<SwapchainKey, SwapchainData>> {
	SWAPCHAIN_MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn insert_instance(key: InstanceKey, data: InstanceData) {
	instance_map().force_write().insert(key, data);
}

pub fn remove_instance(key: InstanceKey) -> Option<InstanceData> {
	instance_map().force_write().remove(&key)
}

pub fn with_instance<R>(key: InstanceKey, f: impl FnOnce(&InstanceData) -> R) -> Option<R> {
	instance_map().force_read().get(&key).map(f)
}

/// Returns `true` when the layer is fully active for the given instance
/// (i.e. the compositor was reached during vkCreateInstance).
pub fn is_layer_active(key: InstanceKey) -> bool {
	with_instance(key, |d| d.status == LayerStatus::Active).unwrap_or(false)
}

/// Get the `Arc<Mutex<WaylandConnection>>` for an instance without holding the map lock.
pub fn get_wayland_connection(key: InstanceKey) -> Option<Arc<Mutex<WaylandConnection>>> {
	with_instance(key, |d| d.wayland.clone()).flatten()
}

/// Check if the application is frame-limiter-aware for the given instance.
pub fn is_frame_limiter_aware(key: InstanceKey) -> bool {
	with_instance(key, |d| d.frame_limiter_aware).unwrap_or(false)
}

pub fn insert_device(key: DeviceKey, data: DeviceData) {
	device_map().force_write().insert(key, data);
}

pub fn remove_device(key: DeviceKey) -> Option<DeviceData> {
	device_map().force_write().remove(&key)
}

pub fn with_device<R>(key: DeviceKey, f: impl FnOnce(&DeviceData) -> R) -> Option<R> {
	device_map().force_read().get(&key).map(f)
}

pub fn insert_surface(key: SurfaceKey, data: SurfaceData) {
	surface_map().force_write().insert(key, data);
}

pub fn remove_surface(key: SurfaceKey) {
	surface_map().force_write().remove(&key);
}

pub fn with_surface<R>(key: SurfaceKey, f: impl FnOnce(&SurfaceData) -> R) -> Option<R> {
	surface_map().force_read().get(&key).map(f)
}

pub fn insert_swapchain(key: SwapchainKey, data: SwapchainData) {
	swapchain_map().force_write().insert(key, data);
}

pub fn remove_swapchain(key: SwapchainKey) {
	swapchain_map().force_write().remove(&key);
}

pub fn with_swapchain<R>(key: SwapchainKey, f: impl FnOnce(&SwapchainData) -> R) -> Option<R> {
	swapchain_map().force_read().get(&key).map(f)
}

pub fn with_swapchain_mut<R>(key: SwapchainKey, f: impl FnOnce(&mut SwapchainData) -> R) -> Option<R> {
	swapchain_map().force_write().get_mut(&key).map(f)
}
