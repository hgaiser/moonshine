//! Zero-copy DMA-BUF import into Vulkan images for video encoding.
//!
//! `DmaBufImporter` caches `VkImage`/`VkDeviceMemory` per DMA-BUF file descriptor,
//! avoiding per-frame `vkCreateImage` + `vkAllocateMemory` (~0.5–1.5ms on NVIDIA).
//!
//! The cache is keyed by raw fd rather than `wl_buffer` ObjectId because the
//! NVIDIA Wayland WSI creates a new `wl_buffer` wrapper each frame while the
//! underlying DMA-BUF fd is stable.
//!
//! Kernel fd recycling is caught by a two-layer check on every cache hit:
//! 1. Import parameters (format, dimensions, modifier) — catches genuine
//!    reconfigurations (e.g. PQ → scRGB HDR switch).
//! 2. `kcmp(2)` open-file-description comparison — catches a recycled fd number
//!    pointing at a new buffer whose parameters happen to be identical.
//!
//! Each cache entry holds a `dup` of its fd for the `kcmp(2)` comparison.
//!
//! The startup healthcheck verifies `kcmp(2)` is available; startup is refused
//! without it.

use ash::vk;
use pixelforge::VideoContext;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::io::{BorrowedFd, IntoRawFd};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// TTL before a cached import is evicted. 2s covers in-flight GPU work
/// (~16ms depth-2 at 120fps) while keeping VRAM under control.
const CACHE_TTL: Duration = Duration::from_secs(2);

/// Sweep for stale entries every N `import_or_reuse` calls.
const SWEEP_INTERVAL_CALLS: u32 = 60;

/// A single DMA-BUF plane descriptor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DmaBufPlane {
	pub fd: RawFd,
	/// Offset within the DMA-BUF to the start of this plane.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
	/// DRM format modifier.
	pub modifier: u64,
}

/// Identifying parameters of a DMA-BUF import. A cached image may only be
/// reused when all of these match the new request; a mismatch on the same fd
/// means the fd number was recycled for a different buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImportParams {
	width: u32,
	height: u32,
	format: vk::Format,
	modifier: u64,
	plane_count: usize,
	/// (offset, stride) per plane.
	plane_layouts: [(u32, u32); 4],
}

impl ImportParams {
	fn new(width: u32, height: u32, format: vk::Format, planes: &[DmaBufPlane]) -> Self {
		let mut plane_layouts = [(0, 0); 4];
		for (i, p) in planes.iter().take(4).enumerate() {
			plane_layouts[i] = (p.offset, p.stride);
		}
		Self {
			width,
			height,
			format,
			modifier: planes.first().map_or(0, |p| p.modifier),
			plane_count: planes.len().min(4),
			plane_layouts,
		}
	}
}

/// Cached Vulkan resources for a single compositor buffer slot.
struct CachedImport {
	image: vk::Image,
	memory: vk::DeviceMemory,
	params: ImportParams,
	/// Duplicate of the DMA-BUF fd, held for `kcmp(2)` identity checks.
	fd: OwnedFd,
	last_used: Instant,
}

/// Check whether two fds refer to the same open file description via `kcmp(2)`.
///
/// All DMA-BUF fds in a session are dups of the exporter's single `struct file`,
/// so "same open file description" means "same buffer".
///
/// The startup healthcheck guarantees `kcmp(2)` is available, so this never
/// returns an error at runtime.
pub(crate) fn same_open_file(a: RawFd, b: RawFd) -> bool {
	/// `KCMP_FILE` — not exposed by `libc` or glibc.
	const KCMP_FILE: libc::c_int = 0;

	// SAFETY: syscall with scalar arguments; fds are only compared, not accessed.
	let result = unsafe {
		libc::syscall(
			libc::SYS_kcmp,
			libc::getpid(),
			libc::getpid(),
			KCMP_FILE,
			a as libc::c_ulong,
			b as libc::c_ulong,
		)
	};

	assert!(
		result >= 0,
		"kcmp(2) failed unexpectedly — the startup healthcheck should have caught this"
	);

	// 0 = identical, 1/2 = ordering between distinct files.
	result == 0
}

impl CachedImport {
	/// Check whether `fd` refers to the same DMA-BUF this entry was imported from.
	fn is_same_buffer(&self, fd: RawFd) -> bool {
		same_open_file(self.fd.as_raw_fd(), fd)
	}
}

/// Imports DMA-BUF fds into Vulkan images with a per-fd cache and TTL eviction.
///
/// Layout transitions are deferred to the consumer (e.g. `ColorConverter`) to
/// avoid a separate GPU submission per first-time import.
pub(crate) struct DmaBufImporter {
	context: VideoContext,
	external_memory_fd: ash::khr::external_memory_fd::Device,
	/// Per-fd cache. Keyed by raw fd because the NVIDIA WSI creates a new
	/// `wl_buffer` wrapper each frame. Recycled fd numbers are caught by
	/// `CachedImport::is_same_buffer`, not by the key.
	cache: HashMap<RawFd, CachedImport>,
	/// Imports whose fd was recycled for a different buffer, awaiting TTL
	/// expiry before destruction (in-flight GPU work may still reference them).
	retired: Vec<CachedImport>,
	/// Calls since the last stale-entry sweep.
	calls_since_sweep: u32,
}

impl DmaBufImporter {
	/// Create a new DMA-BUF importer.
	pub fn new(context: VideoContext) -> Result<Self, String> {
		let external_memory_fd = ash::khr::external_memory_fd::Device::load(context.instance(), context.device());

		Ok(Self {
			context,
			external_memory_fd,
			cache: HashMap::new(),
			retired: Vec::new(),
			calls_since_sweep: 0,
		})
	}

	/// Import a DMA-BUF as a Vulkan image, reusing a cached import on hit.
	///
	/// Returns `(image, needs_transition)` — `needs_transition` is `true` for
	/// first-time imports whose image is still in `UNDEFINED` layout.
	pub fn import_or_reuse(
		&mut self,
		fd: RawFd,
		width: u32,
		height: u32,
		format: vk::Format,
		planes: &[DmaBufPlane],
	) -> Result<(vk::Image, bool), String> {
		self.calls_since_sweep += 1;
		if self.calls_since_sweep >= SWEEP_INTERVAL_CALLS {
			self.calls_since_sweep = 0;
			self.evict_stale();
		}

		let params = ImportParams::new(width, height, format, planes);

		let now = Instant::now();
		if let Some(cached) = self.cache.get_mut(&fd)
			&& cached.params == params
			&& cached.is_same_buffer(fd)
		{
			cached.last_used = now;
			return Ok((cached.image, false));
		}

		// Cache miss: the fd now points to a different buffer or it was
		// reconfigured. Retire the stale entry for TTL-deferred destruction
		// so in-flight GPU work can drain first.
		if let Some(mut stale) = self.cache.remove(&fd) {
			debug!(
				"fd {fd} now refers to a different buffer (params {:?} -> {:?}); retiring stale import",
				stale.params, params
			);
			stale.last_used = now;
			self.retired.push(stale);
		}

		debug!(
			"First import for fd {fd}: {}x{}, format={:?}, stride={}, modifier={:#x}",
			width, height, format, planes[0].stride, planes[0].modifier
		);

		// Keep a dup for `kcmp(2)` identity comparison.
		let owned_fd = unsafe { BorrowedFd::borrow_raw(fd) }
			.try_clone_to_owned()
			.map_err(|e| format!("Failed to duplicate DMA-BUF FD for the import cache: {e}"))?;

		let (image, memory) = self.import_internal(width, height, format, planes)?;

		self.cache.insert(
			fd,
			CachedImport {
				image,
				memory,
				params,
				fd: owned_fd,
				last_used: now,
			},
		);
		Ok((image, true))
	}

	/// Free Vulkan resources for entries that haven't been touched in `CACHE_TTL`.
	fn evict_stale(&mut self) {
		let cutoff = Instant::now() - CACHE_TTL;
		let device = self.context.device();
		let destroy_if_expired = |v: &CachedImport| {
			if v.last_used < cutoff {
				unsafe {
					device.destroy_image(v.image, None);
					device.free_memory(v.memory, None);
				}
				false
			} else {
				true
			}
		};
		let before = self.cache.len() + self.retired.len();
		self.cache.retain(|_, v| destroy_if_expired(v));
		self.retired.retain(destroy_if_expired);
		let evicted = before - self.cache.len() - self.retired.len();
		if evicted > 0 {
			trace!(
				"DmaBufImporter: evicted {evicted} stale cache entries, {} live",
				self.cache.len()
			);
		}
	}

	/// Raw Vulkan DMA-BUF import. Returns `(VkImage, VkDeviceMemory)` in `UNDEFINED` layout.
	fn import_internal(
		&self,
		width: u32,
		height: u32,
		format: vk::Format,
		planes: &[DmaBufPlane],
	) -> Result<(vk::Image, vk::DeviceMemory), String> {
		if planes.is_empty() {
			return Err("At least one DMA-BUF plane is required".to_string());
		}

		let device = self.context.device();

		// Build DRM format modifier plane layouts for all planes.
		// AMD modifiers (e.g. tiled/DCC) may require multiple planes;
		// the layout count must match the modifier's expected plane count.
		let plane_layouts: Vec<vk::SubresourceLayout> = planes
			.iter()
			.map(|p| {
				vk::SubresourceLayout::default()
					.offset(p.offset as u64)
					.row_pitch(p.stride as u64)
			})
			.collect();

		let modifier = planes[0].modifier;
		let mut drm_format_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
			.drm_format_modifier(modifier)
			.plane_layouts(&plane_layouts);

		let mut external_memory_info =
			vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
		external_memory_info.p_next =
			&mut drm_format_modifier_info as *mut vk::ImageDrmFormatModifierExplicitCreateInfoEXT as *mut _;

		let mut image_create_info = vk::ImageCreateInfo::default()
			.image_type(vk::ImageType::TYPE_2D)
			.format(format)
			.extent(vk::Extent3D {
				width,
				height,
				depth: 1,
			})
			.mip_levels(1)
			.array_layers(1)
			.samples(vk::SampleCountFlags::TYPE_1)
			.tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
			.usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::SAMPLED)
			.sharing_mode(vk::SharingMode::EXCLUSIVE)
			.initial_layout(vk::ImageLayout::UNDEFINED);
		image_create_info.p_next = &mut external_memory_info as *mut vk::ExternalMemoryImageCreateInfo as *mut _;

		let image = unsafe { device.create_image(&image_create_info, None) }
			.map_err(|e| format!("DMA-BUF image creation: {e}"))?;

		// Memory requirements.
		let mem_requirements = unsafe { device.get_image_memory_requirements(image) };

		// FD memory properties.
		let mut memory_fd_properties = vk::MemoryFdPropertiesKHR::default();
		unsafe {
			self.external_memory_fd.get_memory_fd_properties(
				vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
				planes[0].fd,
				&mut memory_fd_properties,
			)
		}
		.map_err(|e| format!("Failed to get memory FD properties: {e}"))?;

		// Duplicate the FD — vkAllocateMemory consumes it.
		let fd = unsafe { BorrowedFd::borrow_raw(planes[0].fd) }
			.try_clone_to_owned()
			.map_err(|e| format!("Failed to duplicate DMA-BUF FD: {e}"))?
			.into_raw_fd();

		let mut import_memory_fd_info = vk::ImportMemoryFdInfoKHR::default()
			.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
			.fd(fd);

		let memory_type_bits = mem_requirements.memory_type_bits & memory_fd_properties.memory_type_bits;

		debug!(
			"Memory allocation: size={}, image_type_bits={:#x}, fd_type_bits={:#x}, combined={:#x}",
			mem_requirements.size,
			mem_requirements.memory_type_bits,
			memory_fd_properties.memory_type_bits,
			memory_type_bits
		);

		let memory_type_index = self
			.context
			.find_memory_type(memory_type_bits, vk::MemoryPropertyFlags::empty())
			.ok_or_else(|| "No suitable memory type for DMA-BUF import".to_string())?;

		// Dedicated allocation (required by many drivers for external memory).
		let mut dedicated_alloc_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
		import_memory_fd_info.p_next = &mut dedicated_alloc_info as *mut vk::MemoryDedicatedAllocateInfo as *mut _;

		let mut alloc_info = vk::MemoryAllocateInfo::default()
			.allocation_size(mem_requirements.size)
			.memory_type_index(memory_type_index);
		alloc_info.p_next = &mut import_memory_fd_info as *mut vk::ImportMemoryFdInfoKHR as *mut _;

		let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
			unsafe { device.destroy_image(image, None) };
			format!("DMA-BUF memory import: {e}")
		})?;

		if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
			unsafe {
				device.free_memory(memory, None);
				device.destroy_image(image, None);
			}
			return Err(format!("DMA-BUF memory bind: {e}"));
		}

		Ok((image, memory))
	}
}

impl Drop for DmaBufImporter {
	fn drop(&mut self) {
		let device = self.context.device();
		unsafe {
			for cached in self.cache.drain().map(|(_, v)| v).chain(self.retired.drain(..)) {
				device.destroy_image(cached.image, None);
				device.free_memory(cached.memory, None);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{same_open_file, DmaBufPlane, ImportParams};
	use ash::vk;
	use std::os::fd::AsRawFd;

	fn plane(offset: u32, stride: u32) -> DmaBufPlane {
		DmaBufPlane {
			fd: 42,
			offset,
			stride,
			modifier: 0xdead,
		}
	}

	#[test]
	fn import_params_match_for_identical_buffer() {
		let a = ImportParams::new(1920, 1080, vk::Format::A2B10G10R10_UNORM_PACK32, &[plane(0, 7680)]);
		let b = ImportParams::new(1920, 1080, vk::Format::A2B10G10R10_UNORM_PACK32, &[plane(0, 7680)]);
		assert_eq!(a, b);
	}

	#[test]
	fn import_params_detect_recycled_fd_after_format_switch() {
		// PQ → scRGB: format and stride differ, so the fd is a cache miss despite same fd number.
		let pq = ImportParams::new(1920, 1080, vk::Format::A2B10G10R10_UNORM_PACK32, &[plane(0, 7680)]);
		let scrgb = ImportParams::new(1920, 1080, vk::Format::R16G16B16A16_SFLOAT, &[plane(0, 15360)]);
		assert_ne!(pq, scrgb);
	}

	#[test]
	fn import_params_detect_stride_only_change() {
		let a = ImportParams::new(1920, 1080, vk::Format::R16G16B16A16_SFLOAT, &[plane(0, 15360)]);
		let b = ImportParams::new(1920, 1080, vk::Format::R16G16B16A16_SFLOAT, &[plane(0, 16384)]);
		assert_ne!(a, b);
	}

	#[test]
	fn kcmp_distinguishes_open_file_descriptions() {
		let a = std::fs::File::open("/dev/null").unwrap();
		// Same open file description (shared `struct file`).
		let dup = a.try_clone().unwrap();
		// Different open file description, same inode (separate `open(2)`).
		let other = std::fs::File::open("/dev/null").unwrap();

		assert!(same_open_file(a.as_raw_fd(), dup.as_raw_fd()));
		assert!(!same_open_file(a.as_raw_fd(), other.as_raw_fd()));
	}
}
