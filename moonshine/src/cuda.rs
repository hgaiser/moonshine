use std::{os::raw::{c_int, c_char}, ptr::null_mut, ffi::CStr};

use ffmpeg_sys::{
	cuDeviceGetCount,
	cuDeviceGet,
	cuCtxCreate_v2,
	cuInit,
	CUdevice,
	CUcontext,
	CUctx_flags_enum_CU_CTX_SCHED_AUTO,
	CUresult,
	cudaError_enum_CUDA_SUCCESS, cuGetErrorString, cuCtxSetCurrent
};

use ffmpeg::CudaError;

pub struct CudaContext {
	context: CUcontext,
}

impl CudaContext {
	pub fn new(gpu: i32) -> Result<CudaContext, CudaError> {
		unsafe {
			check_ret(cuInit(0))?;

			let mut number_of_gpus: c_int = 0;
			check_ret(cuDeviceGetCount(&mut number_of_gpus))?;

			let mut device: CUdevice = 0;
			check_ret(cuDeviceGet(&mut device, gpu as c_int))?;

			let mut context: CUcontext = null_mut();
			check_ret(cuCtxCreate_v2(&mut context, CUctx_flags_enum_CU_CTX_SCHED_AUTO, device))?;
			Ok(Self { context })
		}
	}

	pub fn as_raw(&self) -> CUcontext {
		self.context
	}

	pub fn set_current(&self) -> Result<(), CudaError> {
		unsafe {
			check_ret(cuCtxSetCurrent(self.context))?;
			Ok(())
		}
	}
}

pub fn check_ret(error_code: CUresult) -> Result<(), CudaError> {
	if error_code != cudaError_enum_CUDA_SUCCESS {
		let error_message = get_error(error_code)
			.map_err(|_| CudaError::new(error_code, "Unknown error".into()))?;
		return Err(CudaError::new(error_code, error_message));
	}

	Ok(())
}

fn get_error(error_code: CUresult) -> Result<String, String> {
	unsafe {
		let mut error: *const c_char = null_mut();
		cuGetErrorString(error_code, &mut error);
		Ok(CStr::from_ptr(error)
			.to_str()
			.map_err(|e| format!("Failed to convert to str: {}", e))?
			.to_string()
		)
	}
}

unsafe impl Send for CudaContext { }
