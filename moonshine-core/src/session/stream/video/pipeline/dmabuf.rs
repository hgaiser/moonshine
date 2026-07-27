//! DMA-BUF import support for zero-copy video encoding.
//!
//! This module provides the ability to import Linux DMA-BUF file descriptors as
//! Vulkan images for direct video encoding without CPU-side copies.
//!
//! `DmaBufImporter` caches imported Vulkan resources per DMA-BUF file descriptor
//! so that the same underlying buffer is imported only once. Subsequent frames
//! reusing the same DMA-BUF reuse the cached `VkImage` and `VkDeviceMemory`,
//! eliminating per-frame `vkCreateImage` + `vkAllocateMemory(DMA-BUF import)`
//! calls that can cost 0.5\u20131.5ms on NVIDIA drivers.
//!
//! Keying by FD rather than `wl_buffer` ObjectId is critical: the NVIDIA
//! Wayland WSI creates a new `wl_buffer` wrapper each frame even though the
//! underlying DMA-BUF fd is stable. ObjectId-keying would miss the cache
//! every frame; FD-keying catches the reuse.
//!
//! The kernel also recycles fd numbers for *new* buffers: once a `wl_buffer`
//! is released and its fd closed, the same number comes back for an unrelated
//! buffer. A cache hit on the fd number alone may therefore be a different
//! buffer, and the import parameters cannot tell — swapchain images share
//! identical dimensions, format, stride and modifier. Each entry keeps its own
//! `dup` of the fd and a hit is only trusted when `kcmp(2)` confirms that the
//! incoming fd refers to the *same open file description*; mismatched entries
//! are retired and freed after `CACHE_TTL`, once any in-flight GPU work using
//! them has completed.
//!
//! The cache evicts entries that have not been touched in `CACHE_TTL`. The
//! compositor holds client buffers alive until the encoder signals `consumed`,
//! so the fd is guaranteed valid during import. The 2s TTL ensures any
//! in-flight GPU work completes before cached Vulkan resources are freed.

use ash::vk;
use pixelforge::VideoContext;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::io::{BorrowedFd, IntoRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

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
pub(crate) struct DmaBufPlane {
	/// File descriptor for the DMA-BUF.
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
	/// Our own `dup` of the DMA-BUF this image was imported from, kept for the
	/// life of the entry so an incoming fd can be compared against it. It also
	/// pins the buffer, which is what makes the inode fallback below reliable.
	fd: OwnedFd,
	/// Inode of that DMA-BUF, used only when `kcmp(2)` is unavailable.
	inode: Option<u64>,
	last_used: Instant,
}

/// Set once `kcmp(2)` has been seen to be refused outright, so the syscall is
/// not retried every frame.
static KCMP_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// Whether `a` and `b` refer to the same open file description, or `None` when
/// the kernel refuses `kcmp(2)` altogether and the caller must fall back.
///
/// This is what makes fd-keying safe. Kernel fd numbers are recycled: once a
/// `wl_buffer` is released and its fd closed, the same number comes back for an
/// unrelated buffer. `ImportParams` cannot detect that, because swapchain images
/// share identical dimensions, format, stride and modifier — so without an
/// identity check the cache hands back a `VkImage` bound to a previous buffer's
/// memory and the encoder re-encodes a stale frame (the permanent shimmer seen
/// on some games in HDR direct scanout).
///
/// All fds for one dma-buf are dups of the exporter's single `struct file`
/// (`dma_buf_fd()` installs `dmabuf->file`, and SCM_RIGHTS passes it across the
/// socket unchanged), so "same open file description" is exactly "same buffer"
/// here.
fn same_open_file(a: RawFd, b: RawFd) -> Option<bool> {
	/// `KCMP_FILE` — compare a single open file description. Not exposed by the
	/// `libc` crate on Linux, nor wrapped by glibc, so both the constant and the
	/// syscall are spelled out here.
	const KCMP_FILE: libc::c_int = 0;

	if KCMP_UNSUPPORTED.load(Ordering::Relaxed) {
		return None;
	}

	// SAFETY: a plain syscall with scalar arguments; the fds are only compared,
	// never read from or written to.
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
	if result >= 0 {
		// 0 means identical; 1 and 2 are an ordering between distinct files.
		return Some(result == 0);
	}

	let error = std::io::Error::last_os_error();
	match error.raw_os_error() {
		// No CONFIG_CHECKPOINT_RESTORE, or a seccomp policy in the way. This
		// cannot change at runtime, so stop asking and use the inode instead.
		Some(libc::ENOSYS) | Some(libc::EPERM) => {
			warn!("kcmp(2) unavailable ({error}); falling back to DMA-BUF inode comparison");
			KCMP_UNSUPPORTED.store(true, Ordering::Relaxed);
			None
		},
		// EBADF and friends: whatever that descriptor is, it is not the buffer
		// this entry was imported from.
		_ => Some(false),
	}
}

/// Inode of a DMA-BUF, or `None` if it cannot be determined.
///
/// Fallback identity for kernels that refuse `kcmp(2)`. Every dma-buf gets a
/// distinct inode on the anonymous dmabuf filesystem and keeps it for its whole
/// lifetime; inode numbers are only recycled once the buffer is freed, which a
/// live cache entry prevents — it holds a `dup` of the fd, as does the
/// `VkDeviceMemory` imported from it.
fn dmabuf_inode(fd: RawFd) -> Option<u64> {
	// SAFETY: `stat` is only read after fstat reports success.
	unsafe {
		let mut st: libc::stat = std::mem::zeroed();
		if libc::fstat(fd, &mut st) == 0 {
			Some(st.st_ino as u64)
		} else {
			None
		}
	}
}

impl CachedImport {
	/// Whether `fd` still refers to the buffer this entry was imported from.
	///
	/// Refuses reuse when neither check can answer, rather than risking a
	/// `VkImage` bound to another buffer's memory.
	fn is_same_buffer(&self, fd: RawFd) -> bool {
		match same_open_file(self.fd.as_raw_fd(), fd) {
			Some(same) => same,
			None => match (self.inode, dmabuf_inode(fd)) {
				(Some(cached), Some(incoming)) => cached == incoming,
				_ => false,
			},
		}
	}
}

/// Importer for DMA-BUF file descriptors into Vulkan images.
///
/// Owns a per-FD cache of `VkImage` + `VkDeviceMemory` with TTL eviction.
/// Layout transitions are deferred to the consumer (e.g. `ColorConverter`)
/// to avoid a separate GPU submission per first-time import.
pub(crate) struct DmaBufImporter {
	context: VideoContext,
	external_memory_fd: ash::khr::external_memory_fd::Device,
	/// Per-FD cache. Keying on the fd is what makes reuse possible at all —
	/// NVIDIA's Wayland WSI wraps a new `wl_buffer` every frame, so keying on
	/// anything derived from the `wl_buffer` would allocate a fresh entry per
	/// frame and defeat the cache entirely. Recycled fd numbers are caught by
	/// `CachedImport::is_same_buffer` in `import_or_reuse`, not by the key.
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

	/// Import a DMA-BUF as a Vulkan image, reusing a cached import when
	/// the same DMA-BUF fd has been seen before with the same parameters.
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
		if let Some(cached) = self.cache.get_mut(&fd) {
			// Both must match: the parameters catch a genuine reconfiguration,
			// the identity check catches an fd number recycled for a different
			// buffer — which the parameters alone cannot see, since swapchain
			// images are identical on every field they compare.
			if cached.params == params && cached.is_same_buffer(fd) {
				cached.last_used = now;
				return Ok((cached.image, false));
			}
		}

		// A leftover entry here means the fd number now refers to a different
		// buffer, or the same buffer was reconfigured. Reusing the stale image
		// would make the GPU read the wrong memory — silently wrong pixels at
		// best, a device-lost fault at worst. Retire it for TTL-deferred
		// destruction in case prior GPU work still references it.
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

		// Kept for the life of the entry: `is_same_buffer` compares against it,
		// and holding it pins the buffer so its inode cannot be recycled either.
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
				inode: dmabuf_inode(fd),
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
		// PQ→scRGB swapchain recreation: the new FP16 buffer can land on the
		// same fd number as the destroyed 10-bit buffer, with a different
		// format and stride. The cache must not treat this as a hit.
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
		// A dup shares the open file description, exactly like the fd the
		// importer keeps against the one the compositor hands it each frame.
		let dup = a.try_clone().unwrap();
		// A separate open is a different description of the same inode — the
		// case a stat-based comparison could not tell apart.
		let other = std::fs::File::open("/dev/null").unwrap();

		let Some(dup_is_same) = same_open_file(a.as_raw_fd(), dup.as_raw_fd()) else {
			// No CONFIG_CHECKPOINT_RESTORE, or seccomp: the inode fallback
			// applies and there is nothing to assert here.
			return;
		};
		assert!(dup_is_same);
		assert_eq!(same_open_file(a.as_raw_fd(), other.as_raw_fd()), Some(false));
	}
}
