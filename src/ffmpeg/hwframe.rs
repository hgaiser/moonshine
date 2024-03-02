use std::ptr::null_mut;

use ffmpeg::format::Pixel;

use super::{check_ret, hwdevice::CudaDeviceContext};

pub struct HwFrameContext {
	_cuda_device_context: CudaDeviceContext,
	buffer: *mut ffmpeg::sys::AVBufferRef,
}

impl HwFrameContext {
	fn new(cuda_device_context: CudaDeviceContext, buffer: *mut ffmpeg::sys::AVBufferRef) -> Self {
		Self { _cuda_device_context: cuda_device_context, buffer }
	}

	// pub fn as_context_mut(&mut self) -> &mut ffmpeg::sys::AVHWFramesContext {
	// 	unsafe { &mut *((*self.buffer).data as *mut ffmpeg::sys::AVHWFramesContext) }
	// }

	// pub fn as_context(&self) -> &ffmpeg::sys::AVHWFramesContext {
	// 	unsafe { &*((*self.buffer).data as *const ffmpeg::sys::AVHWFramesContext) }
	// }

	pub fn as_raw_mut(&mut self) -> &mut ffmpeg::sys::AVBufferRef {
		unsafe { &mut *self.buffer }
	}

	// pub fn as_raw(&self) -> &ffmpeg::sys::AVBufferRef {
	// 	unsafe { &*self.buffer }
	// }
}

unsafe impl Send for HwFrameContext { }

pub struct HwFrameContextBuilder {
	cuda_device_context: CudaDeviceContext,
	buffer: *mut ffmpeg::sys::AVBufferRef,
}

impl HwFrameContextBuilder {
	pub fn new(mut cuda_device_context: CudaDeviceContext) -> Result<Self, String> {
		let buffer = unsafe { ffmpeg::sys::av_hwframe_ctx_alloc(cuda_device_context.as_raw_mut()) };
		if buffer.is_null() {
			return Err("could not allocate a hwframe".to_string());
		}

		Ok(Self { cuda_device_context, buffer })
	}

	pub fn build(mut self) -> Result<HwFrameContext, ffmpeg::Error> {
		check_ret(unsafe { ffmpeg::sys::av_hwframe_ctx_init(self.buffer) })?;
		let result = Ok(HwFrameContext::new(self.cuda_device_context, self.buffer));
		self.buffer = null_mut();

		result
	}

	pub fn set_width(mut self, width: u32) -> Self {
		self.as_frame_mut().width = width as i32;
		self
	}

	pub fn set_height(mut self, height: u32) -> Self {
		self.as_frame_mut().height = height as i32;
		self
	}

	pub fn set_sw_format(mut self, sw_format: Pixel) -> Self {
		self.as_frame_mut().sw_format = sw_format.into();
		self
	}

	pub fn set_format(mut self, format: Pixel) -> Self {
		self.as_frame_mut().format = format.into();
		self
	}

	pub fn as_frame_mut(&mut self) -> &mut ffmpeg::sys::AVHWFramesContext {
		unsafe { &mut *((*self.buffer).data as *mut ffmpeg::sys::AVHWFramesContext) }
	}

	// pub fn as_frame(&self) -> &ffmpeg::sys::AVHWFramesContext {
	// 	unsafe { &*((*self.buffer).data as *const ffmpeg::sys::AVHWFramesContext) }
	// }
}
