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
	handle: nvfbc_sys::NVFBC_SESSION_HANDLE,
}

impl NvFbc {
	pub fn new() -> Result<Self, NvFbcError> {
		let handle = Self::create_handle()?;

		Ok(Self { handle })
	}

	fn create_handle() -> Result<nvfbc_sys::NVFBC_SESSION_HANDLE, NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_CREATE_HANDLE_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_CREATE_HANDLE_PARAMS_VER;
		let mut handle = 0;
		let ret = unsafe { nvfbc_sys::NvFBCCreateHandle(
			&mut handle as *mut nvfbc_sys::NVFBC_SESSION_HANDLE,
			&mut params
		)};
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), None));
		}

		Ok(handle)
	}

	fn destroy_handle(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_DESTROY_HANDLE_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_DESTROY_HANDLE_PARAMS_VER;
		let ret = unsafe { nvfbc_sys::NvFBCDestroyHandle(self.handle, &mut params) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
		}

		Ok(())
	}

	pub fn get_last_error(&self) -> Result<String, NvFbcError> {
		let error = unsafe { nvfbc_sys::NvFBCGetLastErrorStr(self.handle) };
		let error = unsafe { CStr::from_ptr(error) };
		error.to_str().map_err(|_| NvFbcError::Utf8).map(|o| o.to_string())
	}

	pub fn get_status(&self) -> Result<Status, NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_GET_STATUS_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_GET_STATUS_PARAMS_VER;
		let ret = unsafe { nvfbc_sys::NvFBCGetStatus(self.handle, &mut params as *mut nvfbc_sys::_NVFBC_GET_STATUS_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
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
		let ret = unsafe { nvfbc_sys::NvFBCCreateCaptureSession(self.handle, &mut params as *mut nvfbc_sys::_NVFBC_CREATE_CAPTURE_SESSION_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
		}

		Ok(())
	}

	pub fn destroy_capture_session(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::_NVFBC_DESTROY_CAPTURE_SESSION_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_DESTROY_CAPTURE_SESSION_PARAMS_VER;
		let ret = unsafe { nvfbc_sys::NvFBCDestroyCaptureSession(self.handle, &mut params) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
		}

		Ok(())
	}

	pub fn to_gl_setup(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::NVFBC_TOGL_SETUP_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_TOGL_SETUP_PARAMS_VER;
		params.eBufferFormat = BufferFormat::Rgb as u32;
		let ret = unsafe { nvfbc_sys::NvFBCToGLSetUp(self.handle, &mut params as *mut nvfbc_sys::NVFBC_TOGL_SETUP_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
		}

		Ok(())
	}

	pub fn to_cuda_setup(&self) -> Result<(), NvFbcError> {
		let mut params: nvfbc_sys::NVFBC_TOCUDA_SETUP_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
		params.dwVersion = nvfbc_sys::NVFBC_TOCUDA_SETUP_PARAMS_VER;
		params.eBufferFormat = BufferFormat::Rgb as u32;
		let ret = unsafe { nvfbc_sys::NvFBCToCudaSetUp(self.handle, &mut params as *mut nvfbc_sys::NVFBC_TOCUDA_SETUP_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
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
		let ret = unsafe { nvfbc_sys::NvFBCToCudaGrabFrame(self.handle, &mut params as *mut nvfbc_sys::NVFBC_TOCUDA_GRAB_FRAME_PARAMS) };
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			return Err(NvFbcError::InternalError(ret.into(), self.get_last_error().ok()));
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
		// TODO: Figure out why this crashes (nvfbc examples also fail here..)
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
