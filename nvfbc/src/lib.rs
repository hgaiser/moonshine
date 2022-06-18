use std::{ffi::{CStr, c_void}, mem::MaybeUninit, ptr::null_mut, os::raw::{c_ulonglong, c_uint}};

use nvfbc_sys::{NVFBC_VERSION, NVFBC_API_FUNCTION_LIST, _NVFBCSTATUS_NVFBC_SUCCESS, NVFBCSTATUS};

mod types;
mod error;

pub use types::*;
pub use error::NvFbcError;

#[derive(Debug, Copy, Clone)]
pub struct CudaFrameInfo {
	/// Pointer to the CUDA device where the frame is grabbed.
	pub device_buffer: *mut c_void,
	/// Width of the captured frame.
	pub width: u32,
	/// Height of the captured frame.
	pub height: u32,
	/// Size of the frame in bytes.
	pub byte_size: u32,
	/// Incremental ID of the current frame.
	///
	/// This can be used to identify a frame.
	pub current_frame: u32,
}

pub struct NvFbc {
	#[allow(dead_code)]
	lib: libloading::Library,

	nvfbc_funcs: NVFBC_API_FUNCTION_LIST,

	handle: nvfbc_sys::NVFBC_SESSION_HANDLE,
}

impl NvFbc {
	pub fn new() -> Result<Self, NvFbcError> {
		let lib = unsafe { libloading::Library::new("libnvidia-fbc.so") }?;

		let nvfbc_funcs = Self::create_instance(&lib)?;
		let handle = Self::create_handle(&nvfbc_funcs)?;

		Ok(Self { lib, nvfbc_funcs, handle })
	}

	fn create_handle(nvfbc_funcs: &NVFBC_API_FUNCTION_LIST) -> Result<nvfbc_sys::NVFBC_SESSION_HANDLE, NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_CREATE_HANDLE_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_CREATE_HANDLE_PARAMS_VER;
		let mut handle = 0;
		let ret = unsafe { nvfbc_funcs.nvFBCCreateHandle.unwrap()(
			&mut handle as *mut nvfbc_sys::NVFBC_SESSION_HANDLE,
			&mut params
		)};
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(handle)
	}

	fn destroy_handle(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_DESTROY_HANDLE_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_DESTROY_HANDLE_PARAMS_VER;
		let ret = unsafe { self.nvfbc_funcs.nvFBCDestroyHandle.unwrap()(self.handle, &mut params) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(())
	}

	fn create_instance(lib: &libloading::Library) -> Result<NVFBC_API_FUNCTION_LIST, NvFbcError> {
		let nvfbc_create_instance: libloading::Symbol<unsafe extern fn(*mut NVFBC_API_FUNCTION_LIST) -> NVFBCSTATUS> =
			unsafe { lib.get(b"NvFBCCreateInstance")? };

		let mut nvfbc_funcs: NVFBC_API_FUNCTION_LIST = unsafe { MaybeUninit::zeroed().assume_init() };
		nvfbc_funcs.dwVersion = NVFBC_VERSION;
		let ret = unsafe { nvfbc_create_instance(&mut nvfbc_funcs as *mut NVFBC_API_FUNCTION_LIST) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(nvfbc_funcs)
	}

	pub fn get_last_error(&self) -> Result<&str, NvFbcError> {
		let error = unsafe {self.nvfbc_funcs.nvFBCGetLastErrorStr.unwrap()(self.handle) };
		let error = unsafe { CStr::from_ptr(error) };
		error.to_str().map_err(|_| NvFbcError::Utf8)
	}

	pub fn get_status(&self) -> Result<Status, NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_GET_STATUS_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_GET_STATUS_PARAMS_VER;
		let ret = unsafe { self.nvfbc_funcs.nvFBCGetStatus.unwrap()(self.handle, &mut params as *mut nvfbc_sys::_NVFBC_GET_STATUS_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(params.into())
	}

	pub fn create_capture_session(&self, capture_type: CaptureType) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_CREATE_CAPTURE_SESSION_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_CREATE_CAPTURE_SESSION_PARAMS_VER;
		params.eCaptureType = capture_type as c_uint;
		params.bWithCursor = nvfbc_sys::_NVFBC_BOOL_NVFBC_TRUE;
		params.frameSize = nvfbc_sys::NVFBC_SIZE { w: 0, h: 0 };
		params.eTrackingType = nvfbc_sys::NVFBC_TRACKING_TYPE_NVFBC_TRACKING_DEFAULT;
		let ret = unsafe { self.nvfbc_funcs.nvFBCCreateCaptureSession.unwrap()(self.handle, &mut params as *mut nvfbc_sys::_NVFBC_CREATE_CAPTURE_SESSION_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(())
	}

	pub fn destroy_capture_session(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_DESTROY_CAPTURE_SESSION_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_DESTROY_HANDLE_PARAMS_VER;
		let ret = unsafe { self.nvfbc_funcs.nvFBCDestroyCaptureSession.unwrap()(self.handle, &mut params) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(())
	}

	pub fn to_gl_setup(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::NVFBC_TOGL_SETUP_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_TOGL_SETUP_PARAMS_VER;
		params.eBufferFormat = BufferFormat::Rgb as u32;
		let ret = unsafe { self.nvfbc_funcs.nvFBCToGLSetUp.unwrap()(self.handle, &mut params as *mut nvfbc_sys::NVFBC_TOGL_SETUP_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(())
	}

	pub fn to_cuda_setup(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::NVFBC_TOCUDA_SETUP_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_TOCUDA_SETUP_PARAMS_VER;
		params.eBufferFormat = BufferFormat::Rgb as u32;
		let ret = unsafe { self.nvfbc_funcs.nvFBCToCudaSetUp.unwrap()(self.handle, &mut params as *mut nvfbc_sys::NVFBC_TOCUDA_SETUP_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(())
	}

	pub fn to_cuda_grab_frame(&self) -> Result<CudaFrameInfo, NvFbcError> {
		let mut device_buffer: *mut c_void = null_mut();
		let mut frame_info: nvfbc_sys::NVFBC_FRAME_GRAB_INFO = unsafe { MaybeUninit::zeroed().assume_init() };
		let mut params: nvfbc_sys::NVFBC_TOCUDA_GRAB_FRAME_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_TOCUDA_GRAB_FRAME_PARAMS_VER;
		params.dwFlags = nvfbc_sys::NVFBC_TOCUDA_FLAGS_NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT;
		params.pFrameGrabInfo = &mut frame_info as *mut nvfbc_sys::NVFBC_FRAME_GRAB_INFO;
		params.pCUDADeviceBuffer = &mut device_buffer as *mut _ as *mut c_void;
		let ret = unsafe { self.nvfbc_funcs.nvFBCToCudaGrabFrame.unwrap()(self.handle, &mut params as *mut nvfbc_sys::NVFBC_TOCUDA_GRAB_FRAME_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(ret.into());
		}

		Ok(CudaFrameInfo {
			device_buffer,
			width: frame_info.dwWidth,
			height: frame_info.dwHeight,
			byte_size: frame_info.dwByteSize,
			current_frame: frame_info.dwCurrentFrame,
		})
	}
}

impl Drop for NvFbc {
	fn drop(&mut self) {
		// TODO: Figure out why this crashes (nvfbc examples also fail here..
		self.destroy_handle().unwrap();
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn it_works() {
		let result = 2 + 2;
		assert_eq!(result, 4);
	}
}
