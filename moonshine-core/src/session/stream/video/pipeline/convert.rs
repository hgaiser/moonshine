//! Conventional-codec encoder input processing.
//!
//! Converts the captured RGB DMA-BUF into the encoder's YCbCr input image in a
//! single compute dispatch plus one buffer-to-image copy. The dispatch can also
//! composite the compositor cursor while sampling the source, which lets a
//! fullscreen direct export keep its zero-render path while the cursor stays
//! part of the encoded picture (see `compositor/cursor.rs`).
//!
//! The arithmetic mirrors Pixelforge's `ColorConverter` (shared coefficients,
//! transfer functions and truncating quantization) and its packed buffer
//! layout; it differs in how the work is scheduled:
//!
//! * each invocation writes whole 32-bit words for a 4x2 pixel block, so the
//!   per-byte global atomics and the full-buffer clear before them are gone;
//! * the submission prefers the device's dedicated compute family, which on
//!   AMD runs concurrently with a game's graphics queue instead of waiting
//!   behind its frames (a GPU-bound game otherwise delays every conversion by
//!   roughly a frame and caps the stream frame rate);
//! * conversion GPU time is measured with timestamps when supported.
//!
//! Pixelforge's converter remains the fallback for extents this layout cannot
//! represent (odd heights or widths that are not a multiple of four).

use std::sync::Arc;

use ash::vk;
use pixelforge::{ColorSpace, OutputFormat, VideoContext};

use super::dmabuf::CachedImport;
use crate::session::compositor::frame::{MAX_OVERLAYS, OverlayFormat, OverlayImage};

/// Precompiled from `shaders/convert.comp` by `scripts/build-shaders.sh`.
const CONVERT_SPIRV: &[u8] = include_bytes!("shaders/convert.spv");

/// Workgroup footprint in pixels: 8x8 invocations of 4x2 pixels each.
const GROUP_WIDTH: u32 = 32;
const GROUP_HEIGHT: u32 = 16;

/// Views kept for recently seen source images (swapchains rotate 2-4 images).
const MAX_CACHED_VIEWS: usize = 8;

/// Queue preference for the conversion submission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionQueueMode {
	/// Dedicated compute family when the device exposes one, else graphics.
	#[default]
	Auto,
	/// The graphics-capable family Pixelforge uses for its own converter.
	Graphics,
	/// Require the dedicated compute family; falls back to graphics if absent.
	Compute,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PushConstants {
	width: u32,
	height: u32,
	output_format: u32,
	color_space: u32,
	full_range: u32,
	sdr_white_nits: f32,
	_padding: [u32; 2],
	layer_rect: [[i32; 4]; MAX_OVERLAYS],
	layer_opacity: [f32; MAX_OVERLAYS],
	layer_opaque: [u32; MAX_OVERLAYS],
}

/// Where a late-composition layer's texels come from.
pub(crate) enum LayerInput<'a> {
	/// CPU texels, uploaded once per content generation.
	Pixels(&'a OverlayImage),
	/// An imported client DMA-BUF, read in place like the game source.
	Imported {
		import: &'a Arc<CachedImport>,
		format: vk::Format,
		first_use: bool,
		/// Ignore the alpha channel (X formats).
		opaque: bool,
	},
}

/// One layer composited over the source, at `(x, y)` in output pixels.
pub(crate) struct Layer<'a> {
	pub input: LayerInput<'a>,
	pub x: i32,
	pub y: i32,
	pub width: u32,
	pub height: u32,
	pub opacity: f32,
}

/// Layers bottom to top (notification, cursor).
pub(crate) type Layers<'a> = [Option<Layer<'a>>; MAX_OVERLAYS];

/// Whether the packed word layout can represent `width` x `height`.
pub(crate) fn supports_extent(width: u32, height: u32) -> bool {
	width != 0 && height != 0 && width.is_multiple_of(4) && height.is_multiple_of(2)
}

fn output_format_code(format: OutputFormat) -> Option<u32> {
	// Pixelforge's shader numbering; I420 is never negotiated by Pyroshine.
	match format {
		OutputFormat::NV12 => Some(0),
		OutputFormat::YUV444 => Some(2),
		OutputFormat::P010 => Some(3),
		OutputFormat::YUV444P10 => Some(4),
		OutputFormat::I420 => None,
	}
}

fn color_space_code(color_space: ColorSpace) -> u32 {
	match color_space {
		ColorSpace::Bt709 => 0,
		ColorSpace::Bt2020 => 1,
		ColorSpace::SrgbToBt2020Pq => 2,
		ColorSpace::Bt709LinearToBt2020Pq => 3,
	}
}

/// Total packed output bytes, identical to Pixelforge's layout for these formats.
fn output_size(format: OutputFormat, width: u32, height: u32) -> u64 {
	let pixels = u64::from(width) * u64::from(height);
	match format {
		OutputFormat::NV12 | OutputFormat::I420 => pixels * 3 / 2,
		OutputFormat::YUV444 => pixels * 3,
		OutputFormat::P010 => pixels * 3,
		OutputFormat::YUV444P10 => pixels * 6,
	}
}

fn copy_regions(format: OutputFormat, width: u32, height: u32) -> [vk::BufferImageCopy; 2] {
	let pixels = u64::from(width) * u64::from(height);
	let (chroma_offset, chroma_extent) = match format {
		OutputFormat::NV12 => (pixels, (width / 2, height / 2)),
		OutputFormat::P010 => (pixels * 2, (width / 2, height / 2)),
		OutputFormat::YUV444 => (pixels, (width, height)),
		OutputFormat::YUV444P10 | OutputFormat::I420 => (pixels * 2, (width, height)),
	};
	let region = |offset, aspect, (w, h)| vk::BufferImageCopy {
		buffer_offset: offset,
		buffer_row_length: 0,
		buffer_image_height: 0,
		image_subresource: vk::ImageSubresourceLayers {
			aspect_mask: aspect,
			mip_level: 0,
			base_array_layer: 0,
			layer_count: 1,
		},
		image_offset: vk::Offset3D::default(),
		image_extent: vk::Extent3D {
			width: w,
			height: h,
			depth: 1,
		},
	};
	[
		region(0, vk::ImageAspectFlags::PLANE_0, (width, height)),
		region(chroma_offset, vk::ImageAspectFlags::PLANE_1, chroma_extent),
	]
}

fn color_range() -> vk::ImageSubresourceRange {
	vk::ImageSubresourceRange {
		aspect_mask: vk::ImageAspectFlags::COLOR,
		base_mip_level: 0,
		level_count: 1,
		base_array_layer: 0,
		layer_count: 1,
	}
}

fn find_memory_type(context: &VideoContext, type_bits: u32, required: vk::MemoryPropertyFlags) -> Result<u32, String> {
	// SAFETY: plain physical-device property query.
	let properties = unsafe {
		context
			.instance()
			.get_physical_device_memory_properties(context.physical_device())
	};
	(0..properties.memory_type_count)
		.find(|&index| {
			type_bits & (1 << index) != 0
				&& properties.memory_types[index as usize]
					.property_flags
					.contains(required)
		})
		.ok_or_else(|| format!("no memory type with {required:?}"))
}

/// GPU-resident copy of one layer's CPU texels.
struct OverlayTexture {
	image: vk::Image,
	memory: vk::DeviceMemory,
	views: [vk::ImageView; 2],
	staging: vk::Buffer,
	staging_memory: vk::DeviceMemory,
	staging_ptr: *mut u8,
	width: u32,
	height: u32,
	/// Content generation currently in `image`; `None` until first upload.
	generation: Option<u64>,
	/// The image is still UNDEFINED and needs its first layout transition.
	needs_init: bool,
}

impl OverlayTexture {
	const EMPTY: Self = Self {
		image: vk::Image::null(),
		memory: vk::DeviceMemory::null(),
		views: [vk::ImageView::null(); 2],
		staging: vk::Buffer::null(),
		staging_memory: vk::DeviceMemory::null(),
		staging_ptr: std::ptr::null_mut(),
		width: 0,
		height: 0,
		generation: None,
		needs_init: true,
	};
}

/// Five-second conversion summary for `log_stats`.
#[derive(Default)]
pub(crate) struct ConvertWindow {
	started: Option<std::time::Instant>,
	frames: u64,
	gpu_ns: u64,
	gpu_samples: u64,
	late_layer_frames: u64,
	/// Frames dropped because they carried an overlay this converter cannot draw.
	pub overlay_drops: u64,
}

impl ConvertWindow {
	pub(crate) fn record(&mut self, gpu_ns: Option<u64>, layered: bool) {
		self.frames += 1;
		if let Some(ns) = gpu_ns {
			self.gpu_ns += ns;
			self.gpu_samples += 1;
		}
		self.late_layer_frames += u64::from(layered);
	}

	pub(crate) fn maybe_log(&mut self, async_compute: Option<bool>) {
		let started = *self.started.get_or_insert_with(std::time::Instant::now);
		if started.elapsed() < std::time::Duration::from_secs(5) {
			return;
		}
		tracing::info!(
			packed_converter = async_compute.is_some(),
			async_compute = async_compute.unwrap_or(false),
			converted_frames = self.frames,
			convert_gpu_us = (self.gpu_samples != 0).then(|| self.gpu_ns as f64 / self.gpu_samples as f64 / 1e3),
			late_layer_frames = self.late_layer_frames,
			overlay_drops = self.overlay_drops,
			"Video conversion summary"
		);
		*self = Self {
			started: Some(std::time::Instant::now()),
			..Self::default()
		};
	}
}

/// Result of one conversion; the source is no longer read by the GPU.
pub(crate) struct ConvertOutcome {
	/// Measured GPU execution time of the conversion and copy, if supported.
	pub gpu_ns: Option<u64>,
}

pub(crate) struct InputConverter {
	context: VideoContext,
	queue: vk::Queue,
	queue_family: u32,
	/// Whether conversion runs on a dedicated compute family.
	pub(crate) async_compute: bool,
	width: u32,
	height: u32,
	output_format: OutputFormat,
	output_format_code: u32,
	color_space: ColorSpace,
	full_range: bool,
	sdr_white_nits: f32,
	set_layout: vk::DescriptorSetLayout,
	pipeline_layout: vk::PipelineLayout,
	pipeline: vk::Pipeline,
	sampler: vk::Sampler,
	descriptor_pool: vk::DescriptorPool,
	descriptor_set: vk::DescriptorSet,
	output_buffer: vk::Buffer,
	output_memory: vk::DeviceMemory,
	/// Upload textures for CPU-texel layers, one per layer slot.
	layer_textures: [OverlayTexture; MAX_OVERLAYS],
	/// Views of recently used source images. The `Arc` pins each image for as
	/// long as its view exists; entries the importer no longer holds are
	/// dropped first.
	views: Vec<(Arc<CachedImport>, vk::ImageView)>,
	command_pool: vk::CommandPool,
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
	query_pool: vk::QueryPool,
	timestamp_period_ns: f64,
}

impl InputConverter {
	pub(crate) fn new(
		context: VideoContext,
		mode: ConversionQueueMode,
		width: u32,
		height: u32,
		output_format: OutputFormat,
		color_space: ColorSpace,
		full_range: bool,
	) -> Result<Self, String> {
		if !supports_extent(width, height) {
			return Err(format!("{width}x{height} is not representable by the packed converter"));
		}
		let output_format_code =
			output_format_code(output_format).ok_or_else(|| format!("unsupported output format {output_format:?}"))?;
		// SAFETY: plain physical-device property query.
		let families = unsafe {
			context
				.instance()
				.get_physical_device_queue_family_properties(context.physical_device())
		};
		let dedicated = context.transfer_queue_family();
		let dedicated_compute = dedicated != context.compute_queue_family()
			&& families.get(dedicated as usize).is_some_and(|family| {
				family.queue_flags.contains(vk::QueueFlags::COMPUTE)
					&& !family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
			});
		let use_dedicated = dedicated_compute && mode != ConversionQueueMode::Graphics;
		if mode == ConversionQueueMode::Compute && !dedicated_compute {
			tracing::warn!("No dedicated compute queue family is available; converting on the graphics family");
		}
		// The encoder input image is shared CONCURRENTLY by Pixelforge between
		// its encode, transfer and compute families, so either family may write it.
		let (queue_family, queue) = if use_dedicated {
			(dedicated, context.transfer_queue())
		} else {
			(context.compute_queue_family(), context.compute_queue())
		};
		let timestamps = families
			.get(queue_family as usize)
			.is_some_and(|family| family.timestamp_valid_bits != 0);

		let mut converter = Self {
			queue,
			queue_family,
			async_compute: use_dedicated,
			width,
			height,
			output_format,
			output_format_code,
			color_space,
			full_range,
			sdr_white_nits: 203.0,
			set_layout: vk::DescriptorSetLayout::null(),
			pipeline_layout: vk::PipelineLayout::null(),
			pipeline: vk::Pipeline::null(),
			sampler: vk::Sampler::null(),
			descriptor_pool: vk::DescriptorPool::null(),
			descriptor_set: vk::DescriptorSet::null(),
			output_buffer: vk::Buffer::null(),
			output_memory: vk::DeviceMemory::null(),
			layer_textures: [OverlayTexture::EMPTY; MAX_OVERLAYS],
			views: Vec::new(),
			command_pool: vk::CommandPool::null(),
			command_buffer: vk::CommandBuffer::null(),
			fence: vk::Fence::null(),
			query_pool: vk::QueryPool::null(),
			timestamp_period_ns: f64::from(context.device_properties().limits.timestamp_period),
			context,
		};
		// Partially created objects are released by Drop on error.
		converter.create_resources(timestamps)?;
		// 1x1 placeholders keep the layer bindings valid without layers.
		for slot in 0..MAX_OVERLAYS {
			converter.ensure_overlay_capacity(slot, 1, 1)?;
		}
		Ok(converter)
	}

	fn create_resources(&mut self, timestamps: bool) -> Result<(), String> {
		let device = self.context.device().clone();
		let err = |what: &str| {
			let what = what.to_string();
			move |e: vk::Result| format!("{what}: {e}")
		};
		let bindings = [
			vk::DescriptorSetLayoutBinding::default()
				.binding(0)
				.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
			vk::DescriptorSetLayoutBinding::default()
				.binding(1)
				.descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
			vk::DescriptorSetLayoutBinding::default()
				.binding(2)
				.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
			vk::DescriptorSetLayoutBinding::default()
				.binding(3)
				.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
		];
		let push_range = vk::PushConstantRange::default()
			.stage_flags(vk::ShaderStageFlags::COMPUTE)
			.size(std::mem::size_of::<PushConstants>() as u32);
		let pool_sizes = [
			vk::DescriptorPoolSize::default()
				.ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1 + MAX_OVERLAYS as u32),
			vk::DescriptorPoolSize::default()
				.ty(vk::DescriptorType::STORAGE_BUFFER)
				.descriptor_count(1),
		];
		let code = ash::util::read_spv(&mut std::io::Cursor::new(CONVERT_SPIRV))
			.map_err(|e| format!("invalid converter SPIR-V: {e}"))?;
		let size = output_size(self.output_format, self.width, self.height);

		// SAFETY: every handle below is created on this converter's device and
		// stored immediately so Drop destroys it on any later error.
		unsafe {
			self.set_layout = device
				.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)
				.map_err(err("descriptor set layout"))?;
			let set_layouts = [self.set_layout];
			self.pipeline_layout = device
				.create_pipeline_layout(
					&vk::PipelineLayoutCreateInfo::default()
						.set_layouts(&set_layouts)
						.push_constant_ranges(std::slice::from_ref(&push_range)),
					None,
				)
				.map_err(err("pipeline layout"))?;
			let module = device
				.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
				.map_err(err("shader module"))?;
			let stage = vk::PipelineShaderStageCreateInfo::default()
				.stage(vk::ShaderStageFlags::COMPUTE)
				.module(module)
				.name(c"main");
			let pipeline = device.create_compute_pipelines(
				vk::PipelineCache::null(),
				&[vk::ComputePipelineCreateInfo::default()
					.stage(stage)
					.layout(self.pipeline_layout)],
				None,
			);
			device.destroy_shader_module(module, None);
			self.pipeline = pipeline.map_err(|(_, e)| format!("compute pipeline: {e}"))?[0];
			self.sampler = device
				.create_sampler(
					&vk::SamplerCreateInfo::default()
						.mag_filter(vk::Filter::NEAREST)
						.min_filter(vk::Filter::NEAREST)
						.address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
						.address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
						.address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
						.unnormalized_coordinates(false),
					None,
				)
				.map_err(err("sampler"))?;
			self.descriptor_pool = device
				.create_descriptor_pool(
					&vk::DescriptorPoolCreateInfo::default()
						.max_sets(1)
						.pool_sizes(&pool_sizes),
					None,
				)
				.map_err(err("descriptor pool"))?;
			self.descriptor_set = device
				.allocate_descriptor_sets(
					&vk::DescriptorSetAllocateInfo::default()
						.descriptor_pool(self.descriptor_pool)
						.set_layouts(&set_layouts),
				)
				.map_err(err("descriptor set"))?[0];

			self.output_buffer = device
				.create_buffer(
					&vk::BufferCreateInfo::default()
						.size(size)
						.usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC)
						.sharing_mode(vk::SharingMode::EXCLUSIVE),
					None,
				)
				.map_err(err("output buffer"))?;
			let requirements = device.get_buffer_memory_requirements(self.output_buffer);
			let memory_type = find_memory_type(
				&self.context,
				requirements.memory_type_bits,
				vk::MemoryPropertyFlags::DEVICE_LOCAL,
			)?;
			self.output_memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(memory_type),
					None,
				)
				.map_err(err("output memory"))?;
			device
				.bind_buffer_memory(self.output_buffer, self.output_memory, 0)
				.map_err(err("bind output memory"))?;
			let buffer_info = vk::DescriptorBufferInfo::default()
				.buffer(self.output_buffer)
				.offset(0)
				.range(vk::WHOLE_SIZE);
			device.update_descriptor_sets(
				&[vk::WriteDescriptorSet::default()
					.dst_set(self.descriptor_set)
					.dst_binding(1)
					.descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
					.buffer_info(std::slice::from_ref(&buffer_info))],
				&[],
			);

			self.command_pool = device
				.create_command_pool(
					&vk::CommandPoolCreateInfo::default()
						.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
						.queue_family_index(self.queue_family),
					None,
				)
				.map_err(err("command pool"))?;
			self.command_buffer = device
				.allocate_command_buffers(
					&vk::CommandBufferAllocateInfo::default()
						.command_pool(self.command_pool)
						.level(vk::CommandBufferLevel::PRIMARY)
						.command_buffer_count(1),
				)
				.map_err(err("command buffer"))?[0];
			self.fence = device
				.create_fence(&vk::FenceCreateInfo::default(), None)
				.map_err(err("fence"))?;
			if timestamps {
				self.query_pool = device
					.create_query_pool(
						&vk::QueryPoolCreateInfo::default()
							.query_type(vk::QueryType::TIMESTAMP)
							.query_count(2),
						None,
					)
					.map_err(err("timestamp query pool"))?;
			}
		}
		Ok(())
	}

	pub(crate) fn set_color(&mut self, color_space: ColorSpace, full_range: bool, sdr_white_nits: f32) {
		self.color_space = color_space;
		self.full_range = full_range;
		self.sdr_white_nits = sdr_white_nits;
	}

	/// (Re)allocate a slot's upload texture and staging buffer for `width` x `height`.
	fn ensure_overlay_capacity(&mut self, slot: usize, width: u32, height: u32) -> Result<(), String> {
		let texture = &self.layer_textures[slot];
		if texture.width >= width && texture.height >= height && texture.image != vk::Image::null() {
			return Ok(());
		}
		let width = width.max(texture.width).max(1);
		let height = height.max(texture.height).max(1);
		self.destroy_overlay(slot);
		let device = self.context.device().clone();
		let err = |what: &'static str| move |e: vk::Result| format!("overlay {what}: {e}");
		// SAFETY: objects are created on this device, recorded in `self.layer_textures`
		// immediately, and only destroyed after the GPU finished with them
		// (every conversion waits for its fence before returning).
		unsafe {
			self.layer_textures[slot].image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(vk::Format::R8G8B8A8_UNORM)
						.flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
						.extent(vk::Extent3D {
							width,
							height,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
						.sharing_mode(vk::SharingMode::EXCLUSIVE)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.map_err(err("image"))?;
			let requirements = device.get_image_memory_requirements(self.layer_textures[slot].image);
			let memory_type = find_memory_type(
				&self.context,
				requirements.memory_type_bits,
				vk::MemoryPropertyFlags::DEVICE_LOCAL,
			)?;
			self.layer_textures[slot].memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(memory_type),
					None,
				)
				.map_err(err("memory"))?;
			device
				.bind_image_memory(self.layer_textures[slot].image, self.layer_textures[slot].memory, 0)
				.map_err(err("bind memory"))?;
			// RGBA (xcursor) and BGRA (wl_shm ARGB8888) views of the same texels.
			for (view_index, format) in [vk::Format::R8G8B8A8_UNORM, vk::Format::B8G8R8A8_UNORM]
				.into_iter()
				.enumerate()
			{
				self.layer_textures[slot].views[view_index] = device
					.create_image_view(
						&vk::ImageViewCreateInfo::default()
							.image(self.layer_textures[slot].image)
							.view_type(vk::ImageViewType::TYPE_2D)
							.format(format)
							.subresource_range(color_range()),
						None,
					)
					.map_err(err("view"))?;
			}
			let staging_size = u64::from(width) * u64::from(height) * 4;
			self.layer_textures[slot].staging = device
				.create_buffer(
					&vk::BufferCreateInfo::default()
						.size(staging_size)
						.usage(vk::BufferUsageFlags::TRANSFER_SRC)
						.sharing_mode(vk::SharingMode::EXCLUSIVE),
					None,
				)
				.map_err(err("staging buffer"))?;
			let requirements = device.get_buffer_memory_requirements(self.layer_textures[slot].staging);
			let memory_type = find_memory_type(
				&self.context,
				requirements.memory_type_bits,
				vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
			)?;
			self.layer_textures[slot].staging_memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(memory_type),
					None,
				)
				.map_err(err("staging memory"))?;
			device
				.bind_buffer_memory(
					self.layer_textures[slot].staging,
					self.layer_textures[slot].staging_memory,
					0,
				)
				.map_err(err("bind staging memory"))?;
			self.layer_textures[slot].staging_ptr = device
				.map_memory(
					self.layer_textures[slot].staging_memory,
					0,
					vk::WHOLE_SIZE,
					vk::MemoryMapFlags::empty(),
				)
				.map_err(err("map staging memory"))?
				.cast();
			// Transparent until the first real upload.
			std::ptr::write_bytes(self.layer_textures[slot].staging_ptr, 0, staging_size as usize);
		}
		self.layer_textures[slot].width = width;
		self.layer_textures[slot].height = height;
		self.layer_textures[slot].generation = None;
		self.layer_textures[slot].needs_init = true;
		Ok(())
	}

	fn destroy_overlay(&mut self, slot: usize) {
		let device = self.context.device();
		// SAFETY: callers only reach this after the last submission completed.
		unsafe {
			for view in &mut self.layer_textures[slot].views {
				if *view != vk::ImageView::null() {
					device.destroy_image_view(*view, None);
					*view = vk::ImageView::null();
				}
			}
			if self.layer_textures[slot].image != vk::Image::null() {
				device.destroy_image(self.layer_textures[slot].image, None);
				self.layer_textures[slot].image = vk::Image::null();
			}
			if self.layer_textures[slot].memory != vk::DeviceMemory::null() {
				device.free_memory(self.layer_textures[slot].memory, None);
				self.layer_textures[slot].memory = vk::DeviceMemory::null();
			}
			if self.layer_textures[slot].staging != vk::Buffer::null() {
				device.destroy_buffer(self.layer_textures[slot].staging, None);
				self.layer_textures[slot].staging = vk::Buffer::null();
			}
			if self.layer_textures[slot].staging_memory != vk::DeviceMemory::null() {
				device.free_memory(self.layer_textures[slot].staging_memory, None);
				self.layer_textures[slot].staging_memory = vk::DeviceMemory::null();
				self.layer_textures[slot].staging_ptr = std::ptr::null_mut();
			}
		}
	}

	/// View of `source`, created on first use and cached while the importer
	/// keeps the image alive.
	fn source_view(&mut self, source: &Arc<CachedImport>, format: vk::Format) -> Result<vk::ImageView, String> {
		if let Some((_, view)) = self.views.iter().find(|(import, _)| Arc::ptr_eq(import, source)) {
			return Ok(*view);
		}
		let device = self.context.device().clone();
		// Every previous submission has completed (each conversion waits), so
		// views of images the importer evicted can be destroyed now.
		self.views.retain(|(import, view)| {
			let keep = Arc::strong_count(import) > 1;
			if !keep {
				// SAFETY: no pending GPU work references the view.
				unsafe { device.destroy_image_view(*view, None) };
			}
			keep
		});
		if self.views.len() >= MAX_CACHED_VIEWS {
			let (_, view) = self.views.remove(0);
			// SAFETY: as above.
			unsafe { device.destroy_image_view(view, None) };
		}
		// SAFETY: the image is pinned by `source` for the lifetime of the view.
		let view = unsafe {
			device.create_image_view(
				&vk::ImageViewCreateInfo::default()
					.image(source.image())
					.view_type(vk::ImageViewType::TYPE_2D)
					.format(format)
					.subresource_range(color_range()),
				None,
			)
		}
		.map_err(|e| format!("source image view: {e}"))?;
		self.views.push((Arc::clone(source), view));
		Ok(view)
	}

	/// Stage `image` in `slot` when its content changed. Returns the view to
	/// sample and whether the staging buffer holds new content to upload.
	fn prepare_pixels(&mut self, slot: usize, image: &OverlayImage) -> Result<(vk::ImageView, bool), String> {
		self.ensure_overlay_capacity(slot, image.width, image.height)?;
		let texture = &mut self.layer_textures[slot];
		let upload = texture.generation != Some(image.generation);
		if upload {
			let row = image.width as usize * 4;
			let stride = texture.width as usize * 4;
			// SAFETY: the staging buffer holds texture.width x texture.height
			// texels, at least the image's extent; no GPU work reads it now.
			unsafe {
				for y in 0..image.height as usize {
					std::ptr::copy_nonoverlapping(
						image.pixels[y * row..].as_ptr(),
						texture.staging_ptr.add(y * stride),
						row,
					);
				}
			}
			texture.generation = Some(image.generation);
		}
		let view = match image.format {
			OverlayFormat::Rgba8 => texture.views[0],
			OverlayFormat::Bgra8 => texture.views[1],
		};
		Ok((view, upload))
	}

	/// Convert `source` into `target`, the encoder's input image, compositing
	/// `layers` bottom to top, and wait for completion. On success no GPU work
	/// reads the source or any imported layer any more. `target` is left in
	/// VIDEO_ENCODE_SRC layout.
	pub(crate) fn convert(
		&mut self,
		source: &Arc<CachedImport>,
		source_format: vk::Format,
		first_use: bool,
		layers: &Layers<'_>,
		target: vk::Image,
	) -> Result<ConvertOutcome, vk::Result> {
		let source_view = self
			.source_view(source, source_format)
			.map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
		let mut push = PushConstants {
			width: self.width,
			height: self.height,
			output_format: self.output_format_code,
			color_space: color_space_code(self.color_space),
			full_range: u32::from(self.full_range),
			sdr_white_nits: self.sdr_white_nits,
			_padding: [0; 2],
			layer_rect: [[0; 4]; MAX_OVERLAYS],
			layer_opacity: [0.0; MAX_OVERLAYS],
			layer_opaque: [0; MAX_OVERLAYS],
		};
		// Per slot: the view to bind, whether staged texels must be uploaded,
		// and an imported image with its first-use flag (needs acquire/release).
		let mut layer_views = [vk::ImageView::null(); MAX_OVERLAYS];
		let mut uploads = [false; MAX_OVERLAYS];
		let mut imported: [Option<(vk::Image, bool)>; MAX_OVERLAYS] = [None; MAX_OVERLAYS];
		for (slot, layer) in layers.iter().enumerate() {
			let Some(layer) = layer else {
				// Placeholder: never sampled while the rectangle width is zero.
				layer_views[slot] = self.layer_textures[slot].views[0];
				continue;
			};
			push.layer_rect[slot] = [layer.x, layer.y, layer.width as i32, layer.height as i32];
			push.layer_opacity[slot] = layer.opacity;
			match &layer.input {
				LayerInput::Pixels(image) => {
					let (view, upload) = self
						.prepare_pixels(slot, image)
						.map_err(|_| vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
					layer_views[slot] = view;
					uploads[slot] = upload;
				},
				LayerInput::Imported {
					import,
					format,
					first_use,
					opaque,
				} => {
					layer_views[slot] = self
						.source_view(import, *format)
						.map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
					imported[slot] = Some((import.image(), *first_use));
					push.layer_opaque[slot] = u32::from(*opaque);
				},
			}
		}
		let device = self.context.device().clone();
		let cmd = self.command_buffer;
		let source_image = source.image();

		// SAFETY: all handles belong to this device; the command buffer is not
		// pending (the previous submission was waited for); descriptor updates
		// happen while no submission uses the set.
		unsafe {
			let image_info = |view, layout| {
				vk::DescriptorImageInfo::default()
					.sampler(self.sampler)
					.image_view(view)
					.image_layout(layout)
			};
			// Imported layers are sampled in GENERAL (see the barrier below).
			let layer_layout = |slot: usize| {
				if imported[slot].is_some() {
					vk::ImageLayout::GENERAL
				} else {
					vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
				}
			};
			let image_infos = [
				image_info(source_view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
				image_info(layer_views[0], layer_layout(0)),
				image_info(layer_views[1], layer_layout(1)),
			];
			let writes = [(0u32, 0usize), (2, 1), (3, 2)].map(|(binding, info)| {
				vk::WriteDescriptorSet::default()
					.dst_set(self.descriptor_set)
					.dst_binding(binding)
					.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
					.image_info(std::slice::from_ref(&image_infos[info]))
			});
			device.update_descriptor_sets(&writes, &[]);

			device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
			device.begin_command_buffer(
				cmd,
				&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
			)?;
			if self.query_pool != vk::QueryPool::null() {
				device.cmd_reset_query_pool(cmd, self.query_pool, 0, 2);
				device.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, self.query_pool, 0);
			}

			// Same ownership protocol as Pixelforge's converter: the first use of
			// an imported (EXCLUSIVE) DMA-BUF acquires it from the external
			// owner; later uses start from the GENERAL layout left below. Imported
			// layers are client DMA-BUFs and follow the same protocol.
			let acquire = |image: vk::Image, first_use: bool| {
				vk::ImageMemoryBarrier::default()
					.old_layout(if first_use {
						vk::ImageLayout::UNDEFINED
					} else {
						vk::ImageLayout::GENERAL
					})
					.new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
					.src_queue_family_index(if first_use {
						vk::QUEUE_FAMILY_EXTERNAL
					} else {
						vk::QUEUE_FAMILY_IGNORED
					})
					.dst_queue_family_index(if first_use {
						self.queue_family
					} else {
						vk::QUEUE_FAMILY_IGNORED
					})
					.image(image)
					.subresource_range(color_range())
					.src_access_mask(if first_use {
						vk::AccessFlags::empty()
					} else {
						vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
					})
					.dst_access_mask(vk::AccessFlags::SHADER_READ)
			};
			// Layers are other clients' buffers (e.g. XWayland's). Any layout
			// transition after the first acquire can make the driver treat the
			// buffer as written, and implicit sync then waits for the owner's
			// own pending reads, which queue behind a GPU-bound game. Sample
			// them in GENERAL with a memory dependency only.
			let layer_barrier = |image: vk::Image, layer_first_use: bool| {
				let barrier = acquire(image, layer_first_use).new_layout(vk::ImageLayout::GENERAL);
				if layer_first_use {
					barrier
				} else {
					barrier.old_layout(vk::ImageLayout::GENERAL)
				}
			};
			let mut acquires = [acquire(source_image, first_use); 1 + MAX_OVERLAYS];
			let mut acquire_count = 1;
			for (image, layer_first_use) in imported.iter().flatten() {
				acquires[acquire_count] = layer_barrier(*image, *layer_first_use);
				acquire_count += 1;
			}
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::PipelineStageFlags::COMPUTE_SHADER,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&acquires[..acquire_count],
			);
			for (slot, &upload) in uploads.iter().enumerate() {
				let texture = &self.layer_textures[slot];
				let texture_barrier = |old, new, src, dst| {
					vk::ImageMemoryBarrier::default()
						.old_layout(old)
						.new_layout(new)
						.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.image(texture.image)
						.subresource_range(color_range())
						.src_access_mask(src)
						.dst_access_mask(dst)
				};
				if upload {
					let old = if texture.needs_init {
						vk::ImageLayout::UNDEFINED
					} else {
						vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
					};
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::PipelineStageFlags::TRANSFER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[texture_barrier(
							old,
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::AccessFlags::SHADER_READ,
							vk::AccessFlags::TRANSFER_WRITE,
						)],
					);
					let region = vk::BufferImageCopy {
						buffer_offset: 0,
						buffer_row_length: texture.width,
						buffer_image_height: 0,
						image_subresource: vk::ImageSubresourceLayers {
							aspect_mask: vk::ImageAspectFlags::COLOR,
							mip_level: 0,
							base_array_layer: 0,
							layer_count: 1,
						},
						image_offset: vk::Offset3D::default(),
						image_extent: vk::Extent3D {
							width: texture.width,
							height: texture.height,
							depth: 1,
						},
					};
					device.cmd_copy_buffer_to_image(
						cmd,
						texture.staging,
						texture.image,
						vk::ImageLayout::TRANSFER_DST_OPTIMAL,
						&[region],
					);
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TRANSFER,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[texture_barrier(
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
							vk::AccessFlags::TRANSFER_WRITE,
							vk::AccessFlags::SHADER_READ,
						)],
					);
				} else if texture.needs_init {
					// Placeholder contents are irrelevant; only the layout matters.
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TOP_OF_PIPE,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[texture_barrier(
							vk::ImageLayout::UNDEFINED,
							vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
							vk::AccessFlags::empty(),
							vk::AccessFlags::SHADER_READ,
						)],
					);
				}
				self.layer_textures[slot].needs_init = false;
			}

			device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
			device.cmd_bind_descriptor_sets(
				cmd,
				vk::PipelineBindPoint::COMPUTE,
				self.pipeline_layout,
				0,
				&[self.descriptor_set],
				&[],
			);
			device.cmd_push_constants(
				cmd,
				self.pipeline_layout,
				vk::ShaderStageFlags::COMPUTE,
				0,
				std::slice::from_raw_parts(
					(&push as *const PushConstants).cast::<u8>(),
					std::mem::size_of::<PushConstants>(),
				),
			);
			device.cmd_dispatch(
				cmd,
				self.width.div_ceil(GROUP_WIDTH),
				self.height.div_ceil(GROUP_HEIGHT),
				1,
			);

			// Shader writes -> copy; encoder input -> TRANSFER_DST. The previous
			// encode that read this slot completed before Pixelforge returned it.
			let to_transfer = [vk::ImageMemoryBarrier::default()
				.src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
				.dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
				.old_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
				.new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
				.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
				.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
				.image(target)
				.subresource_range(color_range())];
			let buffer_barrier = [vk::BufferMemoryBarrier::default()
				.src_access_mask(vk::AccessFlags::SHADER_WRITE)
				.dst_access_mask(vk::AccessFlags::TRANSFER_READ)
				.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
				.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
				.buffer(self.output_buffer)
				.size(vk::WHOLE_SIZE)];
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::COMPUTE_SHADER,
				vk::PipelineStageFlags::TRANSFER,
				vk::DependencyFlags::empty(),
				&[],
				&buffer_barrier,
				&to_transfer,
			);
			device.cmd_copy_buffer_to_image(
				cmd,
				self.output_buffer,
				target,
				vk::ImageLayout::TRANSFER_DST_OPTIMAL,
				&copy_regions(self.output_format, self.width, self.height),
			);
			// Sampled images return to GENERAL for their next use.
			let release = |image: vk::Image| {
				vk::ImageMemoryBarrier::default()
					.old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
					.new_layout(vk::ImageLayout::GENERAL)
					.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
					.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
					.image(image)
					.subresource_range(color_range())
					.src_access_mask(vk::AccessFlags::SHADER_READ)
					.dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
			};
			let post = [
				vk::ImageMemoryBarrier::default()
					.src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
					.dst_access_mask(vk::AccessFlags::empty())
					.old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
					.new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
					.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
					.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
					.image(target)
					.subresource_range(color_range()),
				release(source_image),
			];
			// Imported layers already are in GENERAL.
			let post_count = 2;
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
				vk::PipelineStageFlags::BOTTOM_OF_PIPE,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&post[..post_count],
			);
			if self.query_pool != vk::QueryPool::null() {
				device.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, self.query_pool, 1);
			}
			device.end_command_buffer(cmd)?;

			let command_buffers = [cmd];
			device.queue_submit(
				self.queue,
				&[vk::SubmitInfo::default().command_buffers(&command_buffers)],
				self.fence,
			)?;
			// Encode submission needs the converted input; Pixelforge's encode
			// API takes no wait semaphore, so completion is established here.
			let waited = device.wait_for_fences(&[self.fence], true, u64::MAX);
			device.reset_fences(&[self.fence])?;
			waited?;
		}

		let gpu_ns = (self.query_pool != vk::QueryPool::null())
			.then(|| {
				let mut stamps = [0u64; 2];
				// SAFETY: both queries were written by the completed submission.
				unsafe { device.get_query_pool_results(self.query_pool, 0, &mut stamps, vk::QueryResultFlags::TYPE_64) }
					.ok()
					.map(|()| (stamps[1].saturating_sub(stamps[0]) as f64 * self.timestamp_period_ns) as u64)
			})
			.flatten();
		Ok(ConvertOutcome { gpu_ns })
	}
}

impl Drop for InputConverter {
	fn drop(&mut self) {
		let device = self.context.device().clone();
		// SAFETY: every conversion waits for its fence, so no submission that
		// references these objects can still be pending. A failed wait leaves
		// the queue in an unknown state; idle the queue before destroying.
		unsafe {
			let _ = device.queue_wait_idle(self.queue);
			for (_, view) in self.views.drain(..) {
				device.destroy_image_view(view, None);
			}
			for slot in 0..MAX_OVERLAYS {
				self.destroy_overlay(slot);
			}
			if self.query_pool != vk::QueryPool::null() {
				device.destroy_query_pool(self.query_pool, None);
			}
			if self.fence != vk::Fence::null() {
				device.destroy_fence(self.fence, None);
			}
			if self.command_pool != vk::CommandPool::null() {
				device.destroy_command_pool(self.command_pool, None);
			}
			if self.output_buffer != vk::Buffer::null() {
				device.destroy_buffer(self.output_buffer, None);
			}
			if self.output_memory != vk::DeviceMemory::null() {
				device.free_memory(self.output_memory, None);
			}
			if self.descriptor_pool != vk::DescriptorPool::null() {
				device.destroy_descriptor_pool(self.descriptor_pool, None);
			}
			if self.sampler != vk::Sampler::null() {
				device.destroy_sampler(self.sampler, None);
			}
			if self.pipeline != vk::Pipeline::null() {
				device.destroy_pipeline(self.pipeline, None);
			}
			if self.pipeline_layout != vk::PipelineLayout::null() {
				device.destroy_pipeline_layout(self.pipeline_layout, None);
			}
			if self.set_layout != vk::DescriptorSetLayout::null() {
				device.destroy_descriptor_set_layout(self.set_layout, None);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn packed_layout_requires_word_aligned_rows() {
		assert!(supports_extent(1920, 1080));
		assert!(supports_extent(3840, 2160));
		assert!(supports_extent(2880, 1920));
		assert!(
			!supports_extent(1366, 768),
			"1366 bytes per NV12 row is not word aligned"
		);
		assert!(!supports_extent(1920, 1081));
		assert!(!supports_extent(0, 1080));
	}

	#[test]
	fn output_layout_matches_pixelforge() {
		for (w, h) in [(1920u32, 1080u32), (3840, 2160), (2560, 1440)] {
			for format in [
				OutputFormat::NV12,
				OutputFormat::P010,
				OutputFormat::YUV444,
				OutputFormat::YUV444P10,
			] {
				assert_eq!(output_size(format, w, h), format.output_size(w, h) as u64, "{format:?}");
			}
			let p = u64::from(w) * u64::from(h);
			assert_eq!(copy_regions(OutputFormat::NV12, w, h)[1].buffer_offset, p);
			assert_eq!(copy_regions(OutputFormat::P010, w, h)[1].buffer_offset, 2 * p);
			assert_eq!(copy_regions(OutputFormat::YUV444, w, h)[1].buffer_offset, p);
			assert_eq!(copy_regions(OutputFormat::YUV444P10, w, h)[1].buffer_offset, 2 * p);
		}
	}

	/// GPU fixture comparing this converter with Pixelforge's on identical
	/// random sources, and late cursor composition with a CPU-composited
	/// reference. Run on a GPU host:
	/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core packed_converter -- --ignored --nocapture`
	#[test]
	#[ignore = "needs a Vulkan Video GPU: MOONSHINE_TEST_GPU=1"]
	fn packed_converter_matches_pixelforge_on_gpu() {
		assert!(
			std::env::var_os("MOONSHINE_TEST_GPU").is_some(),
			"set MOONSHINE_TEST_GPU=1"
		);
		gpu_fixture::run();
	}

	#[test]
	fn embedded_shader_is_spirv() {
		let words = ash::util::read_spv(&mut std::io::Cursor::new(CONVERT_SPIRV)).unwrap();
		assert_eq!(words[0], 0x0723_0203);
	}
}

#[cfg(test)]
mod gpu_fixture {
	use super::*;
	use pixelforge::{
		Codec, ColorConverter, ColorConverterConfig, EncodeBitDepth, EncodeConfig, Encoder, InputFormat, PixelFormat,
		VideoContextBuilder,
	};

	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> u32 {
			self.0 = self
				.0
				.wrapping_mul(6364136223846793005)
				.wrapping_add(1442695040888963407);
			(self.0 >> 33) as u32
		}
	}

	fn host_buffer(
		context: &VideoContext,
		size: u64,
		usage: vk::BufferUsageFlags,
	) -> (vk::Buffer, vk::DeviceMemory, *mut u8) {
		let device = context.device();
		// SAFETY: test-owned objects on a live device.
		unsafe {
			let buffer = device
				.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), None)
				.unwrap();
			let req = device.get_buffer_memory_requirements(buffer);
			let ty = find_memory_type(
				context,
				req.memory_type_bits,
				vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
			)
			.unwrap();
			let memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(req.size)
						.memory_type_index(ty),
					None,
				)
				.unwrap();
			device.bind_buffer_memory(buffer, memory, 0).unwrap();
			let ptr = device
				.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
				.unwrap()
				.cast();
			(buffer, memory, ptr)
		}
	}

	/// One-shot command buffer on the graphics/compute queue.
	fn submit(context: &VideoContext, record: impl FnOnce(vk::CommandBuffer)) {
		let device = context.device();
		// SAFETY: test-owned objects; waits for completion before returning.
		unsafe {
			let pool = device
				.create_command_pool(
					&vk::CommandPoolCreateInfo::default().queue_family_index(context.compute_queue_family()),
					None,
				)
				.unwrap();
			let cmd = device
				.allocate_command_buffers(
					&vk::CommandBufferAllocateInfo::default()
						.command_pool(pool)
						.command_buffer_count(1),
				)
				.unwrap()[0];
			device
				.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
				.unwrap();
			record(cmd);
			device.end_command_buffer(cmd).unwrap();
			let cmds = [cmd];
			device
				.queue_submit(
					context.compute_queue(),
					&[vk::SubmitInfo::default().command_buffers(&cmds)],
					vk::Fence::null(),
				)
				.unwrap();
			device.queue_wait_idle(context.compute_queue()).unwrap();
			device.destroy_command_pool(pool, None);
		}
	}

	/// A sampled source image in GENERAL layout holding `bytes`.
	fn source_image(context: &VideoContext, w: u32, h: u32, format: vk::Format, bytes: &[u8]) -> Arc<CachedImport> {
		let device = context.device();
		// SAFETY: test-owned objects on a live device.
		let (image, memory) = unsafe {
			let image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(format)
						.extent(vk::Extent3D {
							width: w,
							height: h,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.unwrap();
			let req = device.get_image_memory_requirements(image);
			let ty = find_memory_type(context, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).unwrap();
			let memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(req.size)
						.memory_type_index(ty),
					None,
				)
				.unwrap();
			device.bind_image_memory(image, memory, 0).unwrap();
			(image, memory)
		};
		let (staging, staging_memory, ptr) =
			host_buffer(context, bytes.len() as u64, vk::BufferUsageFlags::TRANSFER_SRC);
		// SAFETY: the mapping holds bytes.len() bytes.
		unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
		submit(context, |cmd| unsafe {
			let barrier = |old, new| {
				vk::ImageMemoryBarrier::default()
					.old_layout(old)
					.new_layout(new)
					.src_access_mask(vk::AccessFlags::MEMORY_WRITE)
					.dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
					.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
					.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
					.image(image)
					.subresource_range(color_range())
			};
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[barrier(
					vk::ImageLayout::UNDEFINED,
					vk::ImageLayout::TRANSFER_DST_OPTIMAL,
				)],
			);
			device.cmd_copy_buffer_to_image(
				cmd,
				staging,
				image,
				vk::ImageLayout::TRANSFER_DST_OPTIMAL,
				&[vk::BufferImageCopy {
					image_subresource: vk::ImageSubresourceLayers {
						aspect_mask: vk::ImageAspectFlags::COLOR,
						mip_level: 0,
						base_array_layer: 0,
						layer_count: 1,
					},
					image_extent: vk::Extent3D {
						width: w,
						height: h,
						depth: 1,
					},
					..Default::default()
				}],
			);
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&[barrier(vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::GENERAL)],
			);
		});
		// SAFETY: the upload completed.
		unsafe {
			device.destroy_buffer(staging, None);
			device.free_memory(staging_memory, None);
		}
		Arc::new(CachedImport::for_test(context.clone(), image, memory, w, h, format))
	}

	fn read_buffer(context: &VideoContext, buffer: vk::Buffer, size: u64) -> Vec<u8> {
		let (dst, memory, ptr) = host_buffer(context, size, vk::BufferUsageFlags::TRANSFER_DST);
		submit(context, |cmd| unsafe {
			context.device().cmd_copy_buffer(
				cmd,
				buffer,
				dst,
				&[vk::BufferCopy {
					src_offset: 0,
					dst_offset: 0,
					size,
				}],
			);
		});
		// SAFETY: the copy completed; the mapping holds `size` bytes.
		let out = unsafe { std::slice::from_raw_parts(ptr, size as usize).to_vec() };
		unsafe {
			context.device().destroy_buffer(dst, None);
			context.device().free_memory(memory, None);
		}
		out
	}

	/// Pixelforge 0.9.1's 8-bit 4:4:4 path computes the V byte offset from the
	/// U byte index, so it ORs V into the U byte and leaves every V byte zero.
	/// Confirm exactly that signature against the correct packed output, then
	/// compare the rest normally (returns the reference with the defect undone).
	fn pixelforge_yuv444_defect(reference: &[u8], packed: &[u8], pixels: usize, failures: &mut Vec<String>) -> Vec<u8> {
		let mut repaired = reference.to_vec();
		let mut mismatches = 0usize;
		for p in 0..pixels {
			let (u, v) = (pixels + 2 * p, pixels + 2 * p + 1);
			if reference[v] != 0 || reference[u] != packed[u] | packed[v] {
				mismatches += 1;
			}
			repaired[u] = packed[u];
			repaired[v] = packed[v];
		}
		eprintln!("YUV444 reference defect signature mismatches: {mismatches}");
		if mismatches != 0 {
			failures.push(format!(
				"YUV444 reference differs beyond the known defect at {mismatches} pixels"
			));
		}
		repaired
	}

	/// (differing bytes, max absolute difference per sample).
	fn compare(a: &[u8], b: &[u8], ten_bit: bool) -> (usize, u32) {
		if ten_bit {
			let words = |v: &[u8]| {
				v.as_chunks::<2>()
					.0
					.iter()
					.map(|c| u16::from_le_bytes(*c) >> 6)
					.collect::<Vec<_>>()
			};
			let (a, b) = (words(a), words(b));
			let mut diff = 0;
			let mut max = 0;
			for (x, y) in a.iter().zip(&b) {
				if x != y {
					diff += 1;
					max = max.max(u32::from(x.abs_diff(*y)));
				}
			}
			(diff, max)
		} else {
			let mut diff = 0;
			let mut max = 0;
			for (x, y) in a.iter().zip(b) {
				if x != y {
					diff += 1;
					max = max.max(u32::from(x.abs_diff(*y)));
				}
			}
			(diff, max)
		}
	}

	fn half(v: f32) -> u16 {
		// Enough for [0, 4): round-to-nearest-even is not needed for a fixture.
		let bits = v.to_bits();
		let sign = (bits >> 16) & 0x8000;
		let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
		if v == 0.0 || exp <= 0 {
			return sign as u16;
		}
		(sign | ((exp as u32) << 10) | ((bits >> 13) & 0x3ff)) as u16
	}

	fn random_source(rng: &mut Rng, format: vk::Format, w: u32, h: u32) -> Vec<u8> {
		let n = (w * h) as usize;
		match format {
			vk::Format::R16G16B16A16_SFLOAT => {
				let mut out = Vec::with_capacity(n * 8);
				for _ in 0..n {
					for c in 0..4 {
						// scRGB-like range, including highlights above 1.0.
						let v = if c == 3 {
							1.0
						} else {
							(rng.next() % 4096) as f32 / 1024.0
						};
						out.extend_from_slice(&half(v).to_le_bytes());
					}
				}
				out
			},
			_ => (0..n * 4).map(|_| rng.next() as u8).collect(),
		}
	}

	/// A non-video planar image for formats the encoder cannot use here.
	/// Dropping it frees the image.
	struct PlainTarget(vk::Image, vk::DeviceMemory, VideoContext);
	impl Drop for PlainTarget {
		fn drop(&mut self) {
			// SAFETY: every conversion targeting it completed.
			unsafe {
				self.2.device().destroy_image(self.0, None);
				self.2.device().free_memory(self.1, None);
			}
		}
	}
	fn plain_planar_target(context: &VideoContext, w: u32, h: u32, format: vk::Format) -> PlainTarget {
		let device = context.device();
		// SAFETY: test-owned objects. Converters transition it from the video
		// encode layout like an encoder input, which RADV accepts for fixtures.
		unsafe {
			let image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(format)
						.extent(vk::Extent3D {
							width: w,
							height: h,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(vk::ImageUsageFlags::TRANSFER_DST)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.unwrap();
			let req = device.get_image_memory_requirements(image);
			let ty = find_memory_type(context, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).unwrap();
			let memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(req.size)
						.memory_type_index(ty),
					None,
				)
				.unwrap();
			device.bind_image_memory(image, memory, 0).unwrap();
			PlainTarget(image, memory, context.clone())
		}
	}

	pub(super) fn run() {
		let context = VideoContextBuilder::new()
			.app_name("convert-fixture")
			.require_encode(Codec::H265)
			.build()
			.expect("Vulkan Video context");
		let (w, h) = (1920u32, 1080u32);
		let mut rng = Rng(0x5eed);
		let mut failures = Vec::new();
		let mut checked = 0;
		for (pixel_format, depth, output_format) in [
			(PixelFormat::Yuv420, EncodeBitDepth::Eight, OutputFormat::NV12),
			(PixelFormat::Yuv420, EncodeBitDepth::Ten, OutputFormat::P010),
			(PixelFormat::Yuv444, EncodeBitDepth::Eight, OutputFormat::YUV444),
			(PixelFormat::Yuv444, EncodeBitDepth::Ten, OutputFormat::YUV444P10),
		] {
			let config = EncodeConfig::h265(w, h)
				.with_pixel_format(pixel_format)
				.with_bit_depth(depth);
			// Without encoder support (e.g. 4:4:4 on RADV) both converters still
			// write their packed buffers; a plain planar image receives the copy.
			let encoder = Encoder::new(context.clone(), config).ok();
			let _plain_target;
			let target = match &encoder {
				Some(encoder) => encoder.input_image(),
				None => {
					eprintln!("{output_format:?}: no encoder; comparing against a plain planar target");
					_plain_target = plain_planar_target(&context, w, h, output_format.vulkan_format());
					_plain_target.0
				},
			};
			let ten = output_format.is_10bit();
			for (input_format, vk_format) in [
				(InputFormat::RGBA, vk::Format::R8G8B8A8_UNORM),
				(InputFormat::BGRx, vk::Format::B8G8R8A8_UNORM),
				(InputFormat::ABGR2101010, vk::Format::A2B10G10R10_UNORM_PACK32),
				(InputFormat::RGBA16F, vk::Format::R16G16B16A16_SFLOAT),
			] {
				let bytes = random_source(&mut rng, vk_format, w, h);
				let source = source_image(&context, w, h, vk_format, &bytes);
				for color_space in [
					ColorSpace::Bt709,
					ColorSpace::Bt2020,
					ColorSpace::SrgbToBt2020Pq,
					ColorSpace::Bt709LinearToBt2020Pq,
				] {
					for full_range in [false, true] {
						let mut reference_config = ColorConverterConfig::new(w, h, input_format, output_format);
						reference_config.color_space = color_space;
						reference_config.full_range = full_range;
						let mut reference = ColorConverter::new(context.clone(), reference_config).unwrap();
						reference
							.convert(source.image(), vk::ImageLayout::GENERAL, target)
							.unwrap();
						let size = output_size(output_format, w, h);
						let expected = read_buffer(&context, reference.output_buffer(), size);

						let mut packed = InputConverter::new(
							context.clone(),
							ConversionQueueMode::Auto,
							w,
							h,
							output_format,
							color_space,
							full_range,
						)
						.unwrap();
						packed
							.convert(&source, vk_format, false, &[None, None], target)
							.unwrap();
						let actual = read_buffer(&context, packed.output_buffer, size);
						let expected = if output_format == OutputFormat::YUV444 {
							pixelforge_yuv444_defect(&expected, &actual, (w * h) as usize, &mut failures)
						} else {
							expected
						};
						let (diff, max) = compare(&expected, &actual, ten);
						checked += 1;
						let line = format!(
							"{output_format:?} {input_format:?} {color_space:?} full={full_range}: {diff} differing samples, max {max}"
						);
						eprintln!("{line}");
						// Shader compilers may contract differently; allow rare 1-code
						// truncation-boundary flips, never larger or systematic errors.
						if max > 1 || diff * 10_000 > expected.len() {
							failures.push(line);
						}
					}
				}
			}
		}
		assert!(checked > 0, "no format could be checked");
		timing(&context);
		overlay_matches_cpu_composition(&context, &mut failures);
		assert!(failures.is_empty(), "converter mismatches: {failures:#?}");
	}

	/// Informational: wall time per synchronous conversion for both converters
	/// (interleaved, same clocks), plus the packed converter's GPU timestamps.
	fn timing(context: &VideoContext) {
		let (w, h) = (3840u32, 2160u32);
		let mut rng = Rng(7);
		let encoder = Encoder::new(context.clone(), EncodeConfig::h265(w, h)).unwrap();
		let target = encoder.input_image();
		let bytes = random_source(&mut rng, vk::Format::R8G8B8A8_UNORM, w, h);
		let source = source_image(context, w, h, vk::Format::R8G8B8A8_UNORM, &bytes);
		let mut reference = ColorConverter::new(
			context.clone(),
			ColorConverterConfig::new(w, h, InputFormat::RGBA, OutputFormat::NV12),
		)
		.unwrap();
		for mode in [ConversionQueueMode::Graphics, ConversionQueueMode::Auto] {
			let mut packed = InputConverter::new(
				context.clone(),
				mode,
				w,
				h,
				OutputFormat::NV12,
				ColorSpace::Bt709,
				false,
			)
			.unwrap();
			let (mut reference_ns, mut packed_ns, mut gpu_ns) = (0u128, 0u128, 0u64);
			const N: u32 = 200;
			for _ in 0..N {
				let t = std::time::Instant::now();
				reference
					.convert(source.image(), vk::ImageLayout::GENERAL, target)
					.unwrap();
				reference_ns += t.elapsed().as_nanos();
				let t = std::time::Instant::now();
				gpu_ns += packed
					.convert(&source, vk::Format::R8G8B8A8_UNORM, false, &[None, None], target)
					.unwrap()
					.gpu_ns
					.unwrap_or(0);
				packed_ns += t.elapsed().as_nanos();
			}
			eprintln!(
				"4K NV12 timing ({mode:?}): pixelforge {:.1} us/frame wall, packed {:.1} us wall, {:.1} us GPU",
				reference_ns as f64 / f64::from(N) / 1e3,
				packed_ns as f64 / f64::from(N) / 1e3,
				gpu_ns as f64 / f64::from(N) / 1e3
			);
		}
	}

	/// Late composition must equal GLES-style premultiplied source-over
	/// blending in the frame's encoding followed by the same conversion: a
	/// notification layer read from an imported image (X format, so its alpha
	/// bytes are ignored) at 75% window opacity, partly off the right edge,
	/// then a cursor with every alpha level, partly off the left edge.
	fn overlay_matches_cpu_composition(context: &VideoContext, failures: &mut Vec<String>) {
		let (w, h) = (256u32, 128u32);
		let mut rng = Rng(42);
		let config = EncodeConfig::h265(w, h);
		let encoder = Encoder::new(context.clone(), config).unwrap();
		let target = encoder.input_image();
		let source_bytes = random_source(&mut rng, vk::Format::R8G8B8A8_UNORM, w, h);
		let (cw, ch, cx, cy) = (32u32, 24u32, -5i32, 37i32);
		let mut cursor = vec![0u8; (cw * ch * 4) as usize];
		for (i, texel) in cursor.as_chunks_mut::<4>().0.iter_mut().enumerate() {
			let a = ((i * 7) % 256) as u8;
			for channel in &mut texel[..3] {
				*channel = (rng.next() % (u32::from(a) + 1)) as u8;
			}
			texel[3] = a;
		}
		// BGRX texels with garbage in the X byte.
		let (nw, nh, nx, ny, opacity) = (120u32, 60u32, 180i32, 30i32, 0.75f32);
		let notification: Vec<u8> = (0..nw * nh * 4).map(|_| rng.next() as u8).collect();

		// Reference: blend each layer into an 8-bit buffer, as GLES draws into
		// an 8-bit target, then convert with Pixelforge.
		let blend =
			|dst: &mut [u8], (ox, oy): (i32, i32), (ow, oh): (u32, u32), texel: &dyn Fn(u32, u32) -> [f32; 4]| {
				for y in 0..oh as i32 {
					for x in 0..ow as i32 {
						let (px, py) = (ox + x, oy + y);
						if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
							continue;
						}
						let o = texel(x as u32, y as u32);
						let d = &mut dst[((py as u32 * w + px as u32) * 4) as usize..][..4];
						for c in 0..3 {
							let v = o[c] + f32::from(d[c]) / 255.0 * (1.0 - o[3]);
							d[c] = (v * 255.0).round().clamp(0.0, 255.0) as u8;
						}
					}
				}
			};
		let mut composited = source_bytes.clone();
		blend(&mut composited, (nx, ny), (nw, nh), &|x, y| {
			let t = &notification[((y * nw + x) * 4) as usize..][..4];
			// BGRA bytes -> RGB, alpha forced to one, times window opacity.
			[
				f32::from(t[2]) / 255.0 * opacity,
				f32::from(t[1]) / 255.0 * opacity,
				f32::from(t[0]) / 255.0 * opacity,
				opacity,
			]
		});
		blend(&mut composited, (cx, cy), (cw, ch), &|x, y| {
			let t = &cursor[((y * cw + x) * 4) as usize..][..4];
			[
				f32::from(t[0]) / 255.0,
				f32::from(t[1]) / 255.0,
				f32::from(t[2]) / 255.0,
				f32::from(t[3]) / 255.0,
			]
		});
		let plain = source_image(context, w, h, vk::Format::R8G8B8A8_UNORM, &source_bytes);
		let notification_image = source_image(context, nw, nh, vk::Format::B8G8R8A8_UNORM, &notification);
		let reference_source = source_image(context, w, h, vk::Format::R8G8B8A8_UNORM, &composited);
		let mut reference = ColorConverter::new(
			context.clone(),
			ColorConverterConfig::new(w, h, InputFormat::RGBA, OutputFormat::NV12),
		)
		.unwrap();
		reference
			.convert(reference_source.image(), vk::ImageLayout::GENERAL, target)
			.unwrap();
		let size = output_size(OutputFormat::NV12, w, h);
		let expected = read_buffer(context, reference.output_buffer(), size);
		let mut packed = InputConverter::new(
			context.clone(),
			ConversionQueueMode::Auto,
			w,
			h,
			OutputFormat::NV12,
			ColorSpace::Bt709,
			false,
		)
		.unwrap();
		let cursor_image = OverlayImage {
			generation: 1,
			width: cw,
			height: ch,
			format: OverlayFormat::Rgba8,
			pixels: cursor.into_boxed_slice(),
		};
		let layers: Layers<'_> = [
			Some(Layer {
				input: LayerInput::Imported {
					import: &notification_image,
					format: vk::Format::B8G8R8A8_UNORM,
					first_use: false,
					opaque: true,
				},
				x: nx,
				y: ny,
				width: nw,
				height: nh,
				opacity,
			}),
			Some(Layer {
				input: LayerInput::Pixels(&cursor_image),
				x: cx,
				y: cy,
				width: cw,
				height: ch,
				opacity: 1.0,
			}),
		];
		packed
			.convert(&plain, vk::Format::R8G8B8A8_UNORM, false, &layers, target)
			.unwrap();
		let actual = read_buffer(context, packed.output_buffer, size);
		let (diff, max) = compare(&expected, &actual, false);
		eprintln!("notification + cursor layers NV12: {diff} differing samples, max {max}");
		// The reference rounds to 8 bits after each layer (as an 8-bit GLES
		// target does); the late path blends in float. Overlapping layers can
		// accumulate one code of rounding each.
		if max > 2 {
			failures.push(format!("layered NV12 max difference {max}"));
		}
		// Without layers the same converter must not draw any.
		packed
			.convert(&plain, vk::Format::R8G8B8A8_UNORM, false, &[None, None], target)
			.unwrap();
		let without = read_buffer(context, packed.output_buffer, size);
		let mut plain_reference = ColorConverter::new(
			context.clone(),
			ColorConverterConfig::new(w, h, InputFormat::RGBA, OutputFormat::NV12),
		)
		.unwrap();
		plain_reference
			.convert(plain.image(), vk::ImageLayout::GENERAL, target)
			.unwrap();
		let plain_expected = read_buffer(context, plain_reference.output_buffer(), size);
		let (diff, max) = compare(&plain_expected, &without, false);
		eprintln!("layers removed NV12: {diff} differing samples, max {max}");
		if max > 1 {
			failures.push(format!("layer removal max difference {max}"));
		}
	}
}
