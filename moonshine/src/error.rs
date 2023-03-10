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
