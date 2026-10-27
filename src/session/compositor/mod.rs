//! Embedded headless compositor for Moonshine, backed by `ferriscope`.
//!
//! ferriscope runs the Wayland/Xwayland server, the render loop and the DMA-BUF
//! pool. This module turns Moonshine's configuration and the client's session
//! into ferriscope's, launches it, and hands its frame receiver to the video
//! pipeline and its input sender to the control stream.
//!
//! The streams are created before the compositor is launched, so they get
//! `CompositorHandles` whose endpoints are filled in by `Compositor::launch`.

pub(crate) mod frame;
pub(crate) mod input;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_shutdown::ShutdownManager;
use ferriscope::{Compositor as FerriscopeCompositor, FrameReceiver};
use serde::{Deserialize, Serialize};

use crate::session::SessionContext;
use crate::session::manager::SessionShutdownReason;

use self::input::CompositorInput;

/// How long [`Compositor::launch`] waits for ferriscope's readiness handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Keyboard configuration for the compositor's XKB state.
///
/// Passed to ferriscope as the seat keymap; Steam's `GAMESCOPE_KEYBOARD_LAYOUT`
/// overrides the layout and variant at runtime, as in gamescope.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct KeyboardConfig {
	pub layout: String,
	pub variant: String,
	pub model: String,
	pub options: Option<String>,
}

impl Default for KeyboardConfig {
	fn default() -> Self {
		Self {
			layout: "us".to_string(),
			variant: String::new(),
			model: String::new(),
			options: None,
		}
	}
}

impl From<&KeyboardConfig> for ferriscope::KeyboardConfig {
	fn from(config: &KeyboardConfig) -> Self {
		Self {
			rules: String::new(),
			model: config.model.clone(),
			layout: config.layout.clone(),
			variant: config.variant.clone(),
			options: config.options.clone().unwrap_or_default(),
		}
	}
}

/// Configuration for the embedded headless compositor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CompositorConfig {
	/// Optional GPU device identifier for compositor rendering.
	pub gpu: Option<String>,

	/// Whether to enable HDR mode in the compositor if the client supports it.
	pub hdr: bool,

	/// Keyboard configuration for the compositor's XKB state.
	pub keyboard: KeyboardConfig,

	/// DMA-BUF frames the consumer (the encoder) may hold at once; ferriscope
	/// renders only into a free slot, so this bounds how far the hosted
	/// application may run ahead of the encoder.
	///
	/// One frame ties the application's throughput to a single
	/// encoder/network round-trip, so with a real Moonlight client the app is
	/// held to `1 / latency`. The legacy `moonshine-main` compositor decoupled
	/// the app from the encoder with three buffers, so three is the parity
	/// value. Each extra frame allows up to one extra frame of end-to-end
	/// latency.
	pub pool_size: usize,
}

impl Default for CompositorConfig {
	fn default() -> Self {
		Self {
			gpu: None,
			hdr: true,
			keyboard: KeyboardConfig::default(),
			pool_size: 3,
		}
	}
}

/// Runtime context derived from the client's session request.
pub(crate) struct CompositorContext {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
	/// The application's Steam integration (gamescope's `-e`).
	pub steam_integration: bool,
}

/// Information sent from the compositor thread once XWayland is ready.
pub(crate) struct CompositorReady {
	/// X11 display number (e.g. `:1` → `1`).
	pub xdisplay: u32,
	/// Wayland socket name for the session compositor.
	pub wayland_display: String,
	/// Whether HDR mode is active for this session.
	pub hdr: bool,
	/// Environment the hosted application needs (ferriscope's `Ready::env`):
	/// the gamescope identity Steam keys on, the ferriscope-wsi layer switch,
	/// and the HDR and capability variables for this session.
	pub env: Vec<(String, String)>,
}

/// The composited frames, available once the compositor has launched.
///
/// Dropping a received frame returns its buffer to the compositor.
pub(crate) type CompositorFrames = Arc<OnceLock<FrameReceiver>>;

/// Handles returned by `Compositor::new()` for wiring into streams.
pub(crate) struct CompositorHandles {
	pub frames: CompositorFrames,
	pub input: CompositorInput,
}

/// Unlaunched compositor — holds the handles' endpoints, can only be launched.
pub(crate) struct Compositor {
	config: CompositorConfig,
	context: CompositorContext,
	stop: ShutdownManager<SessionShutdownReason>,
	handles: CompositorHandles,
}

/// Launched compositor — can be queried, cannot be launched again.
///
/// This is a short-lived "ready" handle: Moonshine drops it once the display
/// names have been read, so it deliberately does not own the compositor. The
/// ferriscope compositor is owned by a supervisor thread that stops it when the
/// session shuts down.
pub(crate) struct LaunchedCompositor {
	ready: CompositorReady,
}

impl From<&SessionContext> for CompositorContext {
	fn from(ctx: &SessionContext) -> Self {
		Self {
			width: ctx.resolution.0,
			height: ctx.resolution.1,
			refresh_rate: ctx.refresh_rate,
			hdr: ctx.hdr,
			steam_integration: ctx.application.steam_integration,
		}
	}
}

/// Build the `ferriscope` config for one session.
///
/// Moonshine spawns the hosted application itself (`Application::spawn`) once
/// the display is ready, so ferriscope must not spawn one (`command: None`).
/// `steam` follows the application's `steam_integration`, which also selects the Steam-controlled focus strategy, as
/// gamescope's `-e` does (`main.cpp:787`); without it Steam sizes Big Picture to
/// a default 1280x800 and never marks its overlay, so ferriscope would
/// composite it as a plain window at the origin.
fn build_ferriscope_config(
	config: &CompositorConfig,
	context: &CompositorContext,
	render_node: std::path::PathBuf,
) -> ferriscope::CompositorConfig {
	let steam = context.steam_integration;
	ferriscope::CompositorConfig {
		width: context.width,
		height: context.height,
		rate: context.refresh_rate as f64,
		render_node,
		xwayland: true,
		command: None,
		steam,
		// HDR output needs both the host configuration and a client that can
		// display it.
		hdr: config.hdr && context.hdr,
		seat: ferriscope::SeatConfig {
			keyboard: (&config.keyboard).into(),
			..ferriscope::SeatConfig::default()
		},
		output: ferriscope::FrameOutput::Channel(ferriscope::ChannelOutput {
			pool_size: config.pool_size.max(1),
		}),
		..ferriscope::CompositorConfig::default()
	}
}

impl Compositor {
	pub fn new(
		config: CompositorConfig,
		context: CompositorContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> (Self, CompositorHandles) {
		let handles = CompositorHandles {
			frames: CompositorFrames::default(),
			input: CompositorInput::new(context.width, context.height),
		};
		let ours = CompositorHandles {
			frames: Arc::clone(&handles.frames),
			input: handles.input.clone(),
		};
		(
			Self {
				config,
				context,
				stop,
				handles: ours,
			},
			handles,
		)
	}

	pub fn launch(self) -> Result<LaunchedCompositor, ()> {
		let Self {
			config,
			context,
			stop,
			handles,
		} = self;

		// Resolve the render node before starting ferriscope so a missing GPU is
		// reported as a clean launch failure.
		let render_node = crate::healthcheck::find_render_node(&config.gpu).map_err(|e| {
			tracing::error!("Failed to find render node: {e}");
		})?;

		let ferriscope_config = build_ferriscope_config(&config, &context, render_node);
		tracing::info!(
			steam = ferriscope_config.steam,
			width = ferriscope_config.width,
			height = ferriscope_config.height,
			"launching ferriscope compositor for session"
		);

		let (compositor, ferriscope_handles) = FerriscopeCompositor::launch(ferriscope_config).map_err(|e| {
			tracing::error!("Failed to launch ferriscope compositor: {e}");
		})?;

		let ferriscope_ready = match ferriscope_handles.ready.recv_timeout(READY_TIMEOUT) {
			Ok(ready) => ready,
			Err(e) => {
				tracing::warn!("Timed out waiting for compositor ready: {e}");
				let _ = compositor.stop();
				return Err(());
			},
		};

		// A dropped frame returns its buffer through its own release guard, so
		// the explicit release channel is not needed.
		if handles.frames.set(ferriscope_handles.frames).is_err() {
			tracing::error!("Compositor was launched twice");
			let _ = compositor.stop();
			return Err(());
		}
		handles.input.connect(ferriscope_handles.input);

		// `LaunchedCompositor` is a short-lived "ready" handle in Moonshine: it
		// is dropped once the display names have been read, so it must not own
		// the compositor. Hand ferriscope to a supervisor thread that blocks
		// until it exits, and ends the session if ferriscope stops on its own.
		// A task on the session's runtime stops ferriscope when the session
		// shuts down.
		//
		// The supervisor holds a delay token until ferriscope has fully stopped,
		// so the session's shutdown (and `stop_session`) waits for it. Without
		// it Moonshine could exit while ferriscope's thread was still tearing
		// down its renderer (with GL, NVIDIA's EGL exit handler crashed it).
		let delay = stop.delay_shutdown_token().map_err(|e| {
			tracing::error!("Failed to hold the session open for the compositor: {e:?}");
		})?;
		let stop_handle = compositor.stop_handle();
		let session_stop = stop.clone();
		std::thread::Builder::new()
			.name("compositor-supervisor".to_string())
			.spawn(move || {
				match compositor.wait() {
					Ok(()) => tracing::debug!("Compositor exited"),
					Err(e) => tracing::warn!("Compositor exited with an error: {e}"),
				}
				// If the session is not already shutting down, ferriscope stopped
				// on its own.
				if stop.trigger_shutdown(SessionShutdownReason::CompositorStopped).is_ok() {
					tracing::info!("Compositor stopped");
				}
				drop(delay);
			})
			.map_err(|e| {
				tracing::error!("Failed to spawn compositor supervisor: {e}");
			})?;
		tokio::spawn(async move {
			session_stop.wait_shutdown_triggered().await;
			stop_handle.stop();
		});

		let ready = CompositorReady {
			xdisplay: ferriscope_ready.xdisplay.unwrap_or(0),
			wayland_display: ferriscope_ready.wayland_display,
			hdr: ferriscope_ready.hdr,
			env: ferriscope_ready.env,
		};

		Ok(LaunchedCompositor { ready })
	}
}

impl LaunchedCompositor {
	pub fn ready(&self) -> &CompositorReady {
		&self.ready
	}
	pub fn hdr(&self) -> bool {
		self.ready.hdr
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn context() -> CompositorContext {
		CompositorContext {
			width: 2424,
			height: 1080,
			refresh_rate: 60,
			hdr: false,
			steam_integration: false,
		}
	}

	/// Regression: the Steam overlay rendered in the left letterbox under
	/// Moonshine+ferriscope because the adapter left ferriscope's Steam
	/// integration off. Steam then never tagged its Big Picture window
	/// `STEAM_OVERLAY` and kept a 1280x800 default, so ferriscope composited it
	/// as an unscaled window at the origin. Steam integration must follow the
	/// application's `steam_integration`, and select Steam-controlled focus.
	///
	/// A nested desktop, in turn, gets no keyboard with Steam integration on:
	/// only Steam and Steam game windows may take focus.
	#[test]
	fn the_application_decides_steam_integration() {
		let config = CompositorConfig::default();
		let render_node = std::path::PathBuf::from("/dev/dri/renderD128");

		let forwarded = build_ferriscope_config(&config, &context(), render_node.clone());
		assert!(!forwarded.steam, "Steam integration is off by default");

		let on = CompositorContext {
			steam_integration: true,
			..context()
		};
		let forwarded = build_ferriscope_config(&config, &on, render_node);
		assert!(forwarded.steam);
	}

	#[test]
	fn the_ferriscope_config_matches_the_session() {
		let config = CompositorConfig::default();
		let forwarded = build_ferriscope_config(&config, &context(), std::path::PathBuf::from("/dev/dri/renderD128"));
		assert_eq!(forwarded.width, 2424);
		assert_eq!(forwarded.height, 1080);
		assert_eq!(forwarded.rate, 60.0);
		assert!(forwarded.xwayland);
		assert!(matches!(
			&forwarded.output,
			ferriscope::FrameOutput::Channel(c) if c.pool_size == 3
		));
		assert!(forwarded.command.is_none(), "Moonshine spawns the app, not ferriscope");
	}

	/// Regression: the adapter must not leave ferriscope at a one-frame pool,
	/// which caps the hosted app at `1 / consumer_latency`. Three matches the
	/// legacy compositor's three-buffer decoupling.
	#[test]
	fn the_pool_defaults_to_three_frames_and_holds_at_least_one() {
		let pool =
			|config: &CompositorConfig| match build_ferriscope_config(config, &context(), "/dev/dri/renderD128".into())
				.output
			{
				ferriscope::FrameOutput::Channel(channel) => channel.pool_size,
				output => panic!("unexpected output {output:?}"),
			};
		assert_eq!(pool(&CompositorConfig::default()), 3);
		let empty = CompositorConfig {
			pool_size: 0,
			..CompositorConfig::default()
		};
		assert_eq!(pool(&empty), 1);
	}
}
