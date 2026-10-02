//! Numeric domains shared by HTTP launch/resume and RTSP ANNOUNCE.
//!
//! Parsing succeeds for many values that downstream consumers cannot use
//! (zero frame rates divide, oversize extents overflow allocation arithmetic,
//! unknown channel counts silently became stereo). Every entry path validates
//! against these domains *before* it changes the session, so a rejected request
//! leaves a working stream untouched.
//!
//! The bounds are representational limits of the consumers, not quality
//! policy: they deliberately admit 8K, high-refresh and multi-gigabit requests.

use crate::session::stream::audio::AudioChannels;

/// Largest accepted width or height. This is the 2D image limit of the Mesa
/// (RADV/ANV) drivers that back the compositor's GBM targets and the Vulkan
/// import path, and exceeds every Vulkan Video coded extent (at most 8192).
/// It also keeps `width * height * bytes_per_pixel` far inside `u32`/`i32`
/// for the smithay output mode, GBM and Vulkan extents. Codec-specific coded
/// extents are still enforced by the encoder.
pub(crate) const MAX_VIDEO_DIMENSION: u32 = 16_384;

/// Largest refresh rate the compositor can advertise: `wl_output` modes carry
/// the refresh in millihertz as a signed 32-bit integer.
pub(crate) const MAX_REFRESH_RATE_HZ: u32 = (i32::MAX / 1000) as u32;

/// Audio packet durations the mixer clock and Opus framing implement
/// (Moonlight negotiates 5 ms, or 10 ms for slow decoders/networks).
pub(crate) const SUPPORTED_AUDIO_PACKET_DURATIONS_MS: [u32; 2] = [5, 10];

/// Validate a stream extent and refresh rate.
pub(crate) fn validate_display_mode(width: u32, height: u32, refresh_rate: u32) -> Result<(), String> {
	for (name, value) in [("width", width), ("height", height)] {
		if !(1..=MAX_VIDEO_DIMENSION).contains(&value) {
			return Err(format!("{name} {value} is outside 1..={MAX_VIDEO_DIMENSION}"));
		}
	}
	if !(1..=MAX_REFRESH_RATE_HZ).contains(&refresh_rate) {
		return Err(format!(
			"refresh rate {refresh_rate} is outside 1..={MAX_REFRESH_RATE_HZ}"
		));
	}
	Ok(())
}

/// A launch/resume `mode=WIDTHxHEIGHTxREFRESH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayMode {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
}

impl DisplayMode {
	pub(crate) fn parse(mode: &str) -> Result<Self, String> {
		let parts: Vec<&str> = mode.split('x').collect();
		let [width, height, refresh_rate] = parts.as_slice() else {
			return Err(format!("expected mode in format WxHxR, got '{mode}'"));
		};
		let parse = |name: &str, value: &str| {
			value
				.parse::<u32>()
				.map_err(|error| format!("invalid {name} '{value}' in mode: {error}"))
		};
		let mode = Self {
			width: parse("width", width)?,
			height: parse("height", height)?,
			refresh_rate: parse("refresh rate", refresh_rate)?,
		};
		validate_display_mode(mode.width, mode.height, mode.refresh_rate)?;
		Ok(mode)
	}
}

/// Parse a GameStream channel count (stereo, 5.1 or 7.1).
pub(crate) fn audio_channels(count: u32) -> Result<AudioChannels, String> {
	match count {
		2 => Ok(AudioChannels::Stereo),
		6 => Ok(AudioChannels::Surround51),
		8 => Ok(AudioChannels::Surround71),
		_ => Err(format!("unsupported audio channel count {count} (expected 2, 6 or 8)")),
	}
}

/// Parse launch/resume `surroundAudioInfo`: channel mask in the high 16 bits,
/// channel count in the low 16 bits.
pub(crate) fn surround_audio_info(value: &str) -> Result<(AudioChannels, u32), String> {
	let value: u32 = value
		.parse()
		.map_err(|error| format!("invalid surroundAudioInfo '{value}': {error}"))?;
	Ok((audio_channels(value & 0xffff)?, value >> 16))
}

pub(crate) fn validate_audio_packet_duration(packet_duration_ms: u32) -> Result<(), String> {
	if SUPPORTED_AUDIO_PACKET_DURATIONS_MS.contains(&packet_duration_ms) {
		Ok(())
	} else {
		Err(format!(
			"unsupported audio packet duration {packet_duration_ms} ms (expected 5 or 10)"
		))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn display_modes_admit_high_end_and_reject_degenerate_values() {
		for mode in [
			"1280x720x60",
			"1920x1080x144",
			"2560x1440x240",
			"3840x2160x120",
			"3840x2160x240",
			"5120x1440x240",
			"7680x4320x60",
			"16384x16384x1",
			"1x1x1",
			"1920x1080x1000",
			"1920x1080x2147483",
		] {
			assert!(DisplayMode::parse(mode).is_ok(), "{mode}");
		}
		for mode in [
			"",
			"1920x1080",
			"1920x1080x60x1",
			"0x1080x60",
			"1920x0x60",
			"1920x1080x0",
			"16385x1080x60",
			"1920x16385x60",
			"1920x1080x2147484",
			"4294967295x4294967295x4294967295",
			"4294967296x1080x60",
			"-1920x1080x60",
			"1920x1080x60.0",
			" 1920x1080x60",
		] {
			assert!(DisplayMode::parse(mode).is_err(), "{mode}");
		}
		// The compositor's millihertz conversion stays representable.
		assert!(i32::try_from(MAX_REFRESH_RATE_HZ * 1000).is_ok());
	}

	#[test]
	fn surround_audio_info_requires_a_supported_channel_count() {
		assert_eq!(surround_audio_info("196610"), Ok((AudioChannels::Stereo, 0x3)));
		assert_eq!(
			surround_audio_info(&((0x3f << 16) | 6).to_string()),
			Ok((AudioChannels::Surround51, 0x3f))
		);
		assert_eq!(
			surround_audio_info(&((0x63f << 16) | 8).to_string()),
			Ok((AudioChannels::Surround71, 0x63f))
		);
		for value in ["0", "1", "3", "-1", "x", "4294967296"] {
			assert!(surround_audio_info(value).is_err(), "{value}");
		}
		// The count is the whole low 16 bits, not a truncated byte: 0x102 is not 2.
		assert!(surround_audio_info(&0x0003_0102u32.to_string()).is_err());
	}

	#[test]
	fn only_implemented_audio_durations_are_accepted() {
		assert!(validate_audio_packet_duration(5).is_ok());
		assert!(validate_audio_packet_duration(10).is_ok());
		for duration in [0, 1, 2, 20, 40, u32::MAX] {
			assert!(validate_audio_packet_duration(duration).is_err(), "{duration}");
		}
	}
}
