//! GPU stat readers — small helpers for reading AMD GPU sysfs nodes.
//!
//! Used only by the `bench` harness's developer-facing summary table.
//! AMD-only by design: no portable cross-vendor story, so we don't expose
//! GPU stats via the public OTel metrics surface.

use std::path::{Path, PathBuf};

const AMD_VENDOR_ID: &str = "0x1002";

/// Find the first AMD card under `/sys/class/drm`, ignoring connector
/// entries like `card1-DP-1`. Returns the card's sysfs root, e.g.
/// `/sys/class/drm/card1`.
pub fn auto_detect_amd_card() -> Option<PathBuf> {
	let entries = std::fs::read_dir("/sys/class/drm").ok()?;
	for entry in entries.flatten() {
		let file_name = entry.file_name();
		let name = file_name.to_string_lossy();
		// Match `card<N>` exactly — skip connector entries like `card1-DP-1`.
		if !name.starts_with("card") || !name[4..].chars().all(|c| c.is_ascii_digit()) {
			continue;
		}
		let vendor_path = entry.path().join("device/vendor");
		if let Ok(vendor) = std::fs::read_to_string(&vendor_path) {
			if vendor.trim() == AMD_VENDOR_ID {
				return Some(entry.path());
			}
		}
	}
	None
}

/// Read `pp_dpm_sclk` and return the currently-active clock in MHz (the
/// line containing `*`). `None` on any parse error.
pub fn read_active_sclk_mhz(path: &Path) -> Option<u32> {
	let content = std::fs::read_to_string(path).ok()?;
	for line in content.lines() {
		if !line.contains('*') {
			continue;
		}
		// Lines look like: "1: 1330Mhz *"
		let after_colon = line.split_once(':')?.1.trim().trim_end_matches('*').trim();
		// Strip trailing "Mhz" / "MHz".
		let mhz = after_colon.trim_end_matches(|c: char| c.is_alphabetic()).trim();
		return mhz.parse().ok();
	}
	None
}

/// Read `gpu_busy_percent` (0–100). `None` on any parse error.
pub fn read_busy_percent(path: &Path) -> Option<u8> {
	std::fs::read_to_string(path).ok()?.trim().parse().ok()
}
