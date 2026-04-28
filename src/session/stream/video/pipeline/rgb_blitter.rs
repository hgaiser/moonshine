//! GPU-side RGB image copy used when the encoder is configured for
//! `VK_VALVE_video_encode_rgb_conversion`.
//!
//! When RGB-direct encode is enabled, VCN5 does the RGB→YUV conversion
//! itself, so the moonshine pipeline no longer needs the compute-shader
//! `ColorConverter`. Instead we just have to land the imported DMA-BUF
//! pixels into the encoder's input image. This module performs that copy
//! with `vkCmdCopyImage` on the transfer queue, including the layout
//! transitions that the encoder expects (`VIDEO_ENCODE_SRC_KHR` ↔
//! `TRANSFER_DST_OPTIMAL`).
//!
//! Async semaphore-signaled, mirroring `pixelforge::ColorConverter`: a
//! single fence + signal semaphore are reused across frames; on entry we
//! wait on the previous frame's fence (so the semaphore is free to
//! re-signal) and on exit we submit and return immediately, handing the
//! semaphore back to the caller for the encoder to wait on. Lets blit
//! and encode overlap on the GPU the same way convert/encode do.

use ash::vk;
use pixelforge::VideoContext;

pub struct RgbBlitter {
	context: VideoContext,
	command_pool: vk::CommandPool,
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
	signal_semaphore: vk::Semaphore,
	in_flight: bool,
	width: u32,
	height: u32,
}

impl RgbBlitter {
	pub fn new(context: VideoContext, width: u32, height: u32) -> Result<Self, String> {
		let device = context.device();
		let queue_family = context.transfer_queue_family();

		let pool_info = vk::CommandPoolCreateInfo::default()
			.queue_family_index(queue_family)
			.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
		let command_pool = unsafe { device.create_command_pool(&pool_info, None) }
			.map_err(|e| format!("RgbBlitter: create_command_pool: {e}"))?;

		let alloc_info = vk::CommandBufferAllocateInfo::default()
			.command_pool(command_pool)
			.level(vk::CommandBufferLevel::PRIMARY)
			.command_buffer_count(1);
		let command_buffer = unsafe { device.allocate_command_buffers(&alloc_info) }
			.map_err(|e| format!("RgbBlitter: allocate_command_buffers: {e}"))?[0];

		let fence_info = vk::FenceCreateInfo::default();
		let fence = unsafe { device.create_fence(&fence_info, None) }
			.map_err(|e| format!("RgbBlitter: create_fence: {e}"))?;

		let semaphore_info = vk::SemaphoreCreateInfo::default();
		let signal_semaphore = unsafe { device.create_semaphore(&semaphore_info, None) }
			.map_err(|e| format!("RgbBlitter: create_semaphore: {e}"))?;

		Ok(Self {
			context,
			command_pool,
			command_buffer,
			fence,
			signal_semaphore,
			in_flight: false,
			width,
			height,
		})
	}

	/// Submit a copy of `src_image` (in `src_layout`) into `dst_image` (the
	/// encoder's input image, currently in `VIDEO_ENCODE_SRC_KHR`). Returns
	/// a binary semaphore that signals when the copy is done — the caller
	/// should pass it to the encoder's submit as a wait dependency, just
	/// like `ColorConverter::convert()`.
	///
	/// Submission is async: the function returns once the command buffer is
	/// queue-submitted, not when the GPU finishes. The semaphore is reused
	/// across frames; the next call waits on the previous frame's fence
	/// before resubmitting so the semaphore is free to re-signal.
	pub fn blit(
		&mut self,
		src_image: vk::Image,
		src_layout: vk::ImageLayout,
		dst_image: vk::Image,
	) -> Result<vk::Semaphore, String> {
		let device = self.context.device();

		// Wait for the previous frame's submission to complete before
		// re-recording the command buffer and re-signaling the semaphore.
		if self.in_flight {
			unsafe { device.wait_for_fences(&[self.fence], true, u64::MAX) }
				.map_err(|e| format!("RgbBlitter: wait_for_fences: {e}"))?;
			self.in_flight = false;
		}

		unsafe { device.reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty()) }
			.map_err(|e| format!("RgbBlitter: reset_command_buffer: {e}"))?;

		let begin_info = vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
		unsafe { device.begin_command_buffer(self.command_buffer, &begin_info) }
			.map_err(|e| format!("RgbBlitter: begin_command_buffer: {e}"))?;

		let subresource = vk::ImageSubresourceRange {
			aspect_mask: vk::ImageAspectFlags::COLOR,
			base_mip_level: 0,
			level_count: 1,
			base_array_layer: 0,
			layer_count: 1,
		};

		// Transition both images: src → TRANSFER_SRC, dst → TRANSFER_DST.
		// We don't care about previous contents of either, so UNDEFINED is
		// an acceptable old layout for the source on first import (the
		// caller signals this via `src_layout = UNDEFINED`).
		let src_barrier = vk::ImageMemoryBarrier::default()
			.old_layout(src_layout)
			.new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(src_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::empty())
			.dst_access_mask(vk::AccessFlags::TRANSFER_READ);

		let dst_barrier = vk::ImageMemoryBarrier::default()
			.old_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
			.new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(dst_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::empty())
			.dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);

		unsafe {
			device.cmd_pipeline_barrier(
				self.command_buffer,
				vk::PipelineStageFlags::TOP_OF_PIPE,
				vk::PipelineStageFlags::TRANSFER,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[src_barrier, dst_barrier],
			);
		}

		let copy = vk::ImageCopy::default()
			.src_subresource(vk::ImageSubresourceLayers {
				aspect_mask: vk::ImageAspectFlags::COLOR,
				mip_level: 0,
				base_array_layer: 0,
				layer_count: 1,
			})
			.src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
			.dst_subresource(vk::ImageSubresourceLayers {
				aspect_mask: vk::ImageAspectFlags::COLOR,
				mip_level: 0,
				base_array_layer: 0,
				layer_count: 1,
			})
			.dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
			.extent(vk::Extent3D {
				width: self.width,
				height: self.height,
				depth: 1,
			});

		unsafe {
			device.cmd_copy_image(
				self.command_buffer,
				src_image,
				vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
				dst_image,
				vk::ImageLayout::TRANSFER_DST_OPTIMAL,
				&[copy],
			);
		}

		// Transition dst back to VIDEO_ENCODE_SRC_KHR for the encoder.
		let dst_back = vk::ImageMemoryBarrier::default()
			.old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
			.new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(dst_image)
			.subresource_range(subresource)
			.src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
			.dst_access_mask(vk::AccessFlags::empty());

		unsafe {
			device.cmd_pipeline_barrier(
				self.command_buffer,
				vk::PipelineStageFlags::TRANSFER,
				vk::PipelineStageFlags::BOTTOM_OF_PIPE,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[dst_back],
			);
		}

		unsafe { device.end_command_buffer(self.command_buffer) }
			.map_err(|e| format!("RgbBlitter: end_command_buffer: {e}"))?;

		// Submit with semaphore signal — do NOT wait for completion. The
		// caller passes the returned semaphore to the encoder's submit as
		// a wait dependency, so blit and encode overlap on the GPU.
		unsafe { device.reset_fences(&[self.fence]) }
			.map_err(|e| format!("RgbBlitter: reset_fences: {e}"))?;

		let command_buffers = [self.command_buffer];
		let signal_semaphores = [self.signal_semaphore];
		let submit = vk::SubmitInfo::default()
			.command_buffers(&command_buffers)
			.signal_semaphores(&signal_semaphores);

		unsafe { device.queue_submit(self.context.transfer_queue(), &[submit], self.fence) }
			.map_err(|e| format!("RgbBlitter: queue_submit: {e}"))?;

		self.in_flight = true;
		Ok(self.signal_semaphore)
	}
}

impl Drop for RgbBlitter {
	fn drop(&mut self) {
		let device = self.context.device();
		unsafe {
			let _ = device.device_wait_idle();
			device.destroy_semaphore(self.signal_semaphore, None);
			device.destroy_fence(self.fence, None);
			device.destroy_command_pool(self.command_pool, None);
		}
	}
}
