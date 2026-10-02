//! Capture/encode identity contract. Pixelforge v0.9.1 has no device selector;
//! never infer identity from enumeration order, vendor IDs, or device names.
use ash::vk;
use ash::vk::TaggedStructure;
use pixelforge::{VideoContext, VideoContextBuilder};
use std::ffi::CStr;
use std::fs::File;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureIdentity {
	render: (i64, i64),
	pci: Option<(u32, u32, u32, u32)>,
}

impl CaptureIdentity {
	fn from_file(file: &File) -> Result<Self, String> {
		let metadata = file
			.metadata()
			.map_err(|e| format!("Cannot stat capture DRM device: {e}"))?;
		if !metadata.file_type().is_char_device() {
			return Err("Capture render node must be a DRM character device".into());
		}
		let render = (libc::major(metadata.rdev()) as i64, libc::minor(metadata.rdev()) as i64);
		// Resolve by device number, so /dev/dri/by-path and custom symlinks work.
		let pci = std::fs::canonicalize(format!("/sys/dev/char/{}:{}/device", render.0, render.1))
			.ok()
			.and_then(|p| p.file_name()?.to_str().and_then(parse_pci));
		Ok(Self { render, pci })
	}

	fn matches(&self, drm: Option<(i64, i64)>, pci: Option<(u32, u32, u32, u32)>) -> bool {
		// DRM is authoritative if advertised. PCI is a fallback for drivers
		// lacking VK_EXT_physical_device_drm, never a vendor/device-ID guess.
		match drm {
			Some(render) => self.render == render,
			None => self.pci.is_some() && self.pci == pci,
		}
	}
}

fn parse_pci(address: &str) -> Option<(u32, u32, u32, u32)> {
	let fields: Vec<_> = address.split([':', '.']).collect();
	if fields.len() != 4 {
		return None;
	}
	Some((
		u32::from_str_radix(fields[0], 16).ok()?,
		u32::from_str_radix(fields[1], 16).ok()?,
		u32::from_str_radix(fields[2], 16).ok()?,
		u32::from_str_radix(fields[3], 16).ok()?,
	))
}

/// Build and verify against the *opened* DRM node used for GBM/EGL. All
/// encoder/import contexts in a session clone this verified context.
pub(crate) fn capture_context(file: &File) -> Result<VideoContext, String> {
	let capture = CaptureIdentity::from_file(file)?;
	let context = VideoContextBuilder::new()
		.app_name("Pyroshine")
		.build()
		.map_err(|e| format!("Failed to initialize Vulkan: {e}"))?;
	let extensions = unsafe {
		context
			.instance()
			.enumerate_device_extension_properties(context.physical_device())
	}
	.map_err(|e| format!("Cannot query Vulkan device identity extensions: {e}"))?;
	let has = |name: &CStr| {
		extensions
			.iter()
			.any(|ext| unsafe { CStr::from_ptr(ext.extension_name.as_ptr()) } == name)
	};
	let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
	let mut pci = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
	let mut ids = vk::PhysicalDeviceIDProperties::default();
	let mut props = vk::PhysicalDeviceProperties2::default().push(&mut ids);
	if has(ash::ext::physical_device_drm::NAME) {
		props = props.push(&mut drm);
	}
	if has(ash::ext::pci_bus_info::NAME) {
		props = props.push(&mut pci);
	}
	// SAFETY: supported property structures and the live context remain valid.
	unsafe {
		context
			.instance()
			.get_physical_device_properties2(context.physical_device(), &mut props);
	}
	let name = unsafe { CStr::from_ptr(props.properties.device_name.as_ptr()) }.to_string_lossy();
	let drm_id = (drm.has_render != 0).then_some((drm.render_major, drm.render_minor));
	let pci_id =
		has(ash::ext::pci_bus_info::NAME).then_some((pci.pci_domain, pci.pci_bus, pci.pci_device, pci.pci_function));
	if !capture.matches(drm_id, pci_id) {
		return Err(format!(
			"Capture DRM {}:{} (PCI {:?}) does not match or cannot be verified against Vulkan '{name}' (DRM {drm_id:?}, PCI {pci_id:?}). Cross-device capture/encoding is not validated. Set compositor.gpu or MOONSHINE_RENDER_NODE to this Vulkan GPU's render node, or configure the Vulkan loader/ICD selection to expose the capture GPU. Pixelforge v0.9.1 has no device-selector API; no cross-GPU fallback is attempted.",
			capture.render.0, capture.render.1, capture.pci
		));
	}
	tracing::info!(capture_drm = ?capture.render, pci = ?capture.pci, device_uuid = ?ids.device_uuid, gpu = %name,
		"Verified capture/encode/import GPU identity (same device)");
	Ok(context)
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn drm_identity_overrides_enumeration_and_pci_guessing() {
		let capture = CaptureIdentity {
			render: (226, 129),
			pci: Some((0, 3, 0, 0)),
		};
		assert!(!capture.matches(Some((226, 128)), capture.pci));
		assert!(capture.matches(Some((226, 129)), None));
		assert!(capture.matches(None, Some((0, 3, 0, 0))));
		assert!(!capture.matches(None, Some((0, 4, 0, 0))));
		assert!(!capture.matches(None, None));
		assert!(
			!CaptureIdentity {
				render: (226, 129),
				pci: None
			}
			.matches(None, None)
		);
	}
	#[test]
	fn pci_address_includes_domain_and_function() {
		assert_eq!(parse_pci("0001:0a:02.3"), Some((1, 10, 2, 3)));
		assert_eq!(parse_pci("not-pci"), None);
		assert_eq!(parse_pci("0000:03:00.g"), None);
	}
	#[test]
	fn regular_files_cannot_masquerade_as_render_nodes() {
		let file = tempfile::tempfile().unwrap();
		assert!(CaptureIdentity::from_file(&file).is_err());
	}
}
