pub mod hwdevice;
pub mod hwframe;

pub fn check_ret(error_code: i32) -> Result<(), ffmpeg::Error> {
	if error_code != 0 {
		return Err(ffmpeg::Error::from(error_code));
	}

	Ok(())
}
