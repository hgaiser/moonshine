use std::fmt;

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
