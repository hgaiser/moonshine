//! DMA-BUF import support for zero-copy video encoding.
//!
//! This module provides the ability to import Linux DMA-BUF file descriptors as
//! Vulkan images for direct video encoding without CPU-side copies.
//!
//! `DmaBufImporter` caches imported Vulkan resources per compositor buffer index
//! so that pre-allocated GBM buffers are imported only once. Subsequent frames
//! from the same buffer reuse the cached `VkImage` and `VkDeviceMemory`,
//! eliminating per-frame Vulkan object creation and layout transitions.
//!
//! The cache evicts entries that have not been touched in `CACHE_TTL`. During
//! real-game streaming, gamescope assigns monotonically-growing `buffer_index`
//! values (~76/sec at 120 fps), so without eviction the cache (and its backing
//! VRAM) would grow unboundedly across a session. With TTL eviction, only the
//! actively-cycling buffers stay resident; old indexes are reclaimed.

use ash::vk;
use pixelforge::VideoContext;
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::os::unix::io::{BorrowedFd, IntoRawFd};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// How long a cached import stays resident after its last use before being
/// evicted and freed. Long enough that any in-flight encoder/blitter work
/// using the image has definitely completed (depth-2 pipeline at 120 fps is
/// ~16 ms of in-flight latency), short enough that monotonically-growing
/// buffer-index churn doesn't accumulate VRAM.
const CACHE_TTL: Duration = Duration::from_secs(2);

/// Sweep for stale cache entries every N `import_or_reuse` calls. Cheap
/// (HashMap retain over a small map) but no point doing it every frame.
const SWEEP_INTERVAL_CALLS: u32 = 60;

/// Information about a single DMA-BUF plane.
#[derive(Debug, Clone, Copy)]
pub struct DmaBufPlane {
	/// File descriptor for the DMA-BUF.
	pub fd: RawFd,
	/// Offset within the DMA-BUF to the start of this plane.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
	/// DRM format modifier.
	pub modifier: u64,
}

/// Cached Vulkan resources for a single compositor buffer slot.
struct CachedImport {
	image: vk::Image,
	memory: vk::DeviceMemory,
	last_used: Instant,
}

/// Importer for DMA-BUF file descriptors into Vulkan images.
///
/// Owns a per-buffer-index cache of `VkImage` + `VkDeviceMemory` with TTL
/// eviction. Layout transitions are deferred to the consumer
/// (e.g. `ColorConverter`/`RgbBlitter`) to avoid a separate GPU submission
/// per first-time import.
pub struct DmaBufImporter {
	context: VideoContext,
	external_memory_fd: ash::khr::external_memory_fd::Device,
	/// Per-buffer-index cache. Switched from `Vec<Option<…>>` to a HashMap so
	/// monotonically-growing `buffer_index` values don't grow a sparse Vec.
	cache: HashMap<usize, CachedImport>,
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
			calls_since_sweep: 0,
		})
	}

	/// Import a DMA-BUF as a Vulkan image, reusing a cached import when
	/// the same `buffer_index` has been seen before.
	///
	/// The `format` parameter specifies the Vulkan format matching the DMA-BUF
	/// pixel format (e.g. `B8G8R8A8_UNORM` for SDR, `A2B10G10R10_UNORM_PACK32`
	/// for 10-bit HDR, `R16G16B16A16_SFLOAT` for FP16 HDR).
	///
	/// Returns `(image, needs_transition)` where `needs_transition` is `true`
	/// for first-time imports whose image is still in `UNDEFINED` layout.
	/// The caller is responsible for transitioning the image (e.g. by passing
	/// the appropriate `src_layout` to `ColorConverter::convert`).
	pub fn import_or_reuse(
		&mut self,
		buffer_index: usize,
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

		let now = Instant::now();
		if let Some(cached) = self.cache.get_mut(&buffer_index) {
			cached.last_used = now;
			return Ok((cached.image, false));
		}

		// First time seeing this buffer — full import.
		debug!(
			"First import for buffer {buffer_index}: {}x{}, format={:?}, fd={}, stride={}, modifier={:#x}",
			width, height, format, planes[0].fd, planes[0].stride, planes[0].modifier
		);

		let (image, memory) = self.import_internal(width, height, format, planes)?;

		self.cache.insert(
			buffer_index,
			CachedImport {
				image,
				memory,
				last_used: now,
			},
		);
		Ok((image, true))
	}

	/// Drop cached entries that haven't been touched in `CACHE_TTL` and free
	/// their backing Vulkan resources. Stale entries are guaranteed to be out
	/// of any encoder/blitter pipeline (TTL >> max in-flight depth at 120 fps),
	/// so it's safe to destroy without an explicit fence wait.
	fn evict_stale(&mut self) {
		let cutoff = Instant::now() - CACHE_TTL;
		let device = self.context.device();
		let before = self.cache.len();
		self.cache.retain(|_, v| {
			if v.last_used < cutoff {
				unsafe {
					device.destroy_image(v.image, None);
					device.free_memory(v.memory, None);
				}
				false
			} else {
				true
			}
		});
		let evicted = before - self.cache.len();
		if evicted > 0 {
			trace!(
				"DmaBufImporter: evicted {evicted} stale cache entries, {} live",
				self.cache.len()
			);
		}
	}

	/// Perform the raw Vulkan import of a DMA-BUF with the specified format.
	///
	/// Returns the `(VkImage, VkDeviceMemory)` pair. The image is in
	/// `UNDEFINED` layout; the caller must transition it.
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
			for (_, cached) in self.cache.drain() {
				device.destroy_image(cached.image, None);
				device.free_memory(cached.memory, None);
			}
		}
	}
}
