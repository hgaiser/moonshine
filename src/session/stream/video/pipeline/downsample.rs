use ash::vk;
use pixelforge::VideoContext;

pub struct Downsampler {
	context: VideoContext,
	dst_image: vk::Image,
	dst_memory: vk::DeviceMemory,
	command_pool: vk::CommandPool,
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
	src_width: u32,
	src_height: u32,
	dst_width: u32,
	dst_height: u32,
	vk_format: vk::Format,
}

impl Downsampler {
	pub fn new(
		context: VideoContext,
		src_width: u32,
		src_height: u32,
		dst_width: u32,
		dst_height: u32,
		vk_format: vk::Format,
	) -> Result<Self, String> {
		let device = context.device();

		// Verify the format supports blit source and destination.
		let format_props = unsafe {
			context
				.instance()
				.get_physical_device_format_properties(context.physical_device(), vk_format)
		};
		let required = vk::FormatFeatureFlags::BLIT_DST;
		let optimal_features = format_props.optimal_tiling_features;
		if !optimal_features.contains(required) {
			return Err(format!(
				"Format {vk_format:?} does not support BLIT_DST (features: {optimal_features:?})"
			));
		}

		// Create destination image at encode resolution.
		let image_info = vk::ImageCreateInfo::default()
			.image_type(vk::ImageType::TYPE_2D)
			.format(vk_format)
			.extent(vk::Extent3D { width: dst_width, height: dst_height, depth: 1 })
			.mip_levels(1)
			.array_layers(1)
			.samples(vk::SampleCountFlags::TYPE_1)
			.tiling(vk::ImageTiling::OPTIMAL)
			.usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::SAMPLED)
			.sharing_mode(vk::SharingMode::EXCLUSIVE)
			.initial_layout(vk::ImageLayout::UNDEFINED);

		let dst_image =
			unsafe { device.create_image(&image_info, None) }.map_err(|e| format!("Downsampler image creation: {e}"))?;

		let mem_req = unsafe { device.get_image_memory_requirements(dst_image) };
		let memory_type_index = context
			.find_memory_type(mem_req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
			.ok_or_else(|| {
				unsafe { device.destroy_image(dst_image, None) };
				"No device-local memory type for downsampler".to_string()
			})?;

		let alloc_info = vk::MemoryAllocateInfo::default()
			.allocation_size(mem_req.size)
			.memory_type_index(memory_type_index);
		let dst_memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
			unsafe { device.destroy_image(dst_image, None) };
			format!("Downsampler memory allocation: {e}")
		})?;

		if let Err(e) = unsafe { device.bind_image_memory(dst_image, dst_memory, 0) } {
			unsafe {
				device.free_memory(dst_memory, None);
				device.destroy_image(dst_image, None);
			}
			return Err(format!("Downsampler memory bind: {e}"));
		}

		// Create command pool on the transfer queue family.
		let pool_info =
			vk::CommandPoolCreateInfo::default().queue_family_index(context.compute_queue_family()).flags(
				vk::CommandPoolCreateFlags::TRANSIENT | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
			);
		let command_pool = unsafe { device.create_command_pool(&pool_info, None) }.map_err(|e| {
			unsafe {
				device.free_memory(dst_memory, None);
				device.destroy_image(dst_image, None);
			}
			format!("Downsampler command pool: {e}")
		})?;

		let alloc_info = vk::CommandBufferAllocateInfo::default()
			.command_pool(command_pool)
			.level(vk::CommandBufferLevel::PRIMARY)
			.command_buffer_count(1);
		let command_buffer = unsafe { device.allocate_command_buffers(&alloc_info) }.map_err(|e| {
			unsafe {
				device.destroy_command_pool(command_pool, None);
				device.free_memory(dst_memory, None);
				device.destroy_image(dst_image, None);
			}
			format!("Downsampler command buffer: {e}")
		})?[0];

		let fence_info = vk::FenceCreateInfo::default();
		let fence = unsafe { device.create_fence(&fence_info, None) }.map_err(|e| {
			unsafe {
				device.destroy_command_pool(command_pool, None);
				device.free_memory(dst_memory, None);
				device.destroy_image(dst_image, None);
			}
			format!("Downsampler fence: {e}")
		})?;

		Ok(Self {
			context,
			dst_image,
			dst_memory,
			command_pool,
			command_buffer,
			fence,
			src_width,
			src_height,
			dst_width,
			dst_height,
			vk_format,
		})
	}

	/// Blit `src_image` (at render resolution) down to the internal dst image (at encode resolution).
	/// Returns the dst image in `GENERAL` layout.
	pub fn blit(&mut self, src_image: vk::Image, src_layout: vk::ImageLayout) -> Result<vk::Image, String> {
		let device = self.context.device();

		let begin_info =
			vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
		unsafe { device.begin_command_buffer(self.command_buffer, &begin_info) }
			.map_err(|e| format!("Downsampler begin command buffer: {e}"))?;

		let subresource = vk::ImageSubresourceRange::default()
			.aspect_mask(vk::ImageAspectFlags::COLOR)
			.base_mip_level(0)
			.level_count(1)
			.base_array_layer(0)
			.layer_count(1);

		// Transition src → TRANSFER_SRC_OPTIMAL.
		let src_barrier = vk::ImageMemoryBarrier::default()
			.old_layout(src_layout)
			.new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(src_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
			.dst_access_mask(vk::AccessFlags::TRANSFER_READ);

		// Transition dst → TRANSFER_DST_OPTIMAL.
		let dst_barrier = vk::ImageMemoryBarrier::default()
			.old_layout(vk::ImageLayout::UNDEFINED)
			.new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(self.dst_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::empty())
			.dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);

		unsafe {
			device.cmd_pipeline_barrier(
				self.command_buffer,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::PipelineStageFlags::TRANSFER,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[src_barrier, dst_barrier],
			);
		}

		let subresource_layers = vk::ImageSubresourceLayers::default()
			.aspect_mask(vk::ImageAspectFlags::COLOR)
			.mip_level(0)
			.base_array_layer(0)
			.layer_count(1);

		let blit_region = vk::ImageBlit::default()
			.src_subresource(subresource_layers)
			.src_offsets([
				vk::Offset3D { x: 0, y: 0, z: 0 },
				vk::Offset3D { x: self.src_width as i32, y: self.src_height as i32, z: 1 },
			])
			.dst_subresource(subresource_layers)
			.dst_offsets([
				vk::Offset3D { x: 0, y: 0, z: 0 },
				vk::Offset3D { x: self.dst_width as i32, y: self.dst_height as i32, z: 1 },
			]);

		unsafe {
			device.cmd_blit_image(
				self.command_buffer,
				src_image,
				vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
				self.dst_image,
				vk::ImageLayout::TRANSFER_DST_OPTIMAL,
				&[blit_region],
				vk::Filter::LINEAR,
			);
		}

		// Transition src back → GENERAL.
		let src_barrier_back = vk::ImageMemoryBarrier::default()
			.old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
			.new_layout(vk::ImageLayout::GENERAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(src_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::TRANSFER_READ)
			.dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);

		// Transition dst → GENERAL for downstream consumers.
		let dst_barrier_back = vk::ImageMemoryBarrier::default()
			.old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
			.new_layout(vk::ImageLayout::GENERAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(self.dst_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
			.dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);

		unsafe {
			device.cmd_pipeline_barrier(
				self.command_buffer,
				vk::PipelineStageFlags::TRANSFER,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[src_barrier_back, dst_barrier_back],
			);

			device.end_command_buffer(self.command_buffer).map_err(|e| format!("Downsampler end command buffer: {e}"))?;
		}

		let submit_info =
			vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
		unsafe {
			device
				.queue_submit(self.context.compute_queue(), &[submit_info], self.fence)
				.map_err(|e| format!("Downsampler queue submit: {e}"))?;
			device
				.wait_for_fences(&[self.fence], true, u64::MAX)
				.map_err(|e| format!("Downsampler fence wait: {e}"))?;
			device.reset_fences(&[self.fence]).map_err(|e| format!("Downsampler fence reset: {e}"))?;
			device
				.reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
				.map_err(|e| format!("Downsampler command buffer reset: {e}"))?;
		}

		Ok(self.dst_image)
	}

	pub fn vk_format(&self) -> vk::Format {
		self.vk_format
	}
}

impl Drop for Downsampler {
	fn drop(&mut self) {
		let device = self.context.device();
		unsafe {
			device.destroy_fence(self.fence, None);
			device.destroy_command_pool(self.command_pool, None);
			device.free_memory(self.dst_memory, None);
			device.destroy_image(self.dst_image, None);
		}
	}
}
