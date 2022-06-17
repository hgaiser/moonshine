use nvfbc_sys::NVFBCSTATUS;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum NvFbcError {
	#[error("failed to load library")]
	LibraryLoading(#[from] libloading::Error),

	// TODO: Add get_last_error information in errors.

	/// This indicates that the API version between the client and the library is not compatible.
	#[error("invalid API version")]
	ApiVersion,
	/// An internal error occurred.
	#[error("internal error occurred")]
	Internal,
	/// This indicates that one or more of the parameter passed to the API call is invalid.
	#[error("received invalid parameter")]
	InvalidParam,
	/// This indicates that one or more of the pointers passed to the API call is invalid.
	#[error("received invalid pointer")]
	InvalidPtr,
	/// This indicates that the handle passed to the API call to identify the client is invalid.
	#[error("received invalid handle")]
	InvalidHandle,
	/// This indicates that the maximum number of threaded clients of the same process has been reached.
	/// The limit is 10 threads per process. There is no limit on the number of process.
	#[error("reached maximum number of threaded clients")]
	MaxClients,
	/// This indicates that the requested feature is not currently supported by the library.
	#[error("the requested feature is unsupported")]
	Unsupported,
	/// This indicates that the API call failed because it was unable to allocate
	/// enough memory to perform the requested operation.
	#[error("unable to allocate enough memory")]
	OutOfMemory,
	/// This indicates that the API call was not expected.
	/// This happens when API calls are performed in a wrong order,
	/// such as trying to capture a frame prior to creating a new capture session;
	/// or trying to set up a capture to video memory although a capture session to system memory was created.
	#[error("received unexpected API call")]
	BadRequest,
	/// This indicates an X error, most likely meaning that the X server has
	/// been terminated.  When this error is returned, the only resort is to
	/// create another FBC handle using NvFBCCreateHandle().
	///
	/// The previous handle should still be freed with NvFBCDestroyHandle(), but
	/// it might leak resources, in particular X, GLX, and GL resources since
	/// it is no longer possible to communicate with an X server to free them
	/// through the driver.
	///
	/// The best course of action to eliminate this potential leak is to close
	/// the OpenGL driver, close the forked process running the capture, or
	/// restart the application.
	#[error("an X error occured")]
	X,
	/// This indicates a GLX error.
	#[error("a GLX error occured")]
	Glx,
	/// This indicates an OpenGL error.
	#[error("an OpenGL error occured")]
	Gl,
	/// This indicates a CUDA error.
	#[error("a CUDA error occured")]
	Cuda,
	/// This indicates a HW encoder error.
	#[error("an encoder error occured")]
	Encoder,
	/// This indicates an NvFBC context error.
	#[error("an NvFBC context error occured")]
	Context,
	/// This indicates that the application must recreate the capture session.
	///
	/// This error can be returned if a modeset event occurred while capturing
	/// frames, and NVFBC_CREATE_HANDLE_PARAMS::bDisableAutoModesetRecovery
	/// was set to NVFBC_TRUE.
	#[error("must recreate capture session")]
	MustRecreate,
	/// This indicates a Vulkan error.
	#[error("a vulkan error occured")]
	Vulkan,

	#[error("an unknown error occured")]
	Unknown,
	#[error("a utf-8 error occured")]
	Utf8,
}

impl From<NVFBCSTATUS> for NvFbcError {
	fn from(error: NVFBCSTATUS) -> Self {
		match error {
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_API_VERSION => NvFbcError::ApiVersion,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_INTERNAL => NvFbcError::Internal,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_INVALID_PARAM => NvFbcError::InvalidParam,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_INVALID_PTR => NvFbcError::InvalidPtr,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_INVALID_HANDLE => NvFbcError::InvalidHandle,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_MAX_CLIENTS => NvFbcError::MaxClients,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_UNSUPPORTED => NvFbcError::Unsupported,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_OUT_OF_MEMORY => NvFbcError::OutOfMemory,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_BAD_REQUEST => NvFbcError::BadRequest,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_X => NvFbcError::X,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_GLX => NvFbcError::Glx,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_GL => NvFbcError::Gl,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_CUDA => NvFbcError::Cuda,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_ENCODER => NvFbcError::Encoder,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_CONTEXT => NvFbcError::Context,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_MUST_RECREATE => NvFbcError::MustRecreate,
			nvfbc_sys::_NVFBCSTATUS_NVFBC_ERR_VULKAN => NvFbcError::Vulkan,
			_ => NvFbcError::Unknown,
		}
	}
}
