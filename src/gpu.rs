//! The Vulkan instances Moonshine keeps for the whole process.
//!
//! Each session renders with ferriscope and encodes with pixelforge, and both
//! need a Vulkan instance. Creating them for every session makes the Vulkan
//! loader unload the driver after each session and load it again for the next
//! one. That costs a driver initialisation each time, and NVIDIA's driver does
//! not survive it: `libnvidia-tls.so` uses static TLS, which glibc does not give
//! back on unload, so after about 26 reloads `vkCreateInstance` fails with
//! `VK_ERROR_INCOMPATIBLE_DRIVER` (#265).
//!
//! So Moonshine creates one instance for each and keeps them. Each session still
//! gets devices of its own: the compositor creates its renderer, and the encoder
//! a new video context, on the shared instances. Destroying a session's devices
//! frees everything they held. The health checks use the same instances.

use std::sync::{Arc, Mutex, PoisonError};

use pixelforge::{VideoContext, VideoContextBuilder, VideoInstance};

/// The process-wide Vulkan instances. Cloning is cheap; clones share them.
///
/// Each instance is created on first use and then kept. A creation that fails
/// is not kept, so the next use tries again.
#[derive(Clone, Default)]
pub struct GpuContext {
	inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
	compositor_instance: Mutex<Option<ferriscope::VulkanInstance>>,
	video_instance: Mutex<Option<VideoInstance>>,
}

/// The settings of every video context (and of the instance they share).
fn video_builder() -> VideoContextBuilder {
	VideoContextBuilder::new().app_name("Moonshine")
}

impl GpuContext {
	pub fn new() -> Self {
		Self::default()
	}

	/// The Vulkan instance the compositors render with.
	pub fn compositor_instance(&self) -> Result<ferriscope::VulkanInstance, String> {
		let mut instance = self
			.inner
			.compositor_instance
			.lock()
			.unwrap_or_else(PoisonError::into_inner);
		if let Some(instance) = instance.as_ref() {
			return Ok(instance.clone());
		}
		let created = ferriscope::VulkanInstance::new().map_err(|e| e.to_string())?;
		tracing::debug!("Created the compositor's Vulkan instance.");
		Ok(instance.insert(created).clone())
	}

	/// A new video context, with a device of its own, on the shared instance.
	///
	/// Each stream gets its own: dropping it destroys the device and everything
	/// the device still holds.
	pub fn video_context(&self) -> Result<VideoContext, String> {
		let instance = {
			let mut instance = self.inner.video_instance.lock().unwrap_or_else(PoisonError::into_inner);
			match instance.as_ref() {
				Some(instance) => instance.clone(),
				None => {
					let created = video_builder().build_instance().map_err(|e| e.to_string())?;
					tracing::debug!("Created the encoder's Vulkan instance.");
					instance.insert(created).clone()
				},
			}
		};
		video_builder().build_on(&instance).map_err(|e| e.to_string())
	}
}
