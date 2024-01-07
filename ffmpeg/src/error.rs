use std::fmt;

#[derive(Debug)]
pub struct CudaError {
	code: u32,
	message: String,
}

impl CudaError {
	pub fn new(code: u32, message: String) -> Self {
		CudaError { code, message }
	}
}

impl fmt::Display for CudaError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "CudaError({}): {}", self.code, self.message)?;
		Ok(())
	}
}

impl std::error::Error for CudaError {}

#[derive(Debug)]
pub struct FfmpegError {
	pub code: i32,
	pub message: String,
}

impl FfmpegError {
	pub fn new(code: i32, message: String) -> Self {
		FfmpegError { code, message }
	}
}

impl fmt::Display for FfmpegError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "FfmpegError({}): {}", self.code, self.message)?;
		Ok(())
	}
}

impl std::error::Error for FfmpegError {}
