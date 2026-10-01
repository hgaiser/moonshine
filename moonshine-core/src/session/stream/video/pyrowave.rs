//! Safe, deliberately small wrapper around the PyroWave 0.7 C API.
//!
//! The shared library is loaded at runtime. This keeps the conventional Vulkan
//! Video codecs usable on systems where PyroWave is not installed, while the
//! package definition can still provide and pin the authoritative fork.

use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd, RawFd};
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ash::vk;
use libloading::Library;
use pixelforge::VideoContext;

use super::format::{
	BitDepth, ChromaFormat, ColorPrimaries, ColorRange, MatrixCoefficients, NegotiatedVideoFormat, TransferFunction,
};
use super::pipeline::dmabuf::{ImportCacheStats, same_open_file};
use crate::session::compositor::frame::{ExportedFrame, FrameColorSpace};
use serde::{Deserialize, Serialize};

pub(crate) const SOURCE_URL: &str = "https://github.com/karsyboy/pyrowave";
pub(crate) const SOURCE_REVISION: &str = "e344479d6c0439e346c788a918ad5645713f7573";
/// Wire-v1 clients cap a reassembled PyroWave frame at 3 MiB.
const PYROWAVE_MAX_FRAME_BYTES: usize = 3 * 1024 * 1024;
const QUALITY_REFERENCE_4K_420_SDR_BYTES: u64 = 400_000;

/// Quality-oriented tuning seed shared with the client default model. This is
/// diagnostic guidance, not a codec limit or a replacement for manual bitrate.
pub(crate) fn quality_reference_frame_bytes(
	width: u32,
	height: u32,
	chroma: ChromaFormat,
	bit_depth: BitDepth,
) -> usize {
	let mut bytes = QUALITY_REFERENCE_4K_420_SDR_BYTES
		.saturating_mul(u64::from(width))
		.saturating_mul(u64::from(height))
		/ (3840 * 2160);
	if chroma == ChromaFormat::Yuv444 {
		// PyroWave's checked-in objective/subjective evaluation measured a
		// roughly 15-20% 4:4:4 penalty at equal luminance quality.
		bytes = bytes.saturating_mul(6) / 5;
	}
	if bit_depth == BitDepth::Ten {
		// The codec's preliminary HDR evaluation uses about 20% headroom. This
		// remains a tuning seed rather than a universal compression ratio.
		bytes = bytes.saturating_mul(6) / 5;
	}
	usize::try_from(bytes).unwrap_or(usize::MAX) & !3
}
const API_VERSION: (u32, u32, u32) = (0, 7, 0);

type ResultCode = i32;
const SUCCESS: ResultCode = 0;

fn wire_v1_frame_budget(bitrate: usize, fps: u32) -> Option<usize> {
	let divisor = (fps as usize).checked_mul(8)?;
	let bytes = bitrate.checked_div(divisor)? & !3;
	(1024..=PYROWAVE_MAX_FRAME_BYTES - 8).contains(&bytes).then_some(bytes)
}

type DeviceHandle = *mut c_void;
type EncoderHandle = *mut c_void;
type ImageHandle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Uuid {
	uuid: [u8; vk::UUID_SIZE],
}

#[repr(C)]
struct EncoderCreateInfo {
	device: DeviceHandle,
	width: i32,
	height: i32,
	chroma: i32,
}

#[repr(C)]
struct ImageCreateInfo {
	device: DeviceHandle,
	external_handle: usize,
	handle_type: vk::ExternalMemoryHandleTypeFlags,
	image_create_info: *const vk::ImageCreateInfo<'static>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ImageView {
	image: vk::Image,
	width: u32,
	height: u32,
	image_format: vk::Format,
	view_format: vk::Format,
	mip_level: u32,
	layer: u32,
	aspect: vk::ImageAspectFlags,
	swizzle: vk::ComponentSwizzle,
	layout: vk::ImageLayout,
}

#[repr(C)]
struct ScaledEncodeInfo {
	view: ImageView,
	input_color_space: vk::ColorSpaceKHR,
	output_color_space: vk::ColorSpaceKHR,
	intermediate_plane_format: vk::Format,
	ycbcr_chroma_midpoint: f32,
	force_linear_filtering: bool,
	skip_dither: bool,
	crop_rect: *const vk::Rect2D,
}

#[repr(C)]
struct RateControl {
	maximum_bitstream_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Packet {
	offset: usize,
	size: usize,
}

#[repr(C)]
struct ColorMetadata {
	color_primaries: u32,
	transfer_function: u32,
	ycbcr_transform: u32,
	ycbcr_range: u32,
	chroma_siting: u32,
}

/// Auto prefers graphics at normal priority; compute is an explicit opt-in.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PyroWaveQueueMode {
	#[default]
	Auto,
	Graphics,
	Compute,
}
impl PyroWaveQueueMode {
	fn flags(self) -> vk::QueueFlags {
		if self == Self::Compute {
			vk::QueueFlags::COMPUTE
		} else {
			vk::QueueFlags::GRAPHICS
		}
	}
}
// Compatibility creation starts on graphics. Failure of an optional queue
// preference leaves that safe default usable, including fallback API failure.
fn select_queue(
	mode: PyroWaveQueueMode,
	mut select: impl FnMut(vk::QueueFlags) -> ResultCode,
) -> (vk::QueueFlags, ResultCode) {
	let requested = mode.flags();
	let result = select(requested);
	if result == SUCCESS {
		(requested, result)
	} else {
		let _ = select(vk::QueueFlags::GRAPHICS);
		(vk::QueueFlags::GRAPHICS, result)
	}
}
type SetQueueType = unsafe extern "C" fn(DeviceHandle, vk::QueueFlags) -> ResultCode;
type ReportPerformance =
	unsafe extern "C" fn(DeviceHandle, unsafe extern "C" fn(*mut c_void, *const std::ffi::c_char), *mut c_void, bool);

/// Exclusive codec stages reported by the pinned C API. Unknown/CPU/heap
/// records are ignored. Timestamp values describe execution intervals, while
/// encode_wait measures submission-to-readback latency, including queue wait.
#[derive(Default, Debug)]
struct GpuTimings {
	stages: [Option<f64>; 6],
}
impl GpuTimings {
	fn record(&mut self, message: &str) {
		let Some((tag, value)) = message.split_once(": ") else {
			return;
		};
		let Some(index) = ["DWT", "Quant", "Analyze", "Resolve", "Packing", "scale"]
			.iter()
			.position(|t| *t == tag)
		else {
			return;
		};
		let Some(ms) = value
			.strip_suffix(" ms per frame")
			.and_then(|v| v.parse::<f64>().ok())
			.filter(|v| v.is_finite() && *v >= 0.0)
		else {
			return;
		};
		self.stages[index] = Some(ms);
	}
	fn total(&self) -> Option<f64> {
		self.stages.iter().copied().sum()
	}
}
unsafe extern "C" fn performance_callback(userdata: *mut c_void, message: *const std::ffi::c_char) {
	// SAFETY: report_performance_stats invokes this callback synchronously with
	// our stack-owned GpuTimings and a NUL-terminated temporary C string.
	if userdata.is_null() || message.is_null() {
		return;
	}
	let stats = unsafe { &mut *userdata.cast::<GpuTimings>() };
	if let Ok(message) = unsafe { CStr::from_ptr(message) }.to_str() {
		stats.record(message);
	}
}

type GetApiVersion = unsafe extern "C" fn(*mut u32, *mut u32, *mut u32);
type CreateDeviceByCompat =
	unsafe extern "C" fn(u32, u32, *const Uuid, *const Uuid, *const c_void, *mut DeviceHandle) -> ResultCode;
type ConfirmInterop = unsafe extern "C" fn(DeviceHandle) -> bool;
type DestroyDevice = unsafe extern "C" fn(DeviceHandle);
type CreateEncoder = unsafe extern "C" fn(*const EncoderCreateInfo, *mut EncoderHandle) -> ResultCode;
type DestroyEncoder = unsafe extern "C" fn(EncoderHandle);
type SetColorMetadata = unsafe extern "C" fn(EncoderHandle, *const ColorMetadata) -> ResultCode;
type CreateImage = unsafe extern "C" fn(*const ImageCreateInfo, *mut ImageHandle) -> ResultCode;
type DestroyImage = unsafe extern "C" fn(ImageHandle);
type GetImageView =
	unsafe extern "C" fn(ImageHandle, vk::ImageAspectFlags, vk::ImageUsageFlags, *mut ImageView) -> ResultCode;
type EncodeScaled = unsafe extern "C" fn(
	EncoderHandle,
	*const c_void,
	*const c_void,
	*const ScaledEncodeInfo,
	*const RateControl,
) -> ResultCode;
type ComputeNumPackets = unsafe extern "C" fn(EncoderHandle, usize, *mut usize) -> ResultCode;
type Packetize = unsafe extern "C" fn(EncoderHandle, *mut Packet, usize, *mut usize, *mut c_void, usize) -> ResultCode;

struct Api {
	// Must outlive every copied symbol pointer below.
	_library: Library,
	create_device_by_compat: CreateDeviceByCompat,
	confirm_interop: ConfirmInterop,
	set_queue_type: SetQueueType,
	report_performance: ReportPerformance,
	destroy_device: DestroyDevice,
	create_encoder: CreateEncoder,
	destroy_encoder: DestroyEncoder,
	set_color_metadata: SetColorMetadata,
	create_image: CreateImage,
	destroy_image: DestroyImage,
	get_image_view: GetImageView,
	encode_scaled: EncodeScaled,
	compute_num_packets: ComputeNumPackets,
	packetize: Packetize,
}

impl Api {
	fn load() -> Result<Rc<Self>, String> {
		let candidates: Vec<PathBuf> = match std::env::var_os("MOONSHINE_PYROWAVE_LIBRARY") {
			Some(path) => vec![path.into()],
			None => vec!["libpyrowave-shared.so.0".into(), "libpyrowave-shared.so".into()],
		};
		let mut errors = Vec::new();
		for candidate in candidates {
			// SAFETY: loading arbitrary paths is avoided unless the administrator
			// explicitly sets the override. Every symbol and the ABI version are
			// checked before the resulting safe wrapper is returned.
			let library = match unsafe { Library::new(&candidate) } {
				Ok(library) => library,
				Err(error) => {
					errors.push(format!("{}: {error}", candidate.display()));
					continue;
				},
			};
			// SAFETY: symbol types mirror pyrowave.h at the pinned 0.7.0 revision.
			unsafe {
				let version = *library
					.get::<GetApiVersion>(b"pyrowave_get_api_version\0")
					.map_err(|e| format!("{}: missing version symbol: {e}", candidate.display()))?;
				let mut major = 0;
				let mut minor = 0;
				let mut patch = 0;
				version(&mut major, &mut minor, &mut patch);
				if (major, minor, patch) != API_VERSION {
					return Err(format!(
						"{} has PyroWave API {major}.{minor}.{patch}; exactly {}.{}.{} is required",
						candidate.display(),
						API_VERSION.0,
						API_VERSION.1,
						API_VERSION.2
					));
				}

				macro_rules! symbol {
					($name:literal, $ty:ty) => {
						*library
							.get::<$ty>(concat!($name, "\0").as_bytes())
							.map_err(|e| format!("{}: missing {}: {e}", candidate.display(), $name))?
					};
				}
				let api = Self {
					create_device_by_compat: symbol!("pyrowave_create_device_by_compat", CreateDeviceByCompat),
					confirm_interop: symbol!("pyrowave_device_confirm_interop_support", ConfirmInterop),
					set_queue_type: symbol!("pyrowave_device_set_queue_type", SetQueueType),
					report_performance: symbol!("pyrowave_device_report_performance_stats", ReportPerformance),
					destroy_device: symbol!("pyrowave_device_destroy", DestroyDevice),
					create_encoder: symbol!("pyrowave_encoder_create", CreateEncoder),
					destroy_encoder: symbol!("pyrowave_encoder_destroy", DestroyEncoder),
					set_color_metadata: symbol!("pyrowave_encoder_set_color_metadata", SetColorMetadata),
					create_image: symbol!("pyrowave_image_create", CreateImage),
					destroy_image: symbol!("pyrowave_image_destroy", DestroyImage),
					get_image_view: symbol!("pyrowave_image_get_image_view", GetImageView),
					encode_scaled: symbol!("pyrowave_encoder_encode_gpu_scaled_synchronous", EncodeScaled),
					compute_num_packets: symbol!("pyrowave_encoder_compute_num_packets", ComputeNumPackets),
					packetize: symbol!("pyrowave_encoder_packetize", Packetize),
					_library: library,
				};
				return Ok(Rc::new(api));
			}
		}
		Err(format!("PyroWave shared library was not found ({})", errors.join("; ")))
	}
}

fn check(code: ResultCode, operation: &str) -> Result<(), String> {
	if code == SUCCESS {
		Ok(())
	} else {
		Err(format!("{operation} failed with PyroWave result {code}"))
	}
}

struct Device {
	api: Rc<Api>,
	handle: DeviceHandle,
	name: String,
}

impl Device {
	fn for_video_context(
		api: Rc<Api>,
		context: &VideoContext,
		mode: PyroWaveQueueMode,
		log_selection: bool,
	) -> Result<Rc<Self>, String> {
		let mut ids = vk::PhysicalDeviceIDProperties::default();
		let mut properties = vk::PhysicalDeviceProperties2 {
			p_next: (&mut ids as *mut vk::PhysicalDeviceIDProperties<'_>).cast(),
			..Default::default()
		};
		// SAFETY: handles originate from the live VideoContext and output structs
		// remain valid for the duration of the call.
		unsafe {
			context
				.instance()
				.get_physical_device_properties2(context.physical_device(), &mut properties);
		}
		let device_name = unsafe { CStr::from_ptr(properties.properties.device_name.as_ptr()) }
			.to_string_lossy()
			.into_owned();
		if properties.properties.device_type == vk::PhysicalDeviceType::CPU {
			return Err(format!(
				"PyroWave refuses software Vulkan device '{device_name}'; a hardware Vulkan GPU is required"
			));
		}
		let device_uuid = Uuid { uuid: ids.device_uuid };
		let driver_uuid = Uuid { uuid: ids.driver_uuid };
		let mut handle = ptr::null_mut();
		// SAFETY: all pointers reference initialized values for the call, and the
		// returned owned handle is destroyed by Drop.
		check(
			unsafe {
				(api.create_device_by_compat)(
					properties.properties.vendor_id,
					properties.properties.device_id,
					&device_uuid,
					&driver_uuid,
					ptr::null(),
					&mut handle,
				)
			},
			"matching the PyroWave Vulkan device",
		)?;
		if handle.is_null() {
			return Err("PyroWave returned a null device".to_string());
		}
		// SAFETY: `handle` is a valid live PyroWave device.
		if !unsafe { (api.confirm_interop)(handle) } {
			// SAFETY: no child objects exist yet.
			unsafe { (api.destroy_device)(handle) };
			return Err("PyroWave device does not support DMA-BUF interoperability".to_string());
		}
		// Keep automatic selection on graphics to avoid observed compute-queue
		// overhead and instability under game load. Explicit compute requests let
		// Granite select a compute family, with library fallback to graphics.
		// The entire PyroWave pipeline uses one queue.
		let (selected, result) = select_queue(mode, |flags| unsafe { (api.set_queue_type)(handle, flags) });
		if log_selection {
			let families = unsafe {
				context
					.instance()
					.get_physical_device_queue_family_properties(context.physical_device())
			};
			let dedicated = families.iter().any(|q| {
				q.queue_count != 0
					&& q.queue_flags.contains(vk::QueueFlags::COMPUTE)
					&& !q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
			});
			tracing::info!(requested_mode = ?mode, queue_preference = ?selected, dedicated_compute_available = dedicated, selection_result = result, global_priority = "medium", "PyroWave queue selection (library falls back to graphics when compute is unavailable)");
		}
		Ok(Rc::new(Self {
			api,
			handle,
			name: device_name,
		}))
	}
}

impl Drop for Device {
	fn drop(&mut self) {
		// SAFETY: children retain Rc<Device>, so this runs after all children.
		unsafe { (self.api.destroy_device)(self.handle) };
	}
}

struct ImportedImage {
	device: Rc<Device>,
	handle: ImageHandle,
	identity_fd: std::os::fd::OwnedFd,
	width: u32,
	height: u32,
	format: vk::Format,
	modifier: u64,
	layouts: Vec<(u32, u32)>,
}

impl ImportedImage {
	fn matches(&self, frame: &ExportedFrame, format: vk::Format) -> bool {
		self.width == frame.width
			&& self.height == frame.height
			&& self.format == format
			&& self.modifier == frame.modifier
			&& self
				.layouts
				.iter()
				.copied()
				.eq(frame.planes.iter().map(|p| (p.offset, p.stride)))
			&& same_open_file(self.identity_fd.as_raw_fd(), frame.planes[0].fd)
	}
}

impl Drop for ImportedImage {
	fn drop(&mut self) {
		// SAFETY: the image handle is owned by this wrapper and the encoder call
		// which referenced it has completed before cache eviction can occur.
		unsafe { (self.device.api.destroy_image)(self.handle) };
	}
}

/// PyroWave waits for each encode before the next import, so expired images
/// can be released immediately. Keep active swapchain buffers warm, but do not
/// pin every fd ever seen across buffer churn or swapchain recreation.
struct ImageCache<T> {
	entries: HashMap<RawFd, (T, Instant)>,
	calls_since_sweep: u32,
	stats: ImportCacheStats,
}

impl<T> ImageCache<T> {
	const TTL: Duration = Duration::from_secs(2);
	const SWEEP_INTERVAL: u32 = 60;

	fn new() -> Self {
		Self {
			entries: HashMap::new(),
			calls_since_sweep: 0,
			stats: ImportCacheStats::default(),
		}
	}

	fn get(&mut self, fd: RawFd, now: Instant) -> Option<&T> {
		self.calls_since_sweep += 1;
		if self.calls_since_sweep >= Self::SWEEP_INTERVAL {
			self.calls_since_sweep = 0;
			let before = self.entries.len();
			self.entries
				.retain(|_, (_, used)| now.saturating_duration_since(*used) <= Self::TTL);
			self.stats.evict += (before - self.entries.len()) as u64;
		}
		self.entries.get_mut(&fd).map(|(image, used)| {
			*used = now;
			&*image
		})
	}

	fn insert(&mut self, fd: RawFd, image: T, now: Instant) {
		self.entries.insert(fd, (image, now));
	}
}

pub(crate) struct EncodedFrame {
	pub data: Vec<u8>,
	pub data_size: usize,
	pub import: std::time::Duration,
	pub submit: std::time::Duration,
	pub encode_wait: std::time::Duration,
}

pub(crate) struct PyroWaveEncoder {
	device: Rc<Device>,
	handle: EncoderHandle,
	format: NegotiatedVideoFormat,
	visible_width: u32,
	visible_height: u32,
	maximum_frame_bytes: usize,
	images: ImageCache<ImportedImage>,
	consecutive_near_limit_frames: u32,
	last_limit_warning: Option<std::time::Instant>,
	scaling_logged: bool,
}

impl PyroWaveEncoder {
	pub(crate) fn is_available(
		context: &VideoContext,
		chroma: ChromaFormat,
		bit_depth: BitDepth,
	) -> Result<(), String> {
		let intermediate = match bit_depth {
			BitDepth::Eight => vk::Format::R8_UNORM,
			BitDepth::Ten => vk::Format::R16_UNORM,
		};
		// SAFETY: the VideoContext owns both handles for the duration of the call.
		let properties = unsafe {
			context
				.instance()
				.get_physical_device_format_properties(context.physical_device(), intermediate)
		};
		let required = vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::STORAGE_IMAGE;
		if !properties.optimal_tiling_features.contains(required) {
			return Err(format!(
				"{intermediate:?} lacks sampled/storage optimal-tiling support required by the PyroWave scaler"
			));
		}
		let api = Api::load()?;
		let device = Device::for_video_context(api, context, PyroWaveQueueMode::Auto, false)?;
		let info = EncoderCreateInfo {
			device: device.handle,
			width: 128,
			height: 128,
			chroma: match chroma {
				ChromaFormat::Yuv420 => 0,
				ChromaFormat::Yuv444 => 1,
			},
		};
		let mut encoder = ptr::null_mut();
		// SAFETY: input and out-pointer are valid for the call.
		check(
			unsafe { (device.api.create_encoder)(&info, &mut encoder) },
			"creating a probe encoder",
		)?;
		if encoder.is_null() {
			return Err("PyroWave returned a null probe encoder".to_string());
		}
		// SAFETY: this is the only owner and no encode is pending.
		unsafe { (device.api.destroy_encoder)(encoder) };
		Ok(())
	}

	pub(crate) fn new(
		context: &VideoContext,
		format: NegotiatedVideoFormat,
		width: u32,
		height: u32,
		bitrate: usize,
		fps: u32,
		queue_mode: PyroWaveQueueMode,
	) -> Result<Self, String> {
		if format.range != ColorRange::Full {
			return Err("PyroWave's scaled RGB path currently supports full-range output only".to_string());
		}
		let maximum_frame_bytes = wire_v1_frame_budget(bitrate, fps).ok_or_else(|| {
			let bytes = bitrate
				.checked_div((fps as usize).saturating_mul(8))
				.unwrap_or_default()
				& !3;
			format!("PyroWave frame budget {bytes} is outside the wire-v1 range (1 KiB to 3 MiB)")
		})?;
		let api = Api::load()?;
		let device = Device::for_video_context(api, context, queue_mode, true)?;
		let info = EncoderCreateInfo {
			device: device.handle,
			width: i32::try_from(width).map_err(|_| "PyroWave width exceeds i32")?,
			height: i32::try_from(height).map_err(|_| "PyroWave height exceeds i32")?,
			chroma: match format.chroma {
				ChromaFormat::Yuv420 => 0,
				ChromaFormat::Yuv444 => 1,
			},
		};
		let mut handle = ptr::null_mut();
		// SAFETY: device and pointers are live for the call; Drop owns the result.
		check(
			unsafe { (device.api.create_encoder)(&info, &mut handle) },
			"creating the PyroWave encoder",
		)?;
		if handle.is_null() {
			return Err("PyroWave returned a null encoder".to_string());
		}
		let metadata = ColorMetadata {
			color_primaries: u32::from(format.primaries == ColorPrimaries::Bt2020),
			transfer_function: u32::from(format.transfer == TransferFunction::Pq),
			ycbcr_transform: u32::from(format.matrix == MatrixCoefficients::Bt2020Ncl),
			ycbcr_range: u32::from(format.range == ColorRange::Limited),
			chroma_siting: 0,
		};
		// The negotiated format is immutable for the encoder lifetime, so set its
		// bitstream metadata once before the first submission.
		if let Err(error) = check(
			unsafe { (device.api.set_color_metadata)(handle, &metadata) },
			"setting PyroWave color metadata",
		) {
			// SAFETY: the handle was created above and no encode is pending.
			unsafe { (device.api.destroy_encoder)(handle) };
			return Err(error);
		}
		Ok(Self {
			device,
			handle,
			format,
			visible_width: width,
			visible_height: height,
			maximum_frame_bytes,
			images: ImageCache::new(),
			consecutive_near_limit_frames: 0,
			last_limit_warning: None,
			scaling_logged: false,
		})
	}

	fn import(&mut self, frame: &ExportedFrame) -> Result<ImageView, String> {
		if frame.planes.is_empty() {
			return Err("compositor exported a DMA-BUF without planes".to_string());
		}
		for plane in &frame.planes[1..] {
			if !same_open_file(frame.planes[0].fd, plane.fd) {
				return Err(
					"PyroWave cannot import a DMA-BUF whose planes use different file descriptions".to_string(),
				);
			}
		}
		let format = drm_fourcc_to_vk(frame.format)?;
		let fd = frame.planes[0].fd;
		let now = Instant::now();
		let reuse = self
			.images
			.get(fd, now)
			.is_some_and(|image| image.matches(frame, format));
		if reuse {
			self.images.stats.hit += 1;
		} else {
			self.images.stats.miss += 1;
			if self.images.entries.remove(&fd).is_some() {
				self.images.stats.recreate += 1;
			}
			let identity_fd = unsafe { BorrowedFd::borrow_raw(fd) }
				.try_clone_to_owned()
				.map_err(|e| format!("duplicating DMA-BUF identity fd: {e}"))?;
			let imported_fd = unsafe { BorrowedFd::borrow_raw(fd) }
				.try_clone_to_owned()
				.map_err(|e| format!("duplicating DMA-BUF import fd: {e}"))?
				.into_raw_fd();
			let plane_layouts: Vec<vk::SubresourceLayout> = frame
				.planes
				.iter()
				.map(|plane| {
					vk::SubresourceLayout::default()
						.offset(plane.offset as u64)
						.row_pitch(plane.stride as u64)
				})
				.collect();
			let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
				.drm_format_modifier(frame.modifier)
				.plane_layouts(&plane_layouts);
			let mut external = vk::ExternalMemoryImageCreateInfo::default()
				.handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
			external.p_next = (&mut modifier as *mut vk::ImageDrmFormatModifierExplicitCreateInfoEXT<'_>).cast();
			let mut vk_info = vk::ImageCreateInfo::default()
				.image_type(vk::ImageType::TYPE_2D)
				.format(format)
				.extent(vk::Extent3D {
					width: frame.width,
					height: frame.height,
					depth: 1,
				})
				.mip_levels(1)
				.array_layers(1)
				.samples(vk::SampleCountFlags::TYPE_1)
				.tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
				.usage(vk::ImageUsageFlags::SAMPLED)
				.sharing_mode(vk::SharingMode::EXCLUSIVE)
				.initial_layout(vk::ImageLayout::UNDEFINED);
			vk_info.p_next = (&mut external as *mut vk::ExternalMemoryImageCreateInfo<'_>).cast();
			let info = ImageCreateInfo {
				device: self.device.handle,
				external_handle: imported_fd as usize,
				handle_type: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
				// The C call consumes this structure synchronously.
				image_create_info: (&vk_info as *const vk::ImageCreateInfo<'_>).cast(),
			};
			let mut handle = ptr::null_mut();
			// SAFETY: the entire pNext chain and plane layout slice remain alive for
			// the call. On success PyroWave owns imported_fd.
			let result = unsafe { (self.device.api.create_image)(&info, &mut handle) };
			if result != SUCCESS {
				// SAFETY: the API only takes handle ownership on successful import.
				unsafe { libc::close(imported_fd) };
				check(result, "importing compositor DMA-BUF into PyroWave")?;
			}
			if handle.is_null() {
				return Err("PyroWave returned a null imported image".to_string());
			}
			self.images.insert(
				fd,
				ImportedImage {
					device: Rc::clone(&self.device),
					handle,
					identity_fd,
					width: frame.width,
					height: frame.height,
					format,
					modifier: frame.modifier,
					layouts: frame.planes.iter().map(|p| (p.offset, p.stride)).collect(),
				},
				now,
			);
		}
		let (image, _) = self.images.entries.get(&fd).expect("inserted or reused PyroWave image");
		let mut view = ImageView::default();
		// SAFETY: the cached image and output pointer are valid. SAMPLED usage
		// matches the imported VkImageCreateInfo.
		check(
			unsafe {
				(self.device.api.get_image_view)(
					image.handle,
					vk::ImageAspectFlags::COLOR,
					vk::ImageUsageFlags::SAMPLED,
					&mut view,
				)
			},
			"creating the PyroWave DMA-BUF image view",
		)?;
		Ok(view)
	}

	pub(crate) fn encode(&mut self, frame: &ExportedFrame, mut data: Vec<u8>) -> Result<EncodedFrame, String> {
		let started = std::time::Instant::now();
		if !self.scaling_logged {
			if frame.width == self.visible_width && frame.height == self.visible_height {
				tracing::debug!(
					width = frame.width,
					height = frame.height,
					"PyroWave source matches the visible stream extent; spatial scaling is bypassed"
				);
			} else {
				tracing::warn!(
					source_width = frame.width,
					source_height = frame.height,
					visible_width = self.visible_width,
					visible_height = self.visible_height,
					"PyroWave is using its GPU scaler for a non-native source extent"
				);
			}
			self.scaling_logged = true;
		}
		let view = self.import(frame)?;
		let imported = std::time::Instant::now();
		let input_color_space = match frame.color_space {
			FrameColorSpace::Srgb => vk::ColorSpaceKHR::SRGB_NONLINEAR,
			FrameColorSpace::Bt2020Pq => vk::ColorSpaceKHR::HDR10_ST2084_EXT,
			FrameColorSpace::ScrgbLinear => vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
		};
		let output_color_space = if self.format.hdr {
			vk::ColorSpaceKHR::HDR10_ST2084_EXT
		} else {
			vk::ColorSpaceKHR::SRGB_NONLINEAR
		};
		let scaling = ScaledEncodeInfo {
			view,
			input_color_space,
			output_color_space,
			intermediate_plane_format: match self.format.bit_depth {
				BitDepth::Eight => vk::Format::R8_UNORM,
				BitDepth::Ten => vk::Format::R16_UNORM,
			},
			ycbcr_chroma_midpoint: match self.format.bit_depth {
				BitDepth::Eight => 128.0 / 255.0,
				BitDepth::Ten => 512.0 / 1023.0,
			},
			// Preserve exact texels at 1:1. For actual resizing, false selects
			// PyroWave's higher-quality scaler rather than forced bilinear sampling.
			force_linear_filtering: false,
			skip_dither: false,
			crop_rect: ptr::null(),
		};
		let rate = RateControl {
			maximum_bitstream_size: self.maximum_frame_bytes,
		};
		// SAFETY: all referenced objects remain alive until packetize waits for
		// this submission below. External DMA-BUF images are documented GENERAL.
		check(
			unsafe { (self.device.api.encode_scaled)(self.handle, ptr::null(), ptr::null(), &scaling, &rate) },
			"submitting the PyroWave GPU encode",
		)?;
		let submitted = std::time::Instant::now();

		let mut packet_count = 0usize;
		// SAFETY: packet_count is a valid out pointer; this waits for GPU work.
		check(
			unsafe { (self.device.api.compute_num_packets)(self.handle, PYROWAVE_MAX_FRAME_BYTES, &mut packet_count) },
			"computing PyroWave packet count",
		)?;
		if packet_count != 1 {
			return Err(format!(
				"PyroWave frame cannot be represented by wire version 1 (produced {packet_count} codec packets)"
			));
		}
		let mut packet = Packet::default();
		let output_capacity = self.maximum_frame_bytes + 4096;
		if data.len() != output_capacity {
			data.resize(output_capacity, 0);
		}
		let mut written_packets = packet_count;
		// SAFETY: output arrays are sized as requested by compute_num_packets;
		// PyroWave validates the bitstream buffer capacity.
		check(
			unsafe {
				(self.device.api.packetize)(
					self.handle,
					&mut packet,
					PYROWAVE_MAX_FRAME_BYTES,
					&mut written_packets,
					data.as_mut_ptr().cast(),
					data.len(),
				)
			},
			"reading the PyroWave bitstream",
		)?;
		if written_packets != 1 || packet.offset != 0 || packet.size < 8 || packet.size > data.len() {
			return Err("PyroWave returned an invalid wire-v1 frame".to_string());
		}
		let ready = std::time::Instant::now();
		if packet.size.saturating_mul(100) >= self.maximum_frame_bytes.saturating_mul(98) {
			self.consecutive_near_limit_frames = self.consecutive_near_limit_frames.saturating_add(1);
			if self.consecutive_near_limit_frames >= 30
				&& self
					.last_limit_warning
					.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(30))
			{
				tracing::warn!(
					encoded_bytes = packet.size,
					maximum_frame_bytes = self.maximum_frame_bytes,
					"PyroWave frames repeatedly saturate the configured frame budget; image quality may be rate-limited"
				);
				self.last_limit_warning = Some(std::time::Instant::now());
			}
		} else {
			self.consecutive_near_limit_frames = 0;
		}
		Ok(EncodedFrame {
			data,
			data_size: packet.size,
			import: imported.duration_since(started),
			submit: submitted.duration_since(imported),
			encode_wait: ready.duration_since(submitted),
		})
	}

	/// Call only at a diagnostic boundary, after packetization completed GPU
	/// work. Granite resolves timestamps when recycling frame contexts; reports
	/// lag by that bounded depth and are averages over resolved contexts.
	pub(crate) fn report_gpu_timings(&mut self, encoded_fps: f64, delivered_fps: f64) {
		let mut stats = GpuTimings::default();
		unsafe {
			(self.device.api.report_performance)(
				self.device.handle,
				performance_callback,
				(&mut stats as *mut GpuTimings).cast(),
				true,
			);
		}
		let c = std::mem::take(&mut self.images.stats);
		tracing::info!(
			import_cache_hit = c.hit,
			import_cache_miss = c.miss,
			import_cache_recreate = c.recreate,
			import_cache_evict = c.evict,
			import_cache = self.images.entries.len(),
			"PyroWave DMA-BUF import summary"
		);
		let s = stats.stages;
		tracing::info!(dwt_gpu_ms = ?s[0], quant_gpu_ms = ?s[1], analyze_gpu_ms = ?s[2], resolve_gpu_ms = ?s[3], packing_gpu_ms = ?s[4], scaler_conversion_gpu_ms = ?s[5],
			pyrowave_stage_gpu_ms_per_encoded_frame = ?stats.total(),
            pyrowave_stage_gpu_ms_per_delivered_frame = ?stats.total().filter(|_| delivered_fps > 0.0).map(|ms| ms * encoded_fps / delivered_fps),
            estimated_stage_gpu_ms_per_delivered_second = ?stats.total().map(|ms| ms * encoded_fps),
			"PyroWave GPU timestamps (exclusive stage sum, excludes transfers; encode_wait is wall-clock latency)");
	}

	pub(crate) fn maximum_frame_bytes(&self) -> usize {
		self.maximum_frame_bytes
	}

	pub(crate) fn import_cache_len(&self) -> usize {
		self.images.entries.len()
	}

	pub(crate) fn device_name(&self) -> &str {
		&self.device.name
	}
}

impl Drop for PyroWaveEncoder {
	fn drop(&mut self) {
		// Images must be destroyed before their device, and the encoder before
		// the device. Clear imports first because C Drop order is explicit here.
		self.images.entries.clear();
		// SAFETY: this wrapper owns the encoder and packetization waited for the
		// last queued frame before returning it.
		unsafe { (self.device.api.destroy_encoder)(self.handle) };
	}
}

fn drm_fourcc_to_vk(fourcc: u32) -> Result<vk::Format, String> {
	match fourcc {
		0x34324241 | 0x34324258 => Ok(vk::Format::R8G8B8A8_UNORM),
		0x34325241 | 0x34325258 => Ok(vk::Format::B8G8R8A8_UNORM),
		0x30334241 | 0x30334258 => Ok(vk::Format::A2B10G10R10_UNORM_PACK32),
		0x30335241 | 0x30335258 => Ok(vk::Format::A2R10G10B10_UNORM_PACK32),
		0x48344241 | 0x48344258 => Ok(vk::Format::R16G16B16A16_SFLOAT),
		_ => Err(format!("unsupported compositor DRM fourcc 0x{fourcc:08x} for PyroWave")),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn import_cache_soak_releases_old_resources_and_preserves_active_buffers() {
		use std::cell::Cell;
		struct Resource(Rc<Cell<usize>>, std::os::fd::OwnedFd);
		impl Drop for Resource {
			fn drop(&mut self) {
				self.0.set(self.0.get() - 1);
			}
		}
		let live = Rc::new(Cell::new(0));
		let resource = || {
			live.set(live.get() + 1);
			Resource(Rc::clone(&live), std::fs::File::open("/dev/null").unwrap().into())
		};
		let mut cache = ImageCache::new();
		let started = Instant::now();
		cache.insert(-1, resource(), started);
		let active_fd = cache.entries[&-1].0.1.as_raw_fd();
		// Simulate minutes of 120 FPS buffer churn without any wall-clock wait.
		for frame in 0..30_000 {
			let now = started + Duration::from_nanos(frame as u64 * 1_000_000_000 / 120);
			assert!(cache.get(-1, now).is_some());
			assert!(cache.get(frame, now).is_none());
			cache.insert(frame, resource(), now);
			assert!(cache.entries.len() <= 272, "cache grew with session duration");
			assert_eq!(live.get(), cache.entries.len());
			assert_eq!(cache.entries[&-1].0.1.as_raw_fd(), active_fd);
		}
		let now = started + Duration::from_secs(300);
		for _ in 0..ImageCache::<Resource>::SWEEP_INTERVAL {
			cache.get(-2, now);
		}
		assert!(cache.entries.is_empty());
		assert_eq!(live.get(), 0);
	}

	#[test]
	fn authoritative_source_is_pinned() {
		assert_eq!(SOURCE_URL, "https://github.com/karsyboy/pyrowave");
		assert_eq!(SOURCE_REVISION.len(), 40);
		assert!(SOURCE_REVISION.chars().all(|c| c.is_ascii_hexdigit()));
		assert_eq!(API_VERSION, (0, 7, 0));
		let nix_dependency = include_str!("../../../../../nix/pyrowave.nix");
		assert!(nix_dependency.contains(SOURCE_URL));
		assert!(nix_dependency.contains(SOURCE_REVISION));
		assert!(!nix_dependency.contains("https://github.com/Themaister/pyrowave"));
	}

	#[test]
	fn known_compositor_formats_map_without_cpu_conversion() {
		for fourcc in [0x34324241, 0x34325241, 0x30334241, 0x30335241, 0x48344241] {
			assert!(drm_fourcc_to_vk(fourcc).is_ok());
		}
	}

	#[test]
	fn wire_v1_budget_matches_the_client_protocol() {
		assert_eq!(wire_v1_frame_budget(200_000_000, 60), Some(416_664));
		assert_eq!(wire_v1_frame_budget(1_000_000_000, 120), Some(1_041_664));
		assert_eq!(wire_v1_frame_budget(2_000_000_000, 120), Some(2_083_332));
		assert_eq!(wire_v1_frame_budget(2_000_000_000, 60), None);
		assert_eq!(wire_v1_frame_budget(200_000_000, 0), None);
	}

	#[test]
	fn quality_reference_preserves_bytes_per_frame_across_fps() {
		assert_eq!(
			quality_reference_frame_bytes(3840, 2160, ChromaFormat::Yuv420, BitDepth::Eight),
			400_000
		);
		assert_eq!(
			quality_reference_frame_bytes(1920, 1080, ChromaFormat::Yuv420, BitDepth::Eight),
			100_000
		);
		assert_eq!(
			quality_reference_frame_bytes(3840, 2160, ChromaFormat::Yuv444, BitDepth::Eight),
			480_000
		);
		assert_eq!(
			quality_reference_frame_bytes(3840, 2160, ChromaFormat::Yuv420, BitDepth::Ten),
			480_000
		);
	}

	#[test]
	fn ffi_loads_pinned_api() {
		if std::env::var_os("MOONSHINE_TEST_PYROWAVE").is_some() {
			Api::load().expect("the packaged authoritative PyroWave library must expose the pinned ABI");
		}
	}
	#[test]
	fn queue_config_defaults_to_graphics_and_compute_is_explicit() {
		let default_config = toml::from_str::<super::super::VideoStreamConfig>("").unwrap();
		assert_eq!(default_config.pyrowave_queue.flags(), vk::QueueFlags::GRAPHICS);
		for (mode, expected) in [
			("auto", vk::QueueFlags::GRAPHICS),
			("graphics", vk::QueueFlags::GRAPHICS),
			("compute", vk::QueueFlags::COMPUTE),
		] {
			let config =
				toml::from_str::<super::super::VideoStreamConfig>(&format!("pyrowave_queue = '{mode}'")).unwrap();
			assert_eq!(config.pyrowave_queue.flags(), expected);
		}
		assert!(toml::from_str::<super::super::VideoStreamConfig>("pyrowave_queue = 'high'").is_err());
	}
	#[test]
	fn gpu_report_ignores_cpu_and_memory_and_requires_all_stages() {
		let mut timings = GpuTimings::default();
		for msg in [
			"submit: 100.000 ms per frame",
			"Memory Heap 0 (DEVICE): MaxSize 12.0 MiB",
			"DWT: NaN ms per frame",
			"DWT: -1 ms per frame",
		] {
			timings.record(msg);
		}
		assert!(timings.total().is_none());
		for tag in ["DWT", "Quant", "Analyze", "Resolve", "Packing", "scale"] {
			timings.record(&format!("{tag}: 0.100 ms per frame"));
		}
		assert!((timings.total().unwrap() - 0.6).abs() < 1e-9);
	}
}

#[cfg(test)]
mod queue_fallback_tests {
	use super::*;
	#[test]
	fn compute_selection_failure_uses_safe_graphics_default() {
		let mut attempts = Vec::new();
		let (flags, result) = select_queue(PyroWaveQueueMode::Compute, |flags| {
			attempts.push(flags);
			if flags == vk::QueueFlags::COMPUTE { -1 } else { SUCCESS }
		});
		assert_eq!(flags, vk::QueueFlags::GRAPHICS);
		assert_eq!(result, -1);
		assert_eq!(attempts, [vk::QueueFlags::COMPUTE, vk::QueueFlags::GRAPHICS]);
		let (flags, _) = select_queue(PyroWaveQueueMode::Compute, |_| -1);
		assert_eq!(flags, vk::QueueFlags::GRAPHICS);
	}
	#[test]
	fn auto_never_requests_compute() {
		let (flags, _) = select_queue(PyroWaveQueueMode::Auto, |flags| {
			assert_eq!(flags, vk::QueueFlags::GRAPHICS);
			SUCCESS
		});
		assert_eq!(flags, vk::QueueFlags::GRAPHICS);
	}
}
