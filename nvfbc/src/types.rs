use std::ffi::CStr;

#[derive(Debug, Copy, Clone)]
pub enum CaptureType {
	/// Capture frames to a buffer in system memory.
	ToSystem = nvfbc_sys::_NVFBC_CAPTURE_TYPE_NVFBC_CAPTURE_TO_SYS as isize,
	/// Capture frames to a CUDA device in video memory.
	///
	/// Specifying this will dlopen() libcuda.so.1 and fail if not available.
	SharedCuda = nvfbc_sys::_NVFBC_CAPTURE_TYPE_NVFBC_CAPTURE_SHARED_CUDA as isize,
	/// Capture frames to an OpenGL buffer in video memory.
	ToOpenGl = nvfbc_sys::_NVFBC_CAPTURE_TYPE_NVFBC_CAPTURE_TO_GL as isize,
}

#[derive(Debug, Copy, Clone)]
pub enum BufferFormat {
	/// Data will be converted to ARGB8888 byte-order format. 32 bpp.
	Argb = nvfbc_sys::_NVFBC_BUFFER_FORMAT_NVFBC_BUFFER_FORMAT_ARGB as isize,
	/// Data will be converted to RGB888 byte-order format. 24 bpp.
	Rgb = nvfbc_sys::_NVFBC_BUFFER_FORMAT_NVFBC_BUFFER_FORMAT_RGB as isize,
	/// Data will be converted to NV12 format using HDTV weights
	/// according to ITU-R BT.709.  12 bpp.
	Nv12 = nvfbc_sys::_NVFBC_BUFFER_FORMAT_NVFBC_BUFFER_FORMAT_NV12 as isize,
	/// Data will be converted to YUV 444 planar format using HDTV weights
	/// according to ITU-R BT.709.  24 bpp
	Yuv444p = nvfbc_sys::_NVFBC_BUFFER_FORMAT_NVFBC_BUFFER_FORMAT_YUV444P as isize,
	/// Data will be converted to RGBA8888 byte-order format. 32 bpp.
	Rgba = nvfbc_sys::_NVFBC_BUFFER_FORMAT_NVFBC_BUFFER_FORMAT_RGBA as isize,
	/// Native format. No pixel conversion needed.
	/// BGRA8888 byte-order format. 32 bpp.
	Bgra = nvfbc_sys::_NVFBC_BUFFER_FORMAT_NVFBC_BUFFER_FORMAT_BGRA as isize,
}

/// Box used to describe an area of the tracked region to capture.
///
/// The coordinates are relative to the tracked region.
///
/// E.g., if the size of the X screen is 3520x1200 and the tracked RandR output
/// scans a region of 1600x1200+1920+0, then setting a capture box of
/// 800x600+100+50 effectively captures a region of 800x600+2020+50 relative to
/// the X screen.
#[derive(Debug, Copy, Clone)]
pub struct Box {
	/// X offset of the box.
	pub x: u32,
	/// Y offset of the box.
	pub y: u32,
	/// Width of the box.
	pub w: u32,
	/// Height of the box.
	pub h: u32,
}

/// Size used to describe the size of a frame.
#[derive(Debug, Copy, Clone)]
pub struct Size {
	/// Width.
	pub w: u32,
	/// Height.
	pub h: u32,
}

/// Describes an RandR output.
///
/// Filling this structure relies on the XRandR extension.  This feature cannot
/// be used if the extension is missing or its version is below the requirements.
#[derive(Debug, Clone)]
pub struct Output {
	/// Identifier of the RandR output.
	pub id: u32,

	/// Name of the RandR output, as reported by tools such as xrandr(1).
	///
	/// Example: "DVI-I-0"
	pub name: String,

	/// Region of the X screen tracked by the RandR CRTC driving this RandR output.
	pub tracked_box: Box,
}

#[derive(Debug, Clone)]
pub struct Status {
	/// Whether or not framebuffer capture is supported by the graphics driver.
	pub is_capture_possible: bool,

	///  Whether or not there is already a capture session on this system.
	pub currently_capturing: bool,

	/// Whether or not it is possible to create a capture session on this system."]
	pub can_create_now: bool,

	/// Size of the X screen (framebuffer).
	pub screen_size: Size,

	/// Whether the XRandR extension is available.
	///
	/// If this extension is not available then it is not possible to have information about RandR outputs.
	pub xrandr_available: bool,

	/// Array of outputs connected to the X screen.
	///
	/// An application can track a specific output by specifying its ID when creating a capture session.
	///
	/// Only if XRandR is available.
	pub outputs: Vec<Output>,

	/// Version of the NvFBC library running on this system.
	pub nvfbc_version: u32,

	/// Whether the X server is currently in modeset.
	///
	/// When the X server is in modeset, it must give up all its video
	/// memory allocations. It is not possible to create a capture
	/// session until the modeset is over.
	///
	/// Note that VT-switches are considered modesets.
	pub in_modeset: bool,
}

impl From<nvfbc_sys::_NVFBC_GET_STATUS_PARAMS> for Status {
	fn from(status: nvfbc_sys::_NVFBC_GET_STATUS_PARAMS) -> Self {
		let mut outputs = Vec::with_capacity(status.dwOutputNum as usize);
		for output_index in 0..status.dwOutputNum {
			outputs.push(Output {
				id: status.outputs[output_index as usize].dwId,
				name: unsafe {
					CStr::from_ptr(&status.outputs[output_index as usize].name as *const i8)
						.to_str().unwrap().to_string()
				},
				tracked_box: Box {
					x: status.outputs[output_index as usize].trackedBox.x,
					y: status.outputs[output_index as usize].trackedBox.y,
					w: status.outputs[output_index as usize].trackedBox.w,
					h: status.outputs[output_index as usize].trackedBox.h,
				},
			});
		}

		Self {
			is_capture_possible: status.bIsCapturePossible == nvfbc_sys::_NVFBC_BOOL_NVFBC_TRUE,
			currently_capturing: status.bCurrentlyCapturing == nvfbc_sys::_NVFBC_BOOL_NVFBC_TRUE,
			can_create_now: status.bCanCreateNow == nvfbc_sys::_NVFBC_BOOL_NVFBC_TRUE,
			screen_size: Size { w: status.screenSize.w, h: status.screenSize.h },
			xrandr_available: status.bXRandRAvailable == nvfbc_sys::_NVFBC_BOOL_NVFBC_TRUE,
			outputs,
			nvfbc_version: status.dwNvFBCVersion,
			in_modeset: status.bInModeset == nvfbc_sys::_NVFBC_BOOL_NVFBC_TRUE,
		}
	}
}
