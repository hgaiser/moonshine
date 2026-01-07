//! DMA-BUF import support for zero-copy video encoding.
//!
//! This module provides the ability to import Linux DMA-BUF file descriptors as Vulkan.
//! images for direct video encoding without CPU-side copies.

use ash::vk;
use pixelforge::VideoContext;
use std::os::fd::RawFd;
use tracing::debug;

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

/// A Vulkan image imported from a DMA-BUF.
///
/// This image can be passed directly to the encoder. The image is in GENERAL layout.
/// after import and is ready for use.
///
/// The DMA-BUF file descriptor is NOT duplicated - the caller must ensure the FD.
/// remains valid for the lifetime of this image.
pub struct DmaBufImage {
	context: VideoContext,
	image: vk::Image,
	memory: vk::DeviceMemory,
	command_pool: vk::CommandPool,
	#[allow(dead_code)]
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
}

impl DmaBufImage {
	/// Get the underlying Vulkan image handle.
	pub fn image(&self) -> vk::Image {
		self.image
	}
}

impl Drop for DmaBufImage {
	fn drop(&mut self) {
		let device = self.context.device();
		unsafe {
			device.destroy_fence(self.fence, None);
			device.destroy_command_pool(self.command_pool, None);
			device.destroy_image(self.image, None);
			device.free_memory(self.memory, None);
		}
	}
}

/// Importer for DMA-BUF file descriptors into Vulkan images.
pub struct DmaBufImporter {
	context: VideoContext,
	#[allow(dead_code)]
	external_memory_fd: ash::khr::external_memory_fd::Device,
}

impl DmaBufImporter {
	/// Create a new DMA-BUF importer.
	///
	/// This requires the Vulkan context to have the appropriate external memory.
	/// extensions enabled.
	pub fn new(context: VideoContext) -> Result<Self, String> {
		let external_memory_fd = ash::khr::external_memory_fd::Device::load(context.instance(), context.device());

		Ok(Self {
			context,
			external_memory_fd,
		})
	}

	/// Import an NV12 DMA-BUF as a Vulkan image.
	///
	/// The DMA-BUF must contain NV12 data (Y plane followed by interleaved UV plane).
	/// For a single-plane NV12 buffer, pass a single plane in `planes`.
	pub fn import_nv12(&mut self, width: u32, height: u32, planes: &[DmaBufPlane]) -> Result<DmaBufImage, String> {
		if planes.is_empty() {
			return Err("At least one DMA-BUF plane is required".to_string());
		}

		debug!(
			"Importing NV12 DMA-BUF: {}x{}, {} planes, fd={}, stride={}, modifier={:#x}",
			width,
			height,
			planes.len(),
			planes[0].fd,
			planes[0].stride,
			planes[0].modifier
		);

		let device = self.context.device();
		let format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

		// Build DRM format modifier info.
		// For NV12, we have 2 planes (Y and UV).
		let mut plane_layouts = Vec::new();
		let y_offset = planes[0].offset as u64;
		plane_layouts.push(
			vk::SubresourceLayout::default()
				.offset(y_offset)
				.row_pitch(planes[0].stride as u64),
		);

		// UV plane is at offset after Y plane (for single-buffer NV12).
		// For a single-plane buffer, UV starts at y_offset + y_size.
		let y_size = planes[0].stride as u64 * height as u64;
		let uv_offset = if planes.len() > 1 {
			planes[1].offset as u64
		} else {
			y_offset + y_size
		};
		let uv_stride = if planes.len() > 1 {
			planes[1].stride as u64
		} else {
			planes[0].stride as u64
		};
		plane_layouts.push(
			vk::SubresourceLayout::default()
				.offset(uv_offset)
				.row_pitch(uv_stride),
		);

		let modifier = planes[0].modifier;
		let mut drm_format_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
			.drm_format_modifier(modifier)
			.plane_layouts(&plane_layouts);

		// External memory image create info.
		let mut external_memory_info =
			vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
		external_memory_info.p_next =
			&mut drm_format_modifier_info as *mut vk::ImageDrmFormatModifierExplicitCreateInfoEXT as *mut _;

		// Create the image.
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
			.map_err(|e| format!("DMA-BUF image creation: {}", e))?;

		// Get memory requirements.
		let mem_requirements = unsafe { device.get_image_memory_requirements(image) };

		// Get memory properties from the FD.
		let mut memory_fd_properties = vk::MemoryFdPropertiesKHR::default();
		unsafe {
			self.external_memory_fd.get_memory_fd_properties(
				vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
				planes[0].fd,
				&mut memory_fd_properties,
			)
		}
		.map_err(|e| format!("Failed to get memory FD properties: {}", e))?;

		// Import memory from DMA-BUF FD.
		let mut import_memory_fd_info = vk::ImportMemoryFdInfoKHR::default()
			.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
			.fd(planes[0].fd);

		// Filter memory types by what the FD supports.
		let memory_type_bits = mem_requirements.memory_type_bits & memory_fd_properties.memory_type_bits;

		// Allocate and import memory.
		let memory_type_index = self
			.context
			.find_memory_type(memory_type_bits, vk::MemoryPropertyFlags::empty())
			.ok_or_else(|| "No suitable memory type for DMA-BUF import".to_string())?;

		// Use dedicated allocation for external memory (required by many drivers).
		let mut dedicated_alloc_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
		import_memory_fd_info.p_next = &mut dedicated_alloc_info as *mut vk::MemoryDedicatedAllocateInfo as *mut _;

		let mut alloc_info = vk::MemoryAllocateInfo::default()
			.allocation_size(mem_requirements.size)
			.memory_type_index(memory_type_index);
		alloc_info.p_next = &mut import_memory_fd_info as *mut vk::ImportMemoryFdInfoKHR as *mut _;

		// Note: After this call, the FD is consumed/owned by Vulkan.
		let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
			unsafe { device.destroy_image(image, None) };
			format!("DMA-BUF memory import: {}", e)
		})?;

		// Bind memory to image.
		if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
			unsafe {
				device.free_memory(memory, None);
				device.destroy_image(image, None);
			}
			return Err(format!("DMA-BUF memory bind: {}", e));
		}

		// Create command pool and buffer for layout transitions.
		let pool_info = vk::CommandPoolCreateInfo::default()
			.queue_family_index(self.context.transfer_queue_family())
			.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);

		let command_pool = unsafe { device.create_command_pool(&pool_info, None) }
			.map_err(|e| format!("Command pool creation: {}", e))?;

		let alloc_info = vk::CommandBufferAllocateInfo::default()
			.command_pool(command_pool)
			.level(vk::CommandBufferLevel::PRIMARY)
			.command_buffer_count(1);

		let command_buffers = unsafe { device.allocate_command_buffers(&alloc_info) }
			.map_err(|e| format!("Command buffer allocation: {}", e))?;
		let command_buffer = command_buffers[0];

		let fence_info = vk::FenceCreateInfo::default();
		let fence = unsafe { device.create_fence(&fence_info, None) }.map_err(|e| format!("Fence creation: {}", e))?;

		// Transition image to GENERAL layout.
		self.transition_image_layout(&command_buffer, &fence, image)?;

		Ok(DmaBufImage {
			context: self.context.clone(),
			image,
			memory,
			command_pool,
			command_buffer,
			fence,
		})
	}

	/// Import an RGB/RGBA DMA-BUF as a Vulkan image.
	pub fn import_rgba(&mut self, width: u32, height: u32, planes: &[DmaBufPlane]) -> Result<DmaBufImage, String> {
		self.import_rgb_internal(width, height, planes, vk::Format::R8G8B8A8_UNORM)
	}

	/// Import a BGR/BGRA DMA-BUF as a Vulkan image.
	pub fn import_bgra(&mut self, width: u32, height: u32, planes: &[DmaBufPlane]) -> Result<DmaBufImage, String> {
		self.import_rgb_internal(width, height, planes, vk::Format::B8G8R8A8_UNORM)
	}

	fn import_rgb_internal(
		&mut self,
		width: u32,
		height: u32,
		planes: &[DmaBufPlane],
		format: vk::Format,
	) -> Result<DmaBufImage, String> {
		if planes.is_empty() {
			return Err("At least one DMA-BUF plane is required".to_string());
		}

		debug!(
			"Importing RGB DMA-BUF: {}x{}, format={:?}, fd={}, stride={}, modifier={:#x}",
			width, height, format, planes[0].fd, planes[0].stride, planes[0].modifier
		);

		let device = self.context.device();

		// Build DRM format modifier info for single-plane RGB.
		// Note: size must be 0 for VkImageDrmFormatModifierExplicitCreateInfoEXT.
		let plane_layout = vk::SubresourceLayout::default()
			.offset(planes[0].offset as u64)
			.row_pitch(planes[0].stride as u64);
		let plane_layouts = [plane_layout];

		let modifier = planes[0].modifier;
		let mut drm_format_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
			.drm_format_modifier(modifier)
			.plane_layouts(&plane_layouts);

		// External memory image create info.
		let mut external_memory_info =
			vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
		external_memory_info.p_next =
			&mut drm_format_modifier_info as *mut vk::ImageDrmFormatModifierExplicitCreateInfoEXT as *mut _;

		// Create the image.
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
			.map_err(|e| format!("DMA-BUF image creation: {}", e))?;

		// Get memory requirements.
		let mem_requirements = unsafe { device.get_image_memory_requirements(image) };

		// Get memory properties from the FD.
		let mut memory_fd_properties = vk::MemoryFdPropertiesKHR::default();
		unsafe {
			self.external_memory_fd.get_memory_fd_properties(
				vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
				planes[0].fd,
				&mut memory_fd_properties,
			)
		}
		.map_err(|e| format!("Failed to get memory FD properties: {}", e))?;

		// Import memory from DMA-BUF FD.
		let mut import_memory_fd_info = vk::ImportMemoryFdInfoKHR::default()
			.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
			.fd(planes[0].fd);

		// Filter memory types by what the FD supports.
		let memory_type_bits = mem_requirements.memory_type_bits & memory_fd_properties.memory_type_bits;

		debug!(
			"Memory allocation: size={}, image_type_bits={:#x}, fd_type_bits={:#x}, combined={:#x}",
			mem_requirements.size,
			mem_requirements.memory_type_bits,
			memory_fd_properties.memory_type_bits,
			memory_type_bits
		);

		// Allocate and import memory.
		let memory_type_index = self
			.context
			.find_memory_type(memory_type_bits, vk::MemoryPropertyFlags::empty())
			.ok_or_else(|| "No suitable memory type for DMA-BUF import".to_string())?;

		// Use dedicated allocation for external memory (required by many drivers).
		let mut dedicated_alloc_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
		import_memory_fd_info.p_next = &mut dedicated_alloc_info as *mut vk::MemoryDedicatedAllocateInfo as *mut _;

		let mut alloc_info = vk::MemoryAllocateInfo::default()
			.allocation_size(mem_requirements.size)
			.memory_type_index(memory_type_index);
		alloc_info.p_next = &mut import_memory_fd_info as *mut vk::ImportMemoryFdInfoKHR as *mut _;

		let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
			unsafe { device.destroy_image(image, None) };
			format!("DMA-BUF memory import: {}", e)
		})?;

		// Bind memory to image.
		if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
			unsafe {
				device.free_memory(memory, None);
				device.destroy_image(image, None);
			}
			return Err(format!("DMA-BUF memory bind: {}", e));
		}

		// Create command pool and buffer for layout transitions.
		let pool_info = vk::CommandPoolCreateInfo::default()
			.queue_family_index(self.context.transfer_queue_family())
			.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);

		let command_pool = unsafe { device.create_command_pool(&pool_info, None) }
			.map_err(|e| format!("Command pool creation: {}", e))?;

		let alloc_info = vk::CommandBufferAllocateInfo::default()
			.command_pool(command_pool)
			.level(vk::CommandBufferLevel::PRIMARY)
			.command_buffer_count(1);

		let command_buffers = unsafe { device.allocate_command_buffers(&alloc_info) }
			.map_err(|e| format!("Command buffer allocation: {}", e))?;
		let command_buffer = command_buffers[0];

		let fence_info = vk::FenceCreateInfo::default();
		let fence = unsafe { device.create_fence(&fence_info, None) }.map_err(|e| format!("Fence creation: {}", e))?;

		// Transition image to GENERAL layout.
		self.transition_image_layout(&command_buffer, &fence, image)?;

		Ok(DmaBufImage {
			context: self.context.clone(),
			image,
			memory,
			command_pool,
			command_buffer,
			fence,
		})
	}

	fn transition_image_layout(
		&self,
		command_buffer: &vk::CommandBuffer,
		fence: &vk::Fence,
		image: vk::Image,
	) -> Result<(), String> {
		let device = self.context.device();

		let begin_info = vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
		unsafe { device.begin_command_buffer(*command_buffer, &begin_info) }
			.map_err(|e| format!("Command buffer begin: {}", e))?;

		let barrier = vk::ImageMemoryBarrier::default()
			.old_layout(vk::ImageLayout::UNDEFINED)
			.new_layout(vk::ImageLayout::GENERAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(image)
			.subresource_range(vk::ImageSubresourceRange {
				aspect_mask: vk::ImageAspectFlags::COLOR,
				base_mip_level: 0,
				level_count: 1,
				base_array_layer: 0,
				layer_count: 1,
			})
			.src_access_mask(vk::AccessFlags::empty())
			.dst_access_mask(vk::AccessFlags::MEMORY_READ);

		unsafe {
			device.cmd_pipeline_barrier(
				*command_buffer,
				vk::PipelineStageFlags::TOP_OF_PIPE,
				vk::PipelineStageFlags::BOTTOM_OF_PIPE,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[barrier],
			);
		}

		unsafe { device.end_command_buffer(*command_buffer) }.map_err(|e| format!("Command buffer end: {}", e))?;

		let submit_info = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(command_buffer));

		unsafe { device.queue_submit(self.context.transfer_queue(), &[submit_info], *fence) }
			.map_err(|e| format!("Queue submit failed: {}", e))?;

		unsafe { device.wait_for_fences(&[*fence], true, u64::MAX) }
			.map_err(|e| format!("Fence wait failed: {}", e))?;

		unsafe { device.reset_fences(&[*fence]) }.map_err(|e| format!("Fence reset failed: {}", e))?;

		Ok(())
	}
}
