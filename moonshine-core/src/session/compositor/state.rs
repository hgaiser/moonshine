//! Compositor state and Smithay protocol handler implementations.
//!
//! `MoonshineCompositor` is the central state struct for the headless compositor.
//! All Smithay `delegate_*!` macros target this struct.

use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::GbmAllocator;
use smithay::backend::allocator::{Allocator, Buffer, Fourcc, Modifier};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement, RescaleRenderElement};
use smithay::backend::renderer::element::{AsRenderElements, Element, Id, Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions, with_renderer_surface_state};
use smithay::backend::renderer::{Bind, BufferType, ImportDma, Renderer};
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::utils::send_frames_surface_tree;
use smithay::desktop::utils::{OutputPresentationFeedback, take_presentation_feedback_surface_tree};
use std::collections::HashMap;

use smithay::backend::input::InputTime;
use smithay::desktop::Space;
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::input::tablet::{TabletDescriptor, TabletSeatTrait};
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Clock, IsAlive, Logical, Monotonic, Point};
use smithay::wayland::compositor;
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::dmabuf::{self, DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::presentation::Refresh;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Wm;

use super::KeyboardConfig;
use super::capture::DirectReject;
use crate::session::compositor::cursor::{self, PointerElement, PointerRenderElement};
use crate::session::compositor::frame::{ExportedFrame, ExportedPlane, FrameColorSpace, HdrMetadata};

/// Number of pre-allocated GBM buffers. Conventional encoders may retain
/// multiple submitted buffers; PyroWave admits only one capture at a time.
/// This pool is buffer lifetime storage, not a frame delivery queue.
const BUFFER_POOL_SIZE: usize = 3;

/// Visual layers in back-to-front order; independent of input targets.
struct SceneLayers<'a> {
	decorations: &'a [smithay::desktop::Window],
	upper: [Option<&'a smithay::desktop::Window>; 5],
}

/// A pre-allocated GBM buffer slot in the compositor's buffer pool.
pub(crate) struct GbmBufferSlot {
	/// The exported DMA-BUF kept alive for the lifetime of the pool.
	dmabuf: Dmabuf,
	/// Shared with the encoder — `true` means the encoder is done reading
	/// and the compositor may render into this buffer again.
	consumed: Arc<AtomicBool>,
}

// Combined render element type for compositing space + cursor elements.
// We use GlesRenderer concretely (no generics) to avoid complex trait bound issues.
pub(crate) enum OutputRenderElements {
	Space(SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>),
	/// A root swapchain that explicitly declared VK_COMPOSITE_ALPHA_OPAQUE.
	OpaqueSpace(SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>),
	Pointer(PointerRenderElement<GlesRenderer>),
}

impl Element for OutputRenderElements {
	fn id(&self) -> &Id {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.id(),
			Self::Pointer(e) => e.id(),
		}
	}

	fn current_commit(&self) -> CommitCounter {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.current_commit(),
			Self::Pointer(e) => e.current_commit(),
		}
	}

	fn geometry(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Rectangle<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.geometry(scale),
			Self::Pointer(e) => e.geometry(scale),
		}
	}

	fn src(&self) -> smithay::utils::Rectangle<f64, smithay::utils::Buffer> {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.src(),
			Self::Pointer(e) => e.src(),
		}
	}

	fn location(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Point<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.location(scale),
			Self::Pointer(e) => e.location(scale),
		}
	}

	fn transform(&self) -> smithay::utils::Transform {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.transform(),
			Self::Pointer(e) => e.transform(),
		}
	}

	fn damage_since(
		&self,
		scale: smithay::utils::Scale<f64>,
		commit: Option<CommitCounter>,
	) -> DamageSet<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.damage_since(scale, commit),
			Self::Pointer(e) => e.damage_since(scale, commit),
		}
	}

	fn opaque_regions(&self, scale: smithay::utils::Scale<f64>) -> OpaqueRegions<i32, smithay::utils::Physical> {
		match self {
			Self::OpaqueSpace(e) if e.alpha() == 1.0 => {
				std::iter::once(smithay::utils::Rectangle::from_size(e.geometry(scale).size)).collect()
			},
			Self::Space(e) | Self::OpaqueSpace(e) => e.opaque_regions(scale),
			Self::Pointer(e) => e.opaque_regions(scale),
		}
	}

	fn alpha(&self) -> f32 {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.alpha(),
			Self::Pointer(e) => e.alpha(),
		}
	}

	fn kind(&self) -> smithay::backend::renderer::element::Kind {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.kind(),
			Self::Pointer(e) => e.kind(),
		}
	}
}

impl RenderElement<GlesRenderer> for OutputRenderElements {
	fn draw(
		&self,
		frame: &mut GlesFrame<'_, '_>,
		src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
		dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
		damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
		opaque_regions: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
		cache: Option<&smithay::utils::user_data::UserDataMap>,
	) -> Result<(), GlesError> {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.draw(frame, src, dst, damage, opaque_regions, cache),
			Self::Pointer(e) => e.draw(frame, src, dst, damage, opaque_regions, cache),
		}
	}

	fn underlying_storage(
		&self,
		renderer: &mut GlesRenderer,
	) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
		match self {
			Self::Space(e) | Self::OpaqueSpace(e) => e.underlying_storage(renderer),
			Self::Pointer(e) => e.underlying_storage(renderer),
		}
	}
}

impl From<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> for OutputRenderElements {
	fn from(e: SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>) -> Self {
		Self::Space(e)
	}
}

impl From<PointerRenderElement<GlesRenderer>> for OutputRenderElements {
	fn from(e: PointerRenderElement<GlesRenderer>) -> Self {
		Self::Pointer(e)
	}
}

/// A composed element with an output scaling and centering transform applied.
type ScaledOutputElement = RelocateRenderElement<RescaleRenderElement<OutputRenderElements>>;

/// Central compositor state for Moonshine's headless compositor.
///
/// Runs on a dedicated calloop thread. All Smithay delegate_*! macros
/// target this struct. No physical display — frames are rendered to
/// GBM buffers and exported to the video encoder.
#[allow(dead_code)]
pub(crate) struct MoonshineCompositor {
	// -- Wayland plumbing --
	pub display_handle: DisplayHandle,
	pub compositor_state: CompositorState,
	pub shm_state: ShmState,
	pub xdg_shell_state: XdgShellState,
	pub seat_state: SeatState<Self>,
	pub output_manager_state: OutputManagerState,
	pub data_device_state: DataDeviceState,
	/// Wayland `xdg-activation` state, the Wayland analog of X11's
	/// `_NET_ACTIVE_WINDOW`: clients request focus through it.
	pub activation_state: XdgActivationState,
	/// Solid-color buffers used by nested compositors such as KWin.
	pub single_pixel_buffer_state: SinglePixelBufferState,

	// -- Rendering --
	pub output: Output,
	pub damage_tracker: OutputDamageTracker,
	pub allocator: GbmAllocator<std::fs::File>,
	pub renderer: GlesRenderer,

	// -- DMA-BUF --
	pub dmabuf_state: DmabufState,
	pub dmabuf_global: DmabufGlobal,

	// -- Frame relay to encoder --
	pub frame_tx: super::admission::CaptureSender,
	pub(super) capture_failed: bool,
	pub(super) next_capture_at: std::time::Instant,
	/// Shared refresh grid for callbacks and receiver-driven capture.
	pub(super) next_refresh_at: std::time::Instant,
	max_capture_lateness_us: u64,
	missed_capture_slots: u64,

	// -- Input --
	pub seat: Seat<Self>,
	/// Clipboard text queued by `TypeText` events, typed in bounded batches on
	/// successive frame ticks. See [`crate::session::compositor::input`].
	pub pending_text: String,

	// -- Cursor --
	pub cursor_position: Point<f64, Logical>,
	pub cursor: super::cursor::CursorState,
	pub pointer_element: PointerElement,
    pub capture_mode: super::CaptureMode,
    capture_path: Option<&'static str>,
	last_resource_summary: std::time::Instant,
	pub(super) log_stats: bool,
	released_scanout_buffers: u64,
	captured_frames: u64,
	pre_render_rejected: u64,
	stale_after_render: u64,
	direct_frames: u64,
	composited_frames: u64,
	direct_rejections: [u64; DirectReject::COUNT],
	gpu_timer: super::gpu_timing::GpuTimer,
	pub pen_tablet_descriptor: TabletDescriptor,
	pub active_pen_tool_kind: Option<u8>,
	pub pen_buttons: u8,

	// -- Desktop --
	pub space: Space<smithay::desktop::Window>,
    pub popups: smithay::desktop::PopupManager,
	pub clock: Clock<Monotonic>,

	// -- Lifecycle --
	pub handle: LoopHandle<'static, Self>,

	// -- Frame dimensions --
	pub width: u32,
	pub height: u32,

	// -- Render format --
	pub render_fourcc: Fourcc,
	pub render_modifiers: Vec<Modifier>,
	sdr_render_format: (Fourcc, Vec<Modifier>),
	hdr_render_format: Option<(Fourcc, Vec<Modifier>)>,

	// -- Buffer pool --
	pub(crate) buffer_pool: Vec<GbmBufferSlot>,
	/// Pools retired by a live resolution change. They remain alive until the
	/// encoder releases every exported DMA-BUF from the old stream epoch.
	retired_buffer_pools: Vec<Vec<GbmBufferSlot>>,
	pub next_buffer_index: usize,
	/// Per-buffer render count for damage tracking.  `None` means the buffer
	/// has never been rendered to yet (age = 0 → full redraw).
	pub buffer_last_rendered_at: [Option<usize>; BUFFER_POOL_SIZE],
	/// Monotonically increasing render counter.
	pub render_count: usize,

	// -- Static screen detection --
	/// Set to `true` whenever visible content changes (surface commit, cursor
	/// move). Cleared after a frame is sent. When false and a frame was sent
	/// less than 1 second ago, rendering is skipped to save GPU/CPU/bandwidth.
	pub screen_dirty: bool,
	/// Timestamp of the last frame that was actually sent to the encoder.
	pub last_frame_sent_at: std::time::Instant,
	/// Set when a STEAM_OVERLAY property-notify arrives, so the overlay z-order
	/// is re-evaluated immediately (see update_overlay_z_order).
	pub overlay_dirty: bool,
	/// Cached cursor position from the last sent frame, to detect cursor-only
	/// changes without a surface commit.
	pub last_cursor_position: Point<f64, Logical>,

	// -- Steam overlay z-order --
	/// True while Steam requests interactive overlay input.
	pub steam_overlay_input_active: bool,

	// -- Extended protocols --
	pub viewporter_state: smithay::wayland::viewporter::ViewporterState,

	// -- HDR / Color Management --
	/// Color management protocol state (wp_color_management_v1).
	/// Present when HDR mode is active.
	pub color_management: Option<super::color_management::ColorManagementState>,

	/// Deferred destructor events for wp_image_description_info_v1.
	///
	/// The `done()` event is a destructor that removes the object from
	/// wayland-backend's map. Sending it inside a `Dispatch::request`
	/// handler would panic because the backend tries to set user_data on
	/// the (now-deleted) child object after the handler returns. We
	/// collect them here and drain them right after `dispatch_clients`.
	pub deferred_info_done: Vec<smithay::reexports::wayland_protocols::wp::color_management::v1::server::wp_image_description_info_v1::WpImageDescriptionInfoV1>,

	// -- XWayland --
	pub xwayland_shell_state: XWaylandShellState,
	pub xwm: Option<X11Wm>,
	pub xdisplay: Option<u32>,
	/// Channel to notify the session thread of the XWayland display number
	/// once it becomes ready.
	pub xdisplay_tx: Option<mpsc::SyncSender<super::CompositorReady>>,
	/// Registration token for the compositor's Wayland listening socket.
	pub wayland_socket_token: Option<RegistrationToken>,
	/// Registration token for the root-window `PropertyNotify` source that
	/// watches Steam's focus control properties.
	pub x11_focus_token: Option<RegistrationToken>,
	/// Name of the compositor's Wayland socket in XDG_RUNTIME_DIR.
	pub wayland_display: String,

	/// Whether HDR mode is active for this session.
	pub hdr: bool,
	pub hdr_capable: bool,

	/// Steam integration mode (gamescope's `-e`). Promotes the connector
	/// strategy to `SteamControlled` and enables Steam window filtering.
	pub steam_mode: bool,

	/// Focus split strategy (gamescope's `backend_virtual_connector_strategy`).
	pub virtual_connector_strategy: super::VirtualConnectorStrategy,

	// -- WSI layer --
	/// WSI override surface `(surface, render_window)`, where `render_window` is
	/// the resolved X11 toplevel of the window the swapchain was created on
	/// (0 for native Wayland).  When set, this surface is rendered instead of
	/// that window's X11 content.
	pub override_surface: Option<(WlSurface, u32)>,

	/// X11 window the WSI layer reported for the current override surface (the
	/// swapchain window, often a child of the rendered toplevel). Kept so the
	/// target can be re-resolved when its window maps later.
	pub override_reported_window: u32,

	/// X11 window ID of the currently focused window (from Smithay's keyboard focus).
	/// Used by the WSI layer to match override surfaces to focused windows.
	pub focused_x11_window: Option<u32>,

	/// The actual Window that currently has keyboard focus (X11 or Wayland).
	/// Used to properly deactivate the old window when focus changes,
	/// especially for Wayland→Wayland transitions where focused_x11_window
	/// would be None for both old and new.
	pub focused_window: Option<smithay::desktop::Window>,
	/// Scaling mode used to fit the focused window to the output.
	pub upscale_scaler: super::scaling::UpscaleScaler,
	/// Texture filter used when scaling.
	pub upscale_filter: smithay::backend::renderer::TextureFilter,
	/// Overscan × magnification scale (`STEAM_SCREEN_SCALE` ×
	/// `STEAM_SCREEN_MAGNIFICATION`), normally 1.0.
	pub global_scale: f64,

	/// Currently active override window (dropdown, menu, tooltip).
	/// Override windows are visually raised and may receive keyboard input
	/// while the primary focus remains on the main game window.
	/// Gamescope: `steamcompmgr_win_t::overrideWindow`
	pub override_window: Option<smithay::desktop::Window>,

	/// Previous override kept painted under a nested popup so it does not blink
	/// out. Gamescope: `focus_t::overrideUnderlayWindow`.
	pub override_underlay_window: Option<smithay::desktop::Window>,

	/// Same-app decoration windows painted above the focus window.
	/// Gamescope: `focus_t::decorationWindows`.
	pub decoration_windows: Vec<smithay::desktop::Window>,

	/// Currently classified interactive Steam overlay window.
	/// Gamescope: `focus_t::overlayWindow` — the main Steam overlay window.
	pub overlay_window: Option<smithay::desktop::Window>,

	/// Currently classified passive Steam notification window.
	/// Gamescope: `focus_t::notificationWindow` — small Steam notification popups.
	pub notification_window: Option<smithay::desktop::Window>,

	/// Currently active external overlay window (e.g., Discord, OBS).
	/// Gamescope: `focus_t::externalOverlayWindow` — non-Steam overlays.
	pub external_overlay_window: Option<smithay::desktop::Window>,

	/// Pointer focus window — where mouse/pointer events are routed.
	/// Separate from keyboard focus when an overlay has `inputFocusMode != 0`.
	/// Gamescope: `focus_t::inputFocusWindow` — where pointer/mouse events go.
	pub pointer_focus_window: Option<smithay::desktop::Window>,

	/// Window that currently receives X input focus (gamescope's
	/// `focus_t::inputFocusWindow`), separate from the presented focus.
	pub input_focus_window: Option<smithay::desktop::Window>,

	/// X11 window ID last focused by the X server (gamescope's
	/// `currentKeyboardFocusWindow`).
	pub current_keyboard_focus_window: Option<u32>,

	/// `STEAM_INPUT_FOCUS` of the input focus window last applied.
	pub input_focus_mode: u32,

	/// Monotonically increasing damage sequence counter. Incremented on each
	/// surface commit for game windows (app_id != 0). Used to detect when
	/// a game window has drawn since the last focus change.
	pub damage_sequence_counter: u64,

	/// Monotonically increasing map sequence counter. Incremented each time
	/// a window is mapped. Used as a tiebreaker in focus priority ranking
	/// (step 10: later-mapped game windows win over earlier ones).
	pub map_sequence_counter: u64,

	/// X11 window ID of the last window that had keyboard focus.
	/// Used for keyboard focus persistence — when a dropdown opens, keyboard
	/// focus stays on this window rather than moving to the dropdown.
	/// Gamescope: tracks keyboard focus separately from primary focus.
	pub last_keyboard_focus_window: Option<u32>,

	/// X11 connection to the XWayland display for reading root window
	/// properties. Used to read Steam's focus control properties.
	pub x11_focus: Option<super::x11_focus::X11Focus>,

	/// Focus dirty-tracking state. Tracks whether focus has changed since
	/// the last time it was applied, avoiding unnecessary recalculation.
	/// Gamescope: `focus_t::ulCurrentFocusSerial` + `MakeFocusDirty()`.
	pub focus_state: super::focus::FocusState,

	/// Metadata for each window, used for focus priority decisions.
	/// Mirrors the fields from `steamcompmgr_win_t` in gamescope.
	pub window_metadata: std::collections::HashMap<smithay::desktop::Window, super::focus::WindowMetadata>,

	/// Maps parent X11 window ID → list of transient child Windows.
	/// Updated at map/unmap time for O(1) child lookup.
	pub transient_children: std::collections::HashMap<u32, Vec<smithay::desktop::Window>>,

	// -- Direct scanout --
	/// Client buffers held alive during direct scanout until the encoder
	/// signals `consumed`. Each entry pairs a consumed flag, the wl_buffer
	/// ObjectId (for `scanout_buffer_map` cleanup), and the cloned Smithay
	/// `Buffer` (keeps wl_buffer from being released).
	held_scanout_buffers: Vec<(
		Arc<AtomicBool>,
		smithay::reexports::wayland_server::backend::ObjectId,
		smithay::backend::renderer::utils::Buffer,
	)>,
	/// Maps wl_buffer ObjectIds to stable buffer indices for pixelforge's
	/// dmabuf import cache. Keying by ObjectId is robust against protocols
	/// that re-duplicate fds per commit (e.g. gamescope_swapchain via
	/// vkd3d-proton); fd-keying produces fresh indices each frame and
	/// effectively bypasses the cache. Indices start at `BUFFER_POOL_SIZE`
	/// to avoid collisions with the GBM pool.
	scanout_buffer_map:
		std::collections::HashMap<smithay::reexports::wayland_server::backend::ObjectId, usize>,
	/// Next available scanout buffer index.
	scanout_next_index: usize,
	/// Last logged direct-scanout buffer parameters (fourcc, modifier,
	/// num_planes, width, height). NVIDIA's Wayland WSI wraps a new
	/// `wl_buffer` (and thus a new buffer index) every frame, so imports are
	/// only logged when these parameters change rather than per buffer.
	last_scanout_buffer_desc: Option<(u32, u64, usize, u32, u32)>,
}

/// Client state required by Smithay's compositor.
pub(crate) struct ClientState {
	pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
	fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}

	fn disconnected(
		&self,
		_client_id: smithay::reexports::wayland_server::backend::ClientId,
		_reason: smithay::reexports::wayland_server::backend::DisconnectReason,
	) {
	}
}

impl MoonshineCompositor {
	/// Apply a client-requested output mode without replacing the compositor or
	/// disconnecting the launched application.
	pub(crate) fn reconfigure_output(
		&mut self,
		width: u32,
		height: u32,
		refresh_rate: u32,
		hdr: bool,
	) -> Result<(), String> {
		if width == 0 || height == 0 || refresh_rate == 0 {
			return Err("output dimensions and refresh rate must be non-zero".to_string());
		}
		if hdr && !self.hdr_capable {
			return Err("HDR was requested but the live compositor is not HDR capable".to_string());
		}

		let (desired_fourcc, desired_modifiers) = if hdr {
			self.hdr_render_format
				.clone()
				.ok_or_else(|| "HDR render format is unavailable".to_string())?
		} else {
			self.sdr_render_format.clone()
		};
		if self.width != width || self.height != height || self.render_fourcc != desired_fourcc {
			let mut replacement = Vec::with_capacity(BUFFER_POOL_SIZE);
			for index in 0..BUFFER_POOL_SIZE {
				let buffer = self
					.allocator
					.create_buffer(width, height, desired_fourcc, &desired_modifiers)
					.map_err(|error| format!("failed to allocate reconfigured GBM buffer {index}: {error}"))?;
				let dmabuf = buffer
					.export()
					.map_err(|error| format!("failed to export reconfigured GBM buffer {index}: {error}"))?;
				replacement.push(GbmBufferSlot {
					dmabuf,
					consumed: Arc::new(AtomicBool::new(true)),
				});
			}
			let retired = std::mem::replace(&mut self.buffer_pool, replacement);
			self.retired_buffer_pools.push(retired);
			self.width = width;
			self.height = height;
			self.render_fourcc = desired_fourcc;
			self.render_modifiers = desired_modifiers;
			self.next_buffer_index = 0;
			self.buffer_last_rendered_at = [None; BUFFER_POOL_SIZE];
			self.scanout_buffer_map.clear();
			self.last_scanout_buffer_desc = None;
		}

		self.hdr = hdr;
		if let Some(color_management) = self.color_management.as_mut() {
			color_management.hdr = hdr;
		}
		let mode = Mode {
			size: (width as i32, height as i32).into(),
			refresh: (refresh_rate * 1000) as i32,
		};
		self.output.change_current_state(Some(mode), None, None, None);
		self.output.set_preferred(mode);
		self.damage_tracker = OutputDamageTracker::from_output(&self.output);
		self.screen_dirty = true;
		self.next_capture_at = self.next_refresh_at;
		tracing::info!(width, height, refresh_rate, hdr, "Reconfigured live compositor output");
		Ok(())
	}

	/// Create a new compositor state.
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		display: Display<Self>,
		display_handle: DisplayHandle,
		handle: LoopHandle<'static, Self>,
		output: Output,
		damage_tracker: OutputDamageTracker,
		mut allocator: GbmAllocator<std::fs::File>,
		renderer: GlesRenderer,
		frame_tx: super::admission::CaptureSender,
		width: u32,
		height: u32,
		render_fourcc: Fourcc,
		render_modifiers: Vec<Modifier>,
		sdr_render_format: (Fourcc, Vec<Modifier>),
		hdr_render_format: Option<(Fourcc, Vec<Modifier>)>,
		xdisplay_tx: mpsc::SyncSender<super::CompositorReady>,
		render_node: &std::path::Path,
		hdr: bool,
		hdr_capable: bool,
		steam_mode: bool,
		virtual_connector_strategy: super::VirtualConnectorStrategy,
		keyboard_config: KeyboardConfig,
		capture_mode: super::CaptureMode,
		log_stats: bool,
	) -> (Self, Display<Self>) {
		let compositor_state = CompositorState::new_v6::<Self>(&display_handle);
		let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
		let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
		let mut seat_state = SeatState::new();
		let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
		let data_device_state = DataDeviceState::new::<Self>(&display_handle);
		let activation_state = XdgActivationState::new::<Self>(&display_handle);
		let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
		let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
		RelativePointerManagerState::new::<Self>(&display_handle);
		PointerConstraintsState::new::<Self>(&display_handle);
		TabletManagerState::new::<Self>(&display_handle);
		let viewporter_state = smithay::wayland::viewporter::ViewporterState::new::<Self>(&display_handle);
		smithay::wayland::presentation::PresentationState::new::<Self>(&display_handle, 1);
		let clock = Clock::new();

		let mut xkb_config = XkbConfig::default();

		if !keyboard_config.layout.is_empty() {
			xkb_config.layout = &keyboard_config.layout;
		}

		if !keyboard_config.variant.is_empty() {
			xkb_config.variant = &keyboard_config.variant;
		}

		if !keyboard_config.model.is_empty() {
			xkb_config.model = &keyboard_config.model;
		}

		if let Some(options) = keyboard_config.options.clone().filter(|options| !options.is_empty()) {
			xkb_config.options = Some(options);
		}

		let mut space = Space::default();

		// Create the input devices exposed to streamed applications.
		let mut seat = seat_state.new_wl_seat(&display_handle, "moonshine");
		seat.add_keyboard(xkb_config, 200, 25)
			.expect("Failed to add keyboard to seat");
		seat.add_pointer();
		seat.add_touch();
		let pen_tablet_descriptor = TabletDescriptor {
			name: "Moonlight Pen".to_owned(),
			usb_id: None,
			syspath: None,
		};
		seat.tablet_seat().add_tablet(&pen_tablet_descriptor);

		// Create the Wayland socket for clients to connect.
		let socket_source = ListeningSocketSource::new_auto().expect("Failed to create Wayland listening socket");
		let socket_name = socket_source.socket_name().to_os_string();
		let wayland_display = socket_name.to_string_lossy().into_owned();
		tracing::debug!("Wayland socket: {:?}", socket_name);

		let hdr_active = hdr;

		// Register the socket source with the event loop.
		let mut display_handle_clone = display_handle.clone();
		let wayland_socket_token = handle
			.insert_source(socket_source, move |client_stream, _, _state| {
				tracing::debug!("New Wayland client connected");
				if let Err(e) = display_handle_clone.insert_client(
					client_stream,
					std::sync::Arc::new(ClientState {
						compositor_state: CompositorClientState::default(),
					}),
				) {
					tracing::error!("Failed to insert client: {e}");
				}
			})
			.expect("Failed to register socket source");

		// Create the output global so clients can see it.
		let _output_global = output.create_global::<Self>(&display_handle);

		// Map the output in the space so that space_render_elements()
		// knows the output geometry and can associate mapped windows
		// with it. Without this, no render elements are produced.
		space.map_output(&output, (0, 0));
		let logical_output_size = space
			.output_geometry(&output)
			.map(|geometry| geometry.size)
			.unwrap_or_else(|| (width as i32, height as i32).into());
		let initial_cursor_position =
			Point::from((logical_output_size.w as f64 / 2.0, logical_output_size.h as f64 / 2.0));

		// Advertise wp_linux_dmabuf_v1 (version 6 with device feedback) so
		// Vulkan WSI and other GPU clients can create DMA-BUF-backed
		// wl_buffer objects. NVIDIA's Vulkan WSI requires the feedback
		// protocol to know which device to allocate on.
		let dmabuf_formats = renderer.dmabuf_formats();

		let render_node_dev = std::fs::metadata(render_node)
			.map(|m| {
				use std::os::unix::fs::MetadataExt;
				m.rdev()
			})
			.expect("Failed to get render node device id");
		let default_feedback = DmabufFeedbackBuilder::new(render_node_dev, dmabuf_formats.clone())
			.build()
			.expect("Failed to build DmabufFeedback");

		let mut dmabuf_state = DmabufState::new();
		let dmabuf_global =
			dmabuf_state.create_global_with_default_feedback::<Self>(&display_handle, &default_feedback);

		// Load the default xcursor and build a PointerElement with it.
		let cursor_buffer = cursor::load_default_cursor();
		let mut pointer_element = PointerElement::default();
		pointer_element.set_buffer(cursor_buffer);

		// Pre-allocate GBM buffer pool for zero-alloc frame export.
		let mut buffer_pool = Vec::with_capacity(BUFFER_POOL_SIZE);
		for i in 0..BUFFER_POOL_SIZE {
			let buffer = allocator
				.create_buffer(width, height, render_fourcc, &render_modifiers)
				.unwrap_or_else(|e| panic!("Failed to pre-allocate GBM buffer {i}: {e}"));
			let dmabuf = buffer
				.export()
				.unwrap_or_else(|e| panic!("Failed to export GBM buffer {i}: {e}"));
			buffer_pool.push(GbmBufferSlot {
				dmabuf,
				consumed: Arc::new(AtomicBool::new(true)),
			});
		}
		tracing::debug!("Pre-allocated {BUFFER_POOL_SIZE} GBM buffers for frame pool.");

		// Initialize color management protocol when HDR is active.
		let color_management = if hdr_capable {
			Some(super::color_management::ColorManagementState::new(&display_handle, hdr))
		} else {
			None
		};

		// Register swapchain protocol globals for WSI layer support.
		// Moonshine globals are always needed (for XWayland bypass, refresh_cycle, retire handling).
		// Gamescope globals are gated on HDR to avoid advertising HDR capability on SDR sessions.
		super::gamescope_swapchain::register_moonshine_globals(&display_handle);
		if hdr_capable {
			super::gamescope_swapchain::register_gamescope_globals(&display_handle);
		}

		(
			Self {
				display_handle,
				compositor_state,
				shm_state,
				xdg_shell_state,
				seat_state,
				output_manager_state,
				data_device_state,
				activation_state,
				single_pixel_buffer_state,
				output,
				damage_tracker,
				allocator,
				renderer,
				dmabuf_state,
				dmabuf_global,
				frame_tx,
				capture_failed: false,
				next_capture_at: std::time::Instant::now(),
				next_refresh_at: std::time::Instant::now(),
				max_capture_lateness_us: 0,
				missed_capture_slots: 0,
				seat,
				pending_text: String::new(),
				cursor_position: initial_cursor_position,
				cursor: Default::default(),
				pointer_element,
				capture_mode,
				capture_path: None,
				last_resource_summary: std::time::Instant::now(),
				log_stats,
				released_scanout_buffers: 0,
				captured_frames: 0,
				pre_render_rejected: 0,
				stale_after_render: 0,
				direct_frames: 0,
				composited_frames: 0,
				direct_rejections: [0; DirectReject::COUNT],
				gpu_timer: super::gpu_timing::GpuTimer::default(),
				pen_tablet_descriptor,
				active_pen_tool_kind: None,
				pen_buttons: 0,
				space,
				popups: Default::default(),
				clock,
				handle,
				width,
				height,
				render_fourcc,
				render_modifiers,
				sdr_render_format,
				hdr_render_format,
				buffer_pool,
				retired_buffer_pools: Vec::new(),
				next_buffer_index: 0,
				buffer_last_rendered_at: [None; BUFFER_POOL_SIZE],
				render_count: 0,
				screen_dirty: true,
				last_frame_sent_at: std::time::Instant::now(),
				overlay_dirty: true,
				last_cursor_position: initial_cursor_position,
				steam_overlay_input_active: false,
				viewporter_state,
				color_management,
				deferred_info_done: Vec::new(),
				xwayland_shell_state,
				xwm: None,
				xdisplay: None,
				xdisplay_tx: Some(xdisplay_tx),
				wayland_socket_token: Some(wayland_socket_token),
				wayland_display,
				hdr: hdr_active,
				hdr_capable,
				steam_mode,
				virtual_connector_strategy,
				override_surface: None,
				override_reported_window: 0,
				focused_x11_window: None,
				focused_window: None,
				upscale_scaler: super::scaling::UpscaleScaler::from_env(),
				upscale_filter: smithay::backend::renderer::TextureFilter::Linear,
				global_scale: 1.0,
				override_window: None,
				override_underlay_window: None,
				decoration_windows: Vec::new(),
				overlay_window: None,
				notification_window: None,
				external_overlay_window: None,
				pointer_focus_window: None,
				input_focus_window: None,
				current_keyboard_focus_window: None,
				input_focus_mode: 0,
				damage_sequence_counter: 0,
				map_sequence_counter: 0,
				last_keyboard_focus_window: None,
				x11_focus: None,
				x11_focus_token: None,
				focus_state: super::focus::FocusState::default(),
				window_metadata: HashMap::new(),
				transient_children: std::collections::HashMap::new(),
				held_scanout_buffers: Vec::new(),
				scanout_buffer_map: std::collections::HashMap::new(),
				scanout_next_index: BUFFER_POOL_SIZE,
				last_scanout_buffer_desc: None,
			},
			display,
		)
	}

	/// Space render elements, substituting the WSI override surface for the
	/// content of the window with X11 ID `override.1`.
	///
	/// Mirrors gamescope's `steamcompmgr_win_t::current_surface()`: a window
	/// presents the override surface whenever one is set, in place of its X11
	/// content, at the window's own geometry.
	fn space_render_elements_with_override(
		renderer: &mut GlesRenderer,
		space: &Space<smithay::desktop::Window>,
		output: &Output,
		override_surface: Option<(&WlSurface, u32)>,
		layers: SceneLayers<'_>,
		opacity: impl Fn(&smithay::desktop::Window) -> f32,
	) -> Vec<OutputRenderElements> {
		let output_scale = output.current_scale().fractional_scale();
		let scale = smithay::utils::Scale::from(output_scale);
		let Some(output_geo) = space.output_geometry(output) else {
			return Vec::new();
		};

		let mut render_window = |elements: &mut Vec<OutputRenderElements>, window: &smithay::desktop::Window| {
			let alpha = opacity(window);
			if alpha == 0.0 {
				return;
			}
			let location = (window.geometry().loc - output_geo.loc).to_physical_precise_round(scale);

			let overridden = override_surface.and_then(|(surface, xid)| {
				window
					.x11_surface()
					.is_some_and(|x| x.window_id() == xid)
					.then_some(surface)
			});

			let root = overridden
				.cloned()
				.or_else(|| window.wl_surface().map(|s| s.into_owned()));
			let opaque_id = root
				.as_ref()
				.filter(|surface| super::gamescope_swapchain::surface_is_opaque(surface))
				.map(Id::from_wayland_resource);
			let wrap = |element: SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>| {
				if opaque_id.as_ref() == Some(element.id()) {
					OutputRenderElements::OpaqueSpace(element)
				} else {
					OutputRenderElements::Space(element)
				}
			};
			if let Some(surface) = overridden {
				elements.extend(
					render_elements_from_surface_tree::<
						_,
						SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
					>(renderer, surface, location, scale, alpha, Kind::Unspecified)
					.into_iter()
					.map(wrap),
				);
			} else {
				elements.extend(
					window
						.render_elements::<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>(
							renderer, location, scale, alpha,
						)
						.into_iter()
						.map(wrap),
				);
			}
		};

		// Preserve space order for ordinary windows, then paint classified
		// layers. Dropdowns and Steam surfaces must remain above decorations.
		let mut paint_order: Vec<&smithay::desktop::Window> = space
			.elements()
			.filter(|window| !layers.decorations.contains(window) && !layers.upper.contains(&Some(*window)))
			.collect();
		for window in layers.decorations.iter().chain(layers.upper.into_iter().flatten()) {
			if space.elements().any(|w| w == window) && !paint_order.contains(&window) {
				paint_order.push(window);
			}
		}

		// Render elements are front to back (the topmost comes first).
		let mut elements = Vec::new();
		for window in paint_order.into_iter().rev() {
			render_window(&mut elements, window);
		}

		if let Some((surface, 0)) = override_surface {
			let opaque = super::gamescope_swapchain::surface_is_opaque(surface);
			let root_id = Id::from_wayland_resource(surface);
			elements.extend(
				render_elements_from_surface_tree::<
					_,
					SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
				>(renderer, surface, (0, 0), scale, 1.0, Kind::Unspecified)
				.into_iter()
				.map(|element| {
					if opaque && element.id() == &root_id {
						OutputRenderElements::OpaqueSpace(element)
					} else {
						OutputRenderElements::Space(element)
					}
				}),
			);
		}

		elements
	}

	/// Compute the scaling transform that fits the main window to the output.
	/// Returns the scale, the centering offset and the origin (the window's
	/// top-left in physical output coordinates).
	///
	/// `None` when no scaling is needed: no window to anchor to, or the window
	/// already fills the output.
	fn compute_output_scale(
		&self,
	) -> Option<(
		super::scaling::OutputScale,
		smithay::utils::Point<i32, smithay::utils::Physical>,
	)> {
		// The scaling base is the focus window.
		let window = self.focused_window.as_ref()?;
		if self.window_metadata.get(window).is_none_or(|m| m.is_useless()) {
			return None;
		}
		let out = self.output_rect();
		let geo = window.geometry();

		// The scaling source is the committed buffer size when available rather
		// than the window geometry: a window can present a swapchain larger than
		// itself (e.g. borderless fullscreen at another resolution).
		let (mut source_w, mut source_h) = self.window_source_size(window).unwrap_or((geo.size.w, geo.size.h));

		// Grow the source to include the override/dropdown window, so it stays
		// on-screen without pushing the scale below 1.
		if let Some(fit) = self.override_window.as_ref() {
			let fit_geo = fit.geometry();
			let fit_right = fit_geo.loc.x - geo.loc.x + fit_geo.size.w;
			let fit_bottom = fit_geo.loc.y - geo.loc.y + fit_geo.size.h;
			source_w = source_w.max(fit_right.clamp(0, out.size.w));
			source_h = source_h.max(fit_bottom.clamp(0, out.size.h));
		}

		let scale = super::scaling::OutputScale::for_source(
			self.upscale_scaler,
			out.size.w as f64,
			out.size.h as f64,
			source_w as f64,
			source_h as f64,
			f64::MAX,
			self.global_scale,
		);
		if scale.is_identity() {
			return None;
		}

		let output_scale = smithay::utils::Scale::from(self.output.current_scale().fractional_scale());
		let origin = (geo.loc - out.loc).to_physical_precise_round(output_scale);
		tracing::debug!(
			target: "focus",
			base_x11 = ?window.x11_surface().map(|x| x.window_id()),
			base_size = ?(geo.size.w, geo.size.h),
			base_loc = ?(geo.loc.x, geo.loc.y),
			output = ?(out.size.w, out.size.h),
			scale_x = scale.scale_x,
			scale_y = scale.scale_y,
			offset_x = scale.offset_x,
			offset_y = scale.offset_y,
			"applying output scale"
		);
		Some((scale, origin))
	}

	/// Map an output-space point into the compositor's scene space, inverting
	/// the output scaling transform.
	///
	/// Absolute pointer input arrives in output coordinates; the scene is what
	/// windows (and hit-testing) live in, so it must be un-transformed.
	pub fn output_to_scene(
		&self,
		point: smithay::utils::Point<f64, smithay::utils::Logical>,
	) -> smithay::utils::Point<f64, smithay::utils::Logical> {
		let Some((scale, origin)) = self.compute_output_scale() else {
			return point;
		};
		smithay::utils::Point::from((
			(point.x - scale.offset_x) / scale.scale_x + origin.x as f64,
			(point.y - scale.offset_y) / scale.scale_y + origin.y as f64,
		))
	}

	/// Scene-space per output-pixel ratio for relative input, inverting the
	/// output scaling (`1.0` per axis when no scaling is active).
	pub fn scene_input_ratio(&self) -> (f64, f64) {
		let output_scale = self.output.current_scale().fractional_scale();
		match self.compute_output_scale() {
			Some((scale, _)) => (
				1.0 / (output_scale * scale.scale_x),
				1.0 / (output_scale * scale.scale_y),
			),
			None => (1.0 / output_scale, 1.0 / output_scale),
		}
	}

	/// Rendered logical size of the window's content, if any.
	///
	/// A window whose content is overridden by the WSI presents the override
	/// surface, so its rendered size is the override's.
	fn window_source_size(&self, window: &smithay::desktop::Window) -> Option<(i32, i32)> {
		if let Some(x11_id) = window.x11_surface().map(|x| x.window_id())
			&& let Some((surface, render_window)) = self.override_surface.as_ref()
			&& *render_window == x11_id
			&& surface.alive()
			&& let Some(size) = Self::surface_source_size(surface)
		{
			return Some(size);
		}
		window.wl_surface().as_deref().and_then(Self::surface_source_size)
	}

	/// The logical destination size Smithay renders for a `wl_surface`.
	///
	/// A viewport can make this differ from the attached buffer's logical size.
	/// Scaling the composed scene from the buffer size would apply that viewport
	/// transform twice, causing fractional-scale clients to appear blurred or
	/// cropped. Fall back to the buffer size only before a surface view exists.
	fn surface_source_size(surface: &WlSurface) -> Option<(i32, i32)> {
		with_renderer_surface_state(surface, |st| {
			select_surface_source_size(
				st.surface_size().map(|s| (s.w, s.h)),
				st.buffer_size().map(|s| (s.w, s.h)),
			)
		})
		.flatten()
	}

	/// Visual stacking consumes classified state and never changes input focus.
	fn update_overlay_z_order(&mut self) {
		if !self.overlay_dirty {
			return;
		}
		self.overlay_dirty = false;
		// Restore the primary scene below all classified compositor overlays.
		if let Some(window) = &self.focused_window {
			self.space.raise_element(window, false);
		}
		for window in [
			self.overlay_window.clone(),
			self.notification_window.clone(),
			self.external_overlay_window.clone(),
		]
		.into_iter()
		.flatten()
		{
			if self.space.elements().any(|w| w == &window) {
				self.space.raise_element(&window, false);
			}
		}
		self.screen_dirty = true;
	}

	pub(crate) fn set_cursor_image(&mut self, image: CursorImageStatus) {
		let changed = self.cursor.image != image;
		let was_visible = self.cursor.visible();
		self.cursor.set_image(image);
		if changed || was_visible != self.cursor.visible() {
			tracing::debug!(image = ?self.cursor.image, was_visible, visible = self.cursor.visible(), "Cursor state changed");
		}
		self.screen_dirty = true;
	}

	fn cursor_visible(&self) -> bool {
		self.cursor.visible()
	}

	fn record_capture_path(&mut self, path: &'static str) {
		if self.capture_path != Some(path) {
			tracing::debug!(capture_path = path, "Capture path changed");
			self.capture_path = Some(path);
		}
	}

	/// One buffer must reproduce the complete scene, independently of focus.
	fn can_direct_scanout_scene(&self) -> Result<(), DirectReject> {
		let extras = super::capture::SceneExtras {
			cursor: self.cursor_visible(),
			steam_overlay: self.overlay_window.is_some(),
			steam_notification: self.notification_window.is_some(),
			external_overlay: self.external_overlay_window.is_some(),
			dropdown: self.override_window.is_some(),
			decoration: !self.decoration_windows.is_empty() || self.override_underlay_window.is_some(),
			scaling: self.compute_output_scale().is_some(),
			fractional_scale: self.output.current_scale().fractional_scale() != 1.0,
		};
		if let Some(reason) = extras.rejection(self.capture_mode) {
			return Err(reason);
		}
		// A standalone native override has no X11 scene window.
		if self.space.elements().next().is_none() {
			return match self.override_surface.as_ref() {
				Some((surface, 0)) if surface.alive() => self.surface_is_complete_output(surface),
				_ => Err(DirectReject::NoSurface),
			};
		}
		let Some(window) = self
			.space
			.elements()
			.rfind(|w| self.window_metadata.get(*w).is_none_or(|m| m.opacity != 0))
		else {
			return Err(DirectReject::NoSurface);
		};
		if self
			.space
			.element_geometry(window)
			.is_none_or(|geo| geo.loc != Point::from((0, 0)))
		{
			return Err(DirectReject::OutputOrigin);
		}
		if self.window_metadata.get(window).is_some_and(|m| m.opacity != 255) {
			return Err(DirectReject::NotOpaque);
		}
		let source = if self.is_override_active() {
			let Some((surface, xid)) = &self.override_surface else {
				return Err(DirectReject::NoSurface);
			};
			if window.x11_surface().is_none_or(|x| x.window_id() != *xid) {
				return Err(DirectReject::Other);
			}
			surface.clone()
		} else {
			let Some(surface) = window.wl_surface() else {
				return Err(DirectReject::NoSurface);
			};
			surface.into_owned()
		};
		if smithay::desktop::PopupManager::popups_for_surface(&source)
			.next()
			.is_some()
		{
			return Err(DirectReject::SurfaceTree);
		}
		self.surface_is_complete_output(&source)
	}

	/// A transformed/cropped tree cannot be represented by its root DMA-BUF.
	fn surface_is_complete_output(&self, surface: &WlSurface) -> Result<(), DirectReject> {
		let output = self.output_rect();
		let declared_opaque = super::gamescope_swapchain::surface_is_opaque(surface);
		with_renderer_surface_state(surface, |state| {
			let view = state.view().ok_or(DirectReject::NoSurface)?;
			if let Some(reason) = super::capture::surface_view_rejection(
				view,
				state.buffer_scale(),
				state.buffer_transform(),
				output.size,
			) {
				return Err(reason);
			}
			if !declared_opaque
				&& !state
					.opaque_regions()
					.is_some_and(|regions| regions.iter().any(|r| r.contains_rect(output)))
			{
				return Err(DirectReject::NotOpaque);
			}
			Ok(())
		})
		.unwrap_or(Err(DirectReject::NoSurface))?;
		let mut extra_content = false;
		compositor::with_surface_tree_downward(
			surface,
			(),
			|_, _, &()| smithay::wayland::compositor::TraversalAction::DoChildren(()),
			|child, states, &()| {
				// Tree callbacks already hold the surface-state lock. Read
				// renderer data directly instead of re-entering with_states.
				if child != surface
					&& states
						.data_map
						.get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()
						.is_some_and(|state| state.lock().unwrap().buffer().is_some())
				{
					extra_content = true;
				}
			},
			|_, _, &()| true,
		);
		if extra_content {
			Err(DirectReject::SurfaceTree)
		} else {
			Ok(())
		}
	}

	/// Service producer lifecycle even when downstream cannot use a capture.
	/// Frame callbacks allow the game to commit its newest buffer. Presentation
	/// feedback for skipped commits is discarded, never reported as displayed.
	fn service_uncaptured_frame(&mut self, send_callbacks: bool) {
		let mut feedback = OutputPresentationFeedback::new(&self.output);
		for window in self.space.elements() {
			if send_callbacks {
				window.send_frame(
					&self.output,
					self.clock.now(),
					Some(std::time::Duration::ZERO),
					|_, _| Some(self.output.clone()),
				);
			}
			window.take_presentation_feedback(
				&mut feedback,
				|_, _| Some(self.output.clone()),
				|_, _| {
					smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty()
				},
			);
		}
		if let Some((surface, _)) = &self.override_surface
			&& surface.alive()
		{
			if send_callbacks {
				send_frames_surface_tree(
					surface,
					&self.output,
					self.clock.now(),
					Some(std::time::Duration::ZERO),
					|_, _| Some(self.output.clone()),
				);
			}
			take_presentation_feedback_surface_tree(
				surface,
				&mut feedback,
				|_, _| Some(self.output.clone()),
				|_, _| {
					smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty()
				},
			);
		}
		// Dropping feedback sends wp_presentation_feedback.discarded.
		drop(feedback);
		if let Err(error) = self.display_handle.flush_clients() {
			tracing::error!(%error, "Failed to flush uncaptured frame callbacks");
		}
	}

	/// Render on demand; only the refresh timer emits frame callbacks. This
	/// prevents demand wakeups from increasing the client's render cadence.
	pub fn render_and_export(&mut self, send_callbacks: bool) {
		if self.log_stats && self.gpu_timer.has_pending() {
			self.gpu_timer.poll(&mut self.renderer);
		}
		self.retired_buffer_pools
			.retain(|pool| !pool.iter().all(|slot| slot.consumed.load(Ordering::Acquire)));
		// Keep the Steam overlay z-ordered above the game while it is open.
		// Must run before the static-screen early return so the raise/lower
		// is detected as soon as the overlay window commits a frame.
		self.update_overlay_z_order();

		if self.cursor.reset_dead_surface() {
			tracing::debug!(
				visible = self.cursor.visible(),
				"Cursor surface destroyed; using active fallback"
			);
			self.screen_dirty = true;
		}

		// Detect cursor-only movement as a screen change.
		if self.cursor_position != self.last_cursor_position {
			self.screen_dirty = true;
			self.last_cursor_position = self.cursor_position;
		}

		// A static scene can mean the producer is waiting for wl_buffer.release.
		// Release completed holds and flush them before deciding to skip capture.
		let tick = super::capture::prepare_capture(
			&mut self.held_scanout_buffers,
			&mut self.scanout_buffer_map,
			self.screen_dirty,
			self.last_frame_sent_at.elapsed(),
			|| {
				if let Err(e) = self.display_handle.flush_clients() {
					tracing::error!("Failed to flush scanout buffer releases: {e}");
				}
			},
		);
		if self.log_stats {
			self.released_scanout_buffers += tick.released as u64;
		}
		if self.log_stats && self.last_resource_summary.elapsed() >= std::time::Duration::from_secs(5) {
			let busy_pool_buffers = self
				.buffer_pool
				.iter()
				.filter(|slot| !slot.consumed.load(Ordering::Acquire))
				.count();
			let busy_scanout_buffers = self
				.held_scanout_buffers
				.iter()
				.filter(|(consumed, _, _)| !consumed.load(Ordering::Acquire))
				.count();
			tracing::info!(
				capture_path = ?self.capture_path, busy_pool_buffers, busy_scanout_buffers,
				compositor_gpu_timer_supported = self.gpu_timer.supported(),
				compositor_gpu_samples = self.gpu_timer.samples, compositor_gpu_disjoint_events = self.gpu_timer.disjoint,
				compositor_gpu_ms_per_composited_capture = (self.gpu_timer.samples != 0).then(|| self.gpu_timer.nanoseconds as f64 / self.gpu_timer.samples as f64 / 1e6),
				compositor_gpu_ms_per_captured_frame = (self.captured_frames != 0 && (self.composited_frames == 0 || self.gpu_timer.samples != 0)).then(|| self.gpu_timer.nanoseconds as f64 / self.captured_frames as f64 / 1e6),
				compositor_gpu_ms_per_second = (self.captured_frames != 0 && (self.composited_frames == 0 || self.gpu_timer.samples != 0)).then(|| self.gpu_timer.nanoseconds as f64 / self.last_resource_summary.elapsed().as_secs_f64() / 1e6),
				captured_frames = self.captured_frames, pre_render_rejected = self.pre_render_rejected,
				stale_after_render = self.stale_after_render, direct_export_frames = self.direct_frames,
				composited_frames = self.composited_frames, capture_requested = self.frame_tx.requested(), capture_occupied = self.frame_tx.occupied(),
				screen_dirty = self.screen_dirty,
				last_capture_age_ms = self.last_frame_sent_at.elapsed().as_millis() as u64,
				held_scanout_buffers = self.held_scanout_buffers.len(),
				scanout_buffer_map = self.scanout_buffer_map.len(),
				retired_buffer_pools = self.retired_buffer_pools.len(),
				released_scanout_buffers = self.released_scanout_buffers,
				max_capture_lateness_us = self.max_capture_lateness_us,
				missed_capture_slots = self.missed_capture_slots,
				"Video capture resources"
			);
			let r = self.direct_rejections;
			tracing::info!(
				direct_reject_forced_composition = r[0],
				direct_reject_cursor = r[1],
				direct_reject_overlay = r[2],
				direct_reject_notification = r[3],
				direct_reject_external_overlay = r[4],
				direct_reject_dropdown = r[5],
				direct_reject_decoration = r[6],
				direct_reject_scaling = r[7],
				direct_reject_fractional_scale = r[8],
				direct_reject_output_origin = r[9],
				direct_reject_not_opaque = r[10],
				direct_reject_surface_tree = r[11],
				direct_reject_transform = r[12],
				direct_reject_crop = r[13],
				direct_reject_size = r[14],
				direct_reject_no_surface = r[15],
				direct_reject_not_dmabuf = r[16],
				direct_reject_other = r[17],
				"Video direct-export rejections"
			);
			self.gpu_timer.reset_window();
			self.captured_frames = 0;
			self.pre_render_rejected = 0;
			self.stale_after_render = 0;
			self.direct_frames = 0;
			self.composited_frames = 0;
			self.direct_rejections = [0; DirectReject::COUNT];
			self.last_resource_summary = std::time::Instant::now();
			self.released_scanout_buffers = 0;
			self.max_capture_lateness_us = 0;
			self.missed_capture_slots = 0;
		}
		// Preserve the one-second keepalive for an actually static screen.
		if !tick.render {
			self.service_uncaptured_frame(send_callbacks);
			return;
		}

		if std::time::Instant::now() < self.next_capture_at {
			self.service_uncaptured_frame(send_callbacks);
			return;
		}
		let Some(credit) = self.frame_tx.try_acquire() else {
			if self.log_stats {
				self.pre_render_rejected += 1;
			}
			self.service_uncaptured_frame(send_callbacks);
			return;
		};
		// Keep the ideal cadence, skipping missed intervals without a catch-up burst.
		let interval = std::time::Duration::from_nanos(
			1_000_000_000_000 / self.output.current_mode().map_or(60_000, |mode| mode.refresh.max(1)) as u64,
		);
		let now = std::time::Instant::now();
		if self.log_stats {
			let lateness = now.saturating_duration_since(self.next_capture_at);
			self.max_capture_lateness_us = self.max_capture_lateness_us.max(lateness.as_micros() as u64);
			self.missed_capture_slots += (lateness.as_nanos() / interval.as_nanos()) as u64;
		}
		self.next_capture_at = super::capture::next_capture_deadline(self.next_capture_at, now, interval);
		let mut credit = Some(credit);

		// Try direct scanout: bypass compositor rendering when a single
		// fullscreen DMA-BUF surface covers the entire output. This avoids
		// the compositor's GLES blit, which otherwise competes with the
		// game on the gfx queue and inflates per-frame encode latency at
		// GPU saturation.
		//
		// When the WSI layer has an active override surface, scanout from
		// the override's wl_surface (the gamescope_swapchain image) and
		// deliver frame callbacks to it. Otherwise scanout from the lone
		// space toplevel as before.
		//
		// Direct scanout bypasses GLES, so skip it while an actually-drawn
		// cursor (not client-hidden) needs compositing.
		if let Err(reason) = self.can_direct_scanout_scene() {
			if self.log_stats {
				self.direct_rejections[reason as usize] += 1;
			}
		} else {
			if self.is_override_active() {
				if self.try_direct_scanout_override(&mut credit, send_callbacks) {
					self.record_capture_path("direct_override");
					return;
				}
			} else if self.try_direct_scanout(&mut credit, send_callbacks) {
				self.record_capture_path("direct");
				return;
			}
		}

		// Pick the next buffer from the pre-allocated pool.
		let idx = self.next_buffer_index;
		let slot = &self.buffer_pool[idx];
		if !slot.consumed.load(Ordering::Acquire) {
			// The encoder is still reading this buffer — skip the frame
			// to avoid overwriting its content.
			tracing::trace!("Buffer {idx} still in use by encoder, skipping frame");
			return;
		}

		self.record_capture_path("composited");

		// Mark the buffer as in-use before rendering.
		self.buffer_pool[idx].consumed.store(false, Ordering::Release);
		self.next_buffer_index = (idx + 1) % BUFFER_POOL_SIZE;

		// Clone the consumed flag before the mutable borrow on the dmabuf
		// so we can signal the encoder later without conflicting borrows.
		let consumed = self.buffer_pool[idx].consumed.clone();

		// Pre-build the ExportedFrame planes (fd duplication) BEFORE the
		// mutable borrow from renderer.bind(). This avoids a borrow
		// conflict: the framebuffer holds a mutable ref to the dmabuf,
		// and export_dmabuf would need an immutable ref to the same dmabuf.
		let frame_cs = self.color_management.as_ref().map(|cm| cm.frame_color_space());
		let mut exported_frame = match export_dmabuf(
			&self.buffer_pool[idx].dmabuf,
			idx,
			consumed.clone(),
			frame_cs,
			self.color_management.as_ref().and_then(|cm| cm.hdr_metadata()),
		) {
			Ok(frame) => frame,
			Err(e) => {
				tracing::error!("Failed to export frame: {e}");
				consumed.store(true, Ordering::Release);
				return;
			},
		};

		// Composition must consume part of the capture-to-send pacing window.
		// Preserve this origin when created_at is updated after the fence wait.
		exported_frame.composition_started_at = Some(std::time::Instant::now());

		// Compute the output scaling before binding the framebuffer, which
		// borrows `self` mutably.
		let output_scale = self.compute_output_scale();

		// Select the sampler filter for this frame (linear by default).
		let filter = self.upscale_filter;
		if let Err(e) = self.renderer.upscale_filter(filter) {
			tracing::debug!("Failed to set upscale filter: {e}");
		}
		if let Err(e) = self.renderer.downscale_filter(filter) {
			tracing::debug!("Failed to set downscale filter: {e}");
		}

		// Cursor lifetime is handled before scanout/static-screen decisions.
		let cursor_status = if self.cursor_visible() {
			self.cursor.image.clone()
		} else {
			CursorImageStatus::Hidden
		};

		if self.log_stats {
			self.gpu_timer.begin(&mut self.renderer);
		}
		// Bind the pre-allocated Dmabuf as a render target.
		let bind_result = self.renderer.bind(&mut self.buffer_pool[idx].dmabuf);
		let mut framebuffer = match bind_result {
			Ok(fb) => fb,
			Err(e) => {
				self.gpu_timer.end(&mut self.renderer);
				tracing::error!("Failed to bind Dmabuf for rendering: {e}");
				consumed.store(true, Ordering::Release);
				return;
			},
		};

		let num_space_elements = self.space.elements().count();

		self.pointer_element.set_status(cursor_status.clone());

		let cursor_hotspot = if let CursorImageStatus::Surface(ref surface) = cursor_status {
			compositor::with_states(surface, |states| {
				states
					.data_map
					.get::<std::sync::Mutex<CursorImageAttributes>>()
					.and_then(|m| m.lock().ok())
					.map(|attrs| attrs.hotspot)
					.unwrap_or_else(|| (0, 0).into())
			})
		} else {
			(0, 0).into()
		};

		let scale = smithay::utils::Scale::from(self.output.current_scale().fractional_scale());
		let cursor_pos = self.cursor_position;
		let cursor_elements: Vec<OutputRenderElements> = self.pointer_element.render_elements(
			&mut self.renderer,
			(cursor_pos - cursor_hotspot.to_f64()).to_physical(scale).to_i32_round(),
			scale,
			1.0,
		);

		// Combine elements in front-to-back order: cursor first (on top), then space.
		let mut elements: Vec<OutputRenderElements> = Vec::new();
		elements.extend(cursor_elements);

		// Drop a dead override surface.
		if self.override_surface.as_ref().is_some_and(|(s, _)| !s.alive()) {
			tracing::debug!("Override surface is dead, clearing.");
			self.override_surface = None;
			self.override_reported_window = 0;
		}

		// A window whose content is overridden by the WSI presents the override
		// surface in place of its X11 content (gamescope `current_surface()`).
		let override_target = self
			.override_surface
			.as_ref()
			.filter(|(s, _)| s.alive())
			.map(|(s, w)| (s.clone(), *w));

		let space_elements = Self::space_render_elements_with_override(
			&mut self.renderer,
			&self.space,
			&self.output,
			override_target.as_ref().map(|(s, w)| (s, *w)),
			SceneLayers {
				decorations: &self.decoration_windows,
				upper: [
					self.override_underlay_window.as_ref(),
					self.override_window.as_ref(),
					self.overlay_window.as_ref(),
					self.notification_window.as_ref(),
					self.external_overlay_window.as_ref(),
				],
			},
			|window| {
				self.window_metadata
					.get(window)
					.map(|m| m.opacity as f32 / 255.0)
					.unwrap_or(1.0)
			},
		);
		elements.extend(space_elements);

		tracing::trace!(
			num_space_elements,
			num_render_elements = elements.len(),
			"Rendering frame"
		);

		// Compute the buffer age for partial damage tracking.
		// Age = number of render_output calls since this buffer was last rendered to.
		// `None` (first use) → 0 → full redraw (contents undefined).
		let buffer_age = self.buffer_last_rendered_at[idx]
			.map(|last| self.render_count - last)
			.unwrap_or(0);

		let render_result = if let Some((scale, origin)) = output_scale {
			let output_scale = self.output.current_scale().fractional_scale();
			let center = smithay::utils::Point::<i32, smithay::utils::Physical>::from((
				(scale.offset_x * output_scale).round() as i32,
				(scale.offset_y * output_scale).round() as i32,
			));
			let scaled: Vec<ScaledOutputElement> = elements
				.into_iter()
				.map(|e| {
					let rescaled = RescaleRenderElement::from_element(e, origin, (scale.scale_x, scale.scale_y));
					RelocateRenderElement::from_element(rescaled, center - origin, Relocate::Relative)
				})
				.collect();
			// The relocation wrapper does not offset damage regions, so redraw
			// the whole output while a scale/offset transform is active.
			self.damage_tracker.render_output(
				&mut self.renderer,
				&mut framebuffer,
				0,
				&scaled,
				[0.0, 0.0, 0.0, 1.0], // black clear color
			)
		} else {
			self.damage_tracker.render_output(
				&mut self.renderer,
				&mut framebuffer,
				buffer_age,
				&elements,
				[0.0, 0.0, 0.0, 1.0], // black clear color
			)
		};

		// Update the buffer's render count for future age calculations.
		self.buffer_last_rendered_at[idx] = Some(self.render_count);
		self.render_count += 1;

		let sync = match &render_result {
			Ok(r) => r.sync.clone(),
			Err(smithay::backend::renderer::damage::Error::OutputNoMode(_)) => {
				smithay::backend::renderer::sync::SyncPoint::signaled()
			},
			Err(e) => {
				drop(framebuffer);
				self.gpu_timer.end(&mut self.renderer);
				tracing::error!("Failed to render output: {e}; stopping capture");
				// A partial submission may still write this buffer.
				self.capture_failed = true;
				return;
			},
		};

		// Drop framebuffer before sending, to release the mutable borrow on dmabuf.
		drop(framebuffer);
		self.gpu_timer.end(&mut self.renderer);

		// Block until the render has actually completed. `finish()` only flushes
		// the GL blit, so without waiting the encoder can read a stale buffer
		// from the round-robin pool (frames arrive out of order).
		if let Err(e) = sync.wait() {
			tracing::error!("Failed to wait for render fence: {e}; stopping capture");
			// Never publish or recycle a buffer whose GPU completion is unknown.
			self.capture_failed = true;
			return;
		}

		// Update created_at to reflect the actual render completion time.
		// The ExportedFrame was built before renderer.bind() (borrow workaround),
		// so the original timestamp is too early.
		let mut exported_frame = exported_frame;
		exported_frame.created_at = std::time::Instant::now();

		// Send the pre-built frame to the encoder.
		// The rendering happened after export_dmabuf duplicated the fds,
		// but the fds reference the same DMA-BUF — the encoder will see
		// the freshly rendered content.
		match self
			.frame_tx
			.try_send(exported_frame, credit.take().expect("capture credit"))
		{
			Err(super::admission::CaptureSendError::Disconnected) => {
				consumed.store(true, Ordering::Release);
				tracing::debug!("Frame channel disconnected, compositor stopping.");
			},
			Err(super::admission::CaptureSendError::Full) => {
				// Channel full (only possible during epoch reset) — release the buffer.
				if self.log_stats {
					self.stale_after_render += 1;
				}
				consumed.store(true, Ordering::Release);
			},
			Ok(()) => {
				// Frame accepted — reset dirty tracking.
				if self.log_stats {
					self.captured_frames += 1;
					self.composited_frames += 1;
				}
				self.screen_dirty = false;
				self.last_frame_sent_at = std::time::Instant::now();
			},
		}

		// Send frame callbacks to clients so they know to submit the
		// next buffer.
		self.space.elements().for_each(|window| {
			if send_callbacks {
				window.send_frame(
					&self.output,
					self.clock.now(),
					Some(std::time::Duration::ZERO),
					|_, _| Some(self.output.clone()),
				);
			}
		});

		// Native Wayland clients can wait for presentation feedback before
		// submitting again, even when their buffers are composited rather than scanned out.
		let mut feedback = OutputPresentationFeedback::new(&self.output);
		if let Ok(result) = &render_result {
			for window in self.space.elements() {
				window.take_presentation_feedback(
					&mut feedback,
					|surface, _| {
						result
							.states
							.element_was_presented(surface)
							.then(|| self.output.clone())
					},
					|_, _| {
						smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty()
					},
				);
			}
		}

		// Also send frame callbacks to the override surface if active,
		// so the NVIDIA driver's Wayland WSI unblocks and presents the
		// next frame.
		if let Some((ref override_surface, _)) = self.override_surface
			&& override_surface.alive()
		{
			if send_callbacks {
				send_frames_surface_tree(
					override_surface,
					&self.output,
					self.clock.now(),
					Some(std::time::Duration::ZERO),
					|_, _| Some(self.output.clone()),
				);
			}

			// Drain and respond to wp_presentation_feedback callbacks
			// so the NVIDIA driver's WaitForPresentKHR can return.
			take_presentation_feedback_surface_tree(
				override_surface,
				&mut feedback,
				|_, _| Some(self.output.clone()),
				|_, _| {
					smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty()
				},
			);
		}
		let frame_period = self
			.output
			.preferred_mode()
			.map(|m| std::time::Duration::from_nanos(1_000_000_000_000u64 / m.refresh.max(1) as u64))
			.unwrap_or(std::time::Duration::from_millis(11));
		feedback.presented::<smithay::utils::Time<Monotonic>, Monotonic>(
			self.clock.now(),
			Refresh::Fixed(frame_period),
			0,
			smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty(
			),
		);

		// Flush the frame callbacks (and any other pending events) to
		// clients immediately. Without this, the wl_callback.done events
		// sit in the outgoing buffer until the next Wayland socket
		// activity (e.g. mouse movement), starving the client's present
		// loop.
		if let Err(e) = self.display_handle.flush_clients() {
			tracing::error!("Failed to flush clients after render: {e}");
		}
	}

	/// Attempt direct DMA-BUF scanout, bypassing compositor rendering.
	///
	/// Returns `true` if a frame was successfully exported directly from
	/// the client's DMA-BUF, skipping the GBM pool and GL compositing.
	/// This preserves pixel-exact content (important for HDR/PQ) and
	/// reduces GPU usage and latency.
	///
	/// Conditions for direct scanout:
	/// - An opaque root exactly covers the output, with no extra visible content
	/// - The window's committed buffer is a DMA-BUF (not SHM)
	fn try_direct_scanout(
		&mut self,
		credit: &mut Option<super::admission::CaptureCredit>,
		send_callbacks: bool,
	) -> bool {
		// Scene eligibility has proved that this opaque top window covers all
		// other windows. Clone just the candidate, without a per-frame Vec.
		let Some(window) = self
			.space
			.elements()
			.rfind(|w| self.window_metadata.get(*w).is_none_or(|m| m.opacity != 0))
			.cloned()
		else {
			return false;
		};
		// Retain the origin safeguard inside the technical export path too.
		if self
			.space
			.element_geometry(&window)
			.is_none_or(|geo| geo.loc != Point::from((0, 0)))
		{
			return false;
		}
		let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else {
			return false;
		};

		// Get the committed buffer and check if it's a DMA-BUF.
		let scanout_buffer = with_renderer_surface_state(&wl_surface, |state| {
			let buffer = state.buffer()?;
			if !matches!(smithay::backend::renderer::buffer_type(buffer), Some(BufferType::Dma)) {
				return None;
			}
			Some(buffer.clone())
		});

		let Some(Some(buffer)) = scanout_buffer else {
			if self.log_stats {
				self.direct_rejections[DirectReject::NotDmabuf as usize] += 1;
			}
			tracing::trace!("Direct scanout: no committed buffer");
			return false;
		};
		let Ok(client_dmabuf) = dmabuf::get_dmabuf(&buffer) else {
			if self.log_stats {
				self.direct_rejections[DirectReject::NotDmabuf as usize] += 1;
			}
			tracing::trace!("Direct scanout: failed to get DMA-BUF from buffer");
			return false;
		};
		let client_dmabuf = client_dmabuf.clone();

		// The surface buffer must exactly match the output dimensions with
		// no scaling or offset, otherwise the encoder would receive a
		// partial or stretched frame.
		if client_dmabuf.width() != self.width || client_dmabuf.height() != self.height {
			if self.log_stats {
				self.direct_rejections[DirectReject::Size as usize] += 1;
			}
			tracing::trace!(
				"Direct scanout: size mismatch (client {}x{} vs output {}x{})",
				client_dmabuf.width(),
				client_dmabuf.height(),
				self.width,
				self.height,
			);
			return false;
		}

		// Assign a stable buffer index for the encoder's import cache, keyed
		// by the wl_buffer's ObjectId (stable across re-attaches of the same
		// buffer regardless of how the protocol handles fd duplication).
		let buffer_id = buffer.id();
		let buffer_index = *self.scanout_buffer_map.entry(buffer_id.clone()).or_insert_with(|| {
			let idx = self.scanout_next_index;
			self.scanout_next_index += 1;
			idx
		});

		let consumed = Arc::new(AtomicBool::new(false));

		let color_space = self
			.color_management
			.as_ref()
			.map(|cm| cm.surface_color_space(&wl_surface))
			.unwrap_or(FrameColorSpace::Srgb);
		let hdr_metadata = self
			.color_management
			.as_ref()
			.and_then(|cm| cm.surface_hdr_metadata(&wl_surface));

		let planes: Vec<ExportedPlane> = client_dmabuf
			.handles()
			.zip(client_dmabuf.offsets())
			.zip(client_dmabuf.strides())
			.map(
				|((handle, offset), stride): ((std::os::unix::io::BorrowedFd<'_>, u32), u32)| ExportedPlane {
					fd: handle.as_raw_fd(),
					offset,
					stride,
				},
			)
			.collect();

		let exported_frame = ExportedFrame {
			capture_credit: None,
			planes,
			format: client_dmabuf.format().code as u32,
			modifier: Into::<u64>::into(client_dmabuf.format().modifier),
			width: client_dmabuf.width(),
			height: client_dmabuf.height(),
			created_at: std::time::Instant::now(),
			composition_started_at: None,
			buffer_index,
			consumed: consumed.clone(),
			color_space,
			hdr_metadata,
		};

		// Hold the client Buffer alive until the encoder finishes reading.
		self.held_scanout_buffers.push((consumed.clone(), buffer_id, buffer));

		match self
			.frame_tx
			.try_send(exported_frame, credit.take().expect("capture credit"))
		{
			Err(super::admission::CaptureSendError::Disconnected) => {
				consumed.store(true, Ordering::Release);
				tracing::debug!("Frame channel disconnected, compositor stopping.");
			},
			Err(super::admission::CaptureSendError::Full) => {
				consumed.store(true, Ordering::Release);
			},
			Ok(()) => {
				if self.log_stats {
					self.captured_frames += 1;
					self.direct_frames += 1;
				}
				self.screen_dirty = false;
				self.last_frame_sent_at = std::time::Instant::now();
			},
		}

		// Send frame callbacks to the client.
		if send_callbacks {
			window.send_frame(
				&self.output,
				self.clock.now(),
				Some(std::time::Duration::ZERO),
				|_, _| Some(self.output.clone()),
			);
		}

		// Drain and respond to wp_presentation_feedback callbacks.
		let mut feedback = OutputPresentationFeedback::new(&self.output);
		take_presentation_feedback_surface_tree(
			&wl_surface,
			&mut feedback,
			|_, _| Some(self.output.clone()),
			|_, _| {
				smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty()
			},
		);
		let frame_period = self
			.output
			.preferred_mode()
			.map(|m| std::time::Duration::from_nanos(1_000_000_000_000u64 / m.refresh.max(1) as u64))
			.unwrap_or(std::time::Duration::from_millis(11));
		feedback.presented::<smithay::utils::Time<Monotonic>, Monotonic>(
			self.clock.now(),
			Refresh::Fixed(frame_period),
			0,
			smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty(
			),
		);

		if let Err(e) = self.display_handle.flush_clients() {
			tracing::error!("Failed to flush clients after scanout: {e}");
		}

		true
	}

	/// Direct DMA-BUF scanout for the gamescope/moonshine override surface.
	///
	/// When the WSI layer has installed an override surface (gamescope_swapchain
	/// or moonshine_swapchain protocol), the game's frames are committed to
	/// `self.override_surface` rather than to a space toplevel. Without this
	/// path the compositor falls back to a GLES blit on the gfx queue, which
	/// competes with the game's rendering at GPU saturation and inflates encode
	/// latency. This sends the override surface's committed DMA-BUF straight
	/// to the encoder and delivers frame callbacks to the override surface so
	/// the WSI layer's `vkQueuePresentKHR` can unblock for the next frame.
	fn try_direct_scanout_override(
		&mut self,
		credit: &mut Option<super::admission::CaptureCredit>,
		send_callbacks: bool,
	) -> bool {
		let override_surface = match self.override_surface.as_ref() {
			Some((s, _)) if s.alive() => s.clone(),
			_ => return false,
		};

		let scanout_buffer = with_renderer_surface_state(&override_surface, |state| {
			let buffer = state.buffer()?;
			if !matches!(smithay::backend::renderer::buffer_type(buffer), Some(BufferType::Dma)) {
				return None;
			}
			Some(buffer.clone())
		});
		let Some(Some(buffer)) = scanout_buffer else {
			if self.log_stats {
				self.direct_rejections[DirectReject::NotDmabuf as usize] += 1;
			}
			tracing::trace!("Override scanout: no committed buffer");
			return false;
		};
		let Ok(client_dmabuf) = dmabuf::get_dmabuf(&buffer) else {
			if self.log_stats {
				self.direct_rejections[DirectReject::NotDmabuf as usize] += 1;
			}
			tracing::trace!("Override scanout: failed to get DMA-BUF from buffer");
			return false;
		};
		let client_dmabuf = client_dmabuf.clone();

		if client_dmabuf.width() != self.width || client_dmabuf.height() != self.height {
			if self.log_stats {
				self.direct_rejections[DirectReject::Size as usize] += 1;
			}
			tracing::trace!(
				"Override scanout: size mismatch (client {}x{} vs output {}x{})",
				client_dmabuf.width(),
				client_dmabuf.height(),
				self.width,
				self.height,
			);
			return false;
		}

		let buffer_id = buffer.id();
		let buffer_index = *self.scanout_buffer_map.entry(buffer_id.clone()).or_insert_with(|| {
			let idx = self.scanout_next_index;
			self.scanout_next_index += 1;
			idx
		});

		// Log buffer parameters only when they change — logging every new
		// buffer index would spam at frame rate on NVIDIA (see
		// `last_scanout_buffer_desc`).
		let buffer_desc = (
			client_dmabuf.format().code as u32,
			Into::<u64>::into(client_dmabuf.format().modifier),
			client_dmabuf.num_planes(),
			client_dmabuf.width(),
			client_dmabuf.height(),
		);
		if self.last_scanout_buffer_desc != Some(buffer_desc) {
			tracing::debug!(
				buffer_index,
				fourcc = ?client_dmabuf.format().code,
				modifier = format!("{:#x}", Into::<u64>::into(client_dmabuf.format().modifier)).as_str(),
				num_planes = client_dmabuf.num_planes(),
				width = client_dmabuf.width(),
				height = client_dmabuf.height(),
				"Override scanout: buffer parameters changed",
			);
			self.last_scanout_buffer_desc = Some(buffer_desc);
		}

		let consumed = Arc::new(AtomicBool::new(false));

		let color_space = self
			.color_management
			.as_ref()
			.map(|cm| cm.surface_color_space(&override_surface))
			.unwrap_or(FrameColorSpace::Srgb);
		let hdr_metadata = self
			.color_management
			.as_ref()
			.and_then(|cm| cm.surface_hdr_metadata(&override_surface));

		let planes: Vec<ExportedPlane> = client_dmabuf
			.handles()
			.zip(client_dmabuf.offsets())
			.zip(client_dmabuf.strides())
			.map(
				|((handle, offset), stride): ((std::os::unix::io::BorrowedFd<'_>, u32), u32)| ExportedPlane {
					fd: handle.as_raw_fd(),
					offset,
					stride,
				},
			)
			.collect();

		let exported_frame = ExportedFrame {
			capture_credit: None,
			planes,
			format: client_dmabuf.format().code as u32,
			modifier: Into::<u64>::into(client_dmabuf.format().modifier),
			width: client_dmabuf.width(),
			height: client_dmabuf.height(),
			created_at: std::time::Instant::now(),
			composition_started_at: None,
			buffer_index,
			consumed: consumed.clone(),
			color_space,
			hdr_metadata,
		};

		self.held_scanout_buffers.push((consumed.clone(), buffer_id, buffer));

		match self
			.frame_tx
			.try_send(exported_frame, credit.take().expect("capture credit"))
		{
			Err(super::admission::CaptureSendError::Disconnected) => {
				consumed.store(true, Ordering::Release);
				tracing::debug!("Frame channel disconnected, compositor stopping.");
			},
			Err(super::admission::CaptureSendError::Full) => {
				consumed.store(true, Ordering::Release);
			},
			Ok(()) => {
				if self.log_stats {
					self.captured_frames += 1;
					self.direct_frames += 1;
				}
				self.screen_dirty = false;
				self.last_frame_sent_at = std::time::Instant::now();
			},
		}

		// Frame callbacks must go to the override surface (the game's WSI
		// layer is waiting on these to unblock vkQueuePresentKHR). Without
		// this the game would block forever after the first frame.
		if send_callbacks {
			send_frames_surface_tree(
				&override_surface,
				&self.output,
				self.clock.now(),
				Some(std::time::Duration::ZERO),
				|_, _| Some(self.output.clone()),
			);
		}

		let mut feedback = OutputPresentationFeedback::new(&self.output);
		take_presentation_feedback_surface_tree(
			&override_surface,
			&mut feedback,
			|_, _| Some(self.output.clone()),
			|_, _| {
				smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty()
			},
		);
		let frame_period = self
			.output
			.preferred_mode()
			.map(|m| std::time::Duration::from_nanos(1_000_000_000_000u64 / m.refresh.max(1) as u64))
			.unwrap_or(std::time::Duration::from_millis(11));
		feedback.presented::<smithay::utils::Time<Monotonic>, Monotonic>(
			self.clock.now(),
			Refresh::Fixed(frame_period),
			0,
			smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty(
			),
		);

		if let Err(e) = self.display_handle.flush_clients() {
			tracing::error!("Failed to flush clients after override scanout: {e}");
		}

		true
	}

	/// Handle gamescope WSI layer's `override_window_content` request.
	///
	/// Stores the override surface so it gets rendered instead of the
	/// original X11 window.
	pub fn override_window_surface(&mut self, x11_window: u32, surface: WlSurface) {
		// Clear stale HDR state from the previous override surface when the
		// surface changes.  DXVK sometimes creates a new X11 window (and thus
		// a new wl_surface) when toggling HDR mode, so the old surface's
		// gamescope_current entry must be evicted explicitly — it won't be
		// cleaned up by create_swapchain (which only sees the new surface).
		if let Some(old_surface) = self.override_surface.as_ref().map(|(s, _)| s.clone())
			&& old_surface != surface
			&& let Some(cm) = &mut self.color_management
		{
			cm.clear_gamescope_current(&old_surface);
		}

		self.override_reported_window = x11_window;
		self.override_surface = Some((surface, x11_window));
		self.resolve_override_window();
	}

	/// Resolve the override surface's target window from the WSI-reported xid.
	///
	/// The WSI reports the window its Vulkan swapchain is created on, which for
	/// Wine/DXVK is a child of the WM-visible toplevel. Key the override by the
	/// ancestor the compositor actually renders, independent of the current
	/// focus (which can change after the override is stored).
	///
	/// Smithay's XWM reparents clients into frame windows, so the root's child
	/// is a frame that is never rendered; the managed client window is the
	/// first ancestor present in the space. Re-run on window map because the
	/// swapchain can be created before its window is mapped.
	pub fn resolve_override_window(&mut self) {
		let Some(alive) = self.override_surface.as_ref().map(|(s, _)| s.alive()) else {
			return;
		};
		// A dead swapchain surface has no live window to resolve against; the
		// render path clears it.
		if !alive {
			return;
		}
		let reported = self.override_reported_window;
		if reported == 0 {
			// Native Wayland surface: not tied to an X11 window.
			if let Some((_, render)) = self.override_surface.as_mut() {
				*render = 0;
			}
			return;
		}

		// Keep the current mapping if it already points at a rendered window.
		let current = self.override_surface.as_ref().map(|(_, r)| *r);
		if current.is_some_and(|c| {
			self.space
				.elements()
				.any(|w| w.x11_surface().is_some_and(|x| x.window_id() == c))
		}) {
			return;
		}

		let chain = self
			.x11_focus
			.as_ref()
			.map(|xf| xf.get_ancestor_chain(reported))
			.unwrap_or_default();
		let resolved = chain
			.iter()
			.copied()
			.find(|id| {
				self.space
					.elements()
					.any(|w| w.x11_surface().is_some_and(|x| x.window_id() == *id))
			})
			.unwrap_or(reported);
		if let Some((_, render)) = self.override_surface.as_mut() {
			*render = resolved;
		}
		tracing::debug!(
			reported,
			resolved,
			chain = ?chain,
			"Resolved override surface window"
		);
	}

	/// Returns `true` when a live WSI override surface exists for a window in
	/// the scene.  The override *is* that window's content, so it applies
	/// whenever the window is shown, not only while it is focused.
	pub fn is_override_active(&self) -> bool {
		self.override_surface.as_ref().is_some_and(|(s, render_window)| {
			s.alive()
				&& (*render_window == 0
					|| self
						.space
						.elements()
						.any(|w| w.x11_surface().is_some_and(|x| x.window_id() == *render_window)))
		})
	}

	/// Clear all dropdown/override windows.
	///
	/// Gamescope: `wlserver_clear_dropdowns()` — clears all dropdown surfaces
	/// when focus changes. This ensures dropdowns don't persist across focus
	/// changes and don't interfere with the new focus window.
	pub fn clear_dropdowns(&mut self) {
		if let Some(override_win) = self.override_window.take() {
			self.focus_state.mark_dirty();
			// Unmap the override window from the space so stale menus/tooltips
			// are no longer rendered or receive input.
			self.space.unmap_elem(&override_win);
			// Clean up metadata and transient children index.
			if let Some(meta) = self.window_metadata.get(&override_win)
				&& let Some(parent_id) = meta.transient_for
				&& let Some(children) = self.transient_children.get_mut(&parent_id)
			{
				children.retain(|w| *w != override_win);
				if children.is_empty() {
					self.transient_children.remove(&parent_id);
				}
			}
			self.window_metadata.remove(&override_win);
		}
		tracing::debug!(target: "focus", "Cleared dropdown/override windows");
	}

	/// Register a dropdown/override window.
	///
	/// Gamescope: `wlserver_notify_dropdown()` — registers a dropdown surface
	/// with its position. This is called when a new dropdown/menu/tooltip
	/// window is mapped and is a transient child of the focused window.
	///
	/// Also walks transient children to find nested dropdowns (e.g., submenu).
	///
	/// Returns `true` if the dropdown was successfully registered, `false`
	/// if it was rejected (e.g., conflicts with notification/external overlay).
	#[must_use = "dropdown registration result indicates success or rejection"]
	pub fn notify_dropdown(&mut self, window: smithay::desktop::Window, x: i32, y: i32) -> bool {
		// Don't register dropdown if it conflicts with notification or
		// external overlay windows. Gamescope keeps these separate.
		if self.window_metadata.get(&window).is_some_and(|m| {
			m.flags.intersects(
				super::focus::WindowFlags::NOTIFICATION
					| super::focus::WindowFlags::OVERLAY
					| super::focus::WindowFlags::EXTERNAL_OVERLAY,
			)
		}) {
			tracing::debug!(
				target: "focus",
				window_id = window.x11_surface().map(|x| x.window_id()),
				"Rejecting dropdown: conflicts with notification window"
			);
			return false;
		}
		if self.external_overlay_window.as_ref() == Some(&window) {
			tracing::debug!(
				target: "focus",
				window_id = window.x11_surface().map(|x| x.window_id()),
				"Rejecting dropdown: conflicts with external overlay window"
			);
			return false;
		}

		self.override_window = Some(window.clone());
		self.focus_state.mark_dirty();

		// Send a synthetic pointer motion to establish pointer focus on the
		// dropdown so that subsequent MouseButtonDown/Up are delivered there
		// instead of the previously focused surface.
		if let Some(x11) = window.x11_surface()
			&& let Some(wl_surface) = x11.wl_surface()
		{
			let window_loc = Point::from((x as f64, y as f64));
			let pointer = self.seat.get_pointer().expect("pointer should exist");
			let serial = smithay::utils::SERIAL_COUNTER.next_serial();
			tracing::debug!(
				target: "focus",
				surface_id = ?wl_surface,
				"notify_dropdown: sending initial pointer motion event to dropdown"
			);
			pointer.motion(
				self,
				Some((wl_surface.clone(), window_loc)),
				&smithay::input::pointer::MotionEvent {
					location: self.cursor_position,
					serial,
					time: InputTime::from_millis(self.clock.now().as_millis()),
				},
			);
			pointer.frame(self);
		}

		tracing::debug!(
			target: "focus",
			window_id = window.x11_surface().map(|x| x.window_id()),
			x,
			y,
			"Registered dropdown/override window"
		);

		true
	}

	/// Start the XWayland server so X11 applications can connect.
	///
	/// Spawns the XWayland process and registers it as a calloop
	/// event source. When XWayland signals readiness, the X11 window
	/// manager is started and DISPLAY is set for child processes.
	pub fn start_xwayland(&mut self) {
		use smithay::wayland::compositor::CompositorHandler;
		use smithay::xwayland::{XWayland, XWaylandEvent};

		// Log XWayland stderr to a file for debugging.
		let log_dir = std::env::temp_dir().join("moonshine");
		let _ = std::fs::create_dir_all(&log_dir);
		let xwayland_log_stderr = std::fs::File::create(log_dir.join("xwayland.log"))
			.map(std::process::Stdio::from)
			.unwrap_or_else(|_| std::process::Stdio::null());
		let xwayland_log_stdout = std::fs::File::create(log_dir.join("xwayland_stdout.log"))
			.map(std::process::Stdio::from)
			.unwrap_or_else(|_| std::process::Stdio::null());

		// Log key environment state before spawning.
		tracing::debug!(
			wayland_display = %self.wayland_display,
			xdg_runtime_dir = ?std::env::var("XDG_RUNTIME_DIR"),
			"Spawning XWayland"
		);

		let (xwayland, client) = match XWayland::spawn(
			&self.display_handle,
			None,
			std::env::var("MOONSHINE_WAYLAND_DEBUG")
				.ok()
				.map(|_| ("WAYLAND_DEBUG", "1")),
			// Emulate RandR so games can change resolution: without it Xwayland
			// accepts the request but the mode never changes, leaving games
			// stuck. Games launched through wlroots/gamescope get this flag.
			std::iter::once("-force-xrandr-emulation"),
			true,
			xwayland_log_stdout,
			xwayland_log_stderr,
			|_| (),
		) {
			Ok(result) => result,
			Err(e) => {
				tracing::error!("Failed to spawn XWayland: {e}");
				return;
			},
		};
		tracing::debug!(
			display_number = xwayland.display_number(),
			"XWayland process spawned, waiting for readiness."
		);

		let ret = self
			.handle
			.insert_source(xwayland, move |event, _, data: &mut MoonshineCompositor| match event {
				XWaylandEvent::Ready {
					x11_socket,
					display_number,
				} => {
					// Set the client compositor scale to 1.0 (no HiDPI scaling for XWayland).
					data.client_compositor_state(&client).set_client_scale(1.0);

					let wm = smithay::xwayland::X11Wm::start_wm(
						data.handle.clone(),
						&data.display_handle,
						x11_socket,
						client.clone(),
					)
					.expect("Failed to start X11 window manager.");

					tracing::debug!(display_number, "XWayland ready.");

					data.xwm = Some(wm);
					data.xdisplay = Some(display_number);

					// Open an X11 connection to the XWayland display for
					// reading root window properties (Steam focus control).
					if data.x11_focus.is_none() {
						data.x11_focus = super::x11_focus::X11Focus::open(display_number);
						data.watch_steam_focus_control();
					}

					// Notify the session thread that XWayland is ready.
					if let Some(tx) = data.xdisplay_tx.take() {
						let _ = tx.send(super::CompositorReady {
							xdisplay: display_number,
							wayland_display: data.wayland_display.clone(),
							hdr_capable: data.hdr_capable,
						});
					}
				},
				XWaylandEvent::Error => {
					tracing::error!("XWayland crashed on startup.");
				},
			});

		if let Err(e) = ret {
			tracing::error!("Failed to insert XWayland source into event loop: {e}");
		}
	}

	/// Watch the root window for Steam focus-control changes.
	///
	/// Steam signals "the game is up, focus it" by reordering
	/// `GAMESCOPECTRL_BASELAYER_APPID` on the root window. Nothing else in the
	/// compositor wakes on that: `XwmHandler::property_notify` only fires for
	/// windows the XWM manages, and once a game is running and steady no window
	/// event triggers a re-evaluation. Without this source the handoff is read
	/// exactly once — while the Steam UI is still legitimately in front — and
	/// focus never moves to the game.
	fn watch_steam_focus_control(&mut self) {
		let Some(x11_focus) = self.x11_focus.as_ref() else {
			return;
		};
		if !x11_focus.watch_focus_control() {
			tracing::warn!(target: "focus", "Failed to select root PropertyNotify; Steam focus handoff will be missed");
			return;
		}
		let Some(fd) = x11_focus.connection_fd() else {
			tracing::warn!(target: "focus", "No X11 connection fd; Steam focus handoff will be missed");
			return;
		};

		// Safety: the fd belongs to `self.x11_focus`, which outlives this
		// source — `shutdown_session_processes` removes the source before
		// dropping `X11Focus` and closing the display.
		let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
		let source = calloop::generic::Generic::new(borrowed, calloop::Interest::READ, calloop::Mode::Level);
		match self.handle.insert_source(source, |_, _, state: &mut Self| {
			if state
				.x11_focus
				.as_ref()
				.is_some_and(|x11_focus| x11_focus.drain_focus_control_change())
			{
				state.reevaluate_focus();
			}
			Ok(calloop::PostAction::Continue)
		}) {
			Ok(token) => {
				self.x11_focus_token = Some(token);
				tracing::debug!(target: "focus", fd, "Watching root window for Steam focus control changes");
			},
			Err(e) => {
				tracing::error!("Failed to insert X11 focus control source: {e}");
			},
		}
	}

	/// Shut down XWayland server connections.
	///
	/// Drops the X11 window manager connection so Xwayland sees no remaining
	/// connections and exits. The application's systemd scope is stopped by
	/// `Application::Drop` after the compositor thread has exited.
	pub fn shutdown_session_processes(&mut self) {
		self.gpu_timer.destroy(&mut self.renderer);
		if let Some(token) = self.wayland_socket_token.take() {
			self.handle.remove(token);
			tracing::debug!(wayland_display = %self.wayland_display, "Removed Wayland listening socket source");
		}

		// Remove the root-window PropertyNotify source before the connection
		// goes away: its fd is borrowed from `x11_focus`, so closing the
		// display first would leave a stale fd registered with the event loop.
		if let Some(token) = self.x11_focus_token.take() {
			self.handle.remove(token);
			tracing::debug!(target: "focus", "Removed X11 focus control source");
		}

		// Clear the X11 focus control connection so reevaluate_focus
		// can't read from a dead X11 display during cleanup.
		if self.x11_focus.take().is_some() {
			tracing::debug!("Cleared X11 focus control connection");
		}

		// Drop the X11 window manager, closing the privileged WM
		// connection to Xwayland. Xwayland will see no remaining connections.
		if self.xwm.take().is_some() {
			tracing::debug!("Dropped X11 window manager");
		}
	}
}

fn select_surface_source_size(surface_size: Option<(i32, i32)>, buffer_size: Option<(i32, i32)>) -> Option<(i32, i32)> {
	surface_size.or(buffer_size)
}

/// Convert a Smithay Dmabuf into our pipeline's ExportedFrame.
///
/// Export a DMA-BUF as an `ExportedFrame` for the video encoder.
///
/// Plane fds are borrowed (raw fd numbers) from the compositor's buffer pool.
/// The pool outlives all in-flight frames and the `consumed` flag prevents
/// buffer recycling before the encoder finishes reading.
fn export_dmabuf(
	dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
	buffer_index: usize,
	consumed: Arc<AtomicBool>,
	surface_color_space: Option<FrameColorSpace>,
	hdr_metadata: Option<HdrMetadata>,
) -> Result<ExportedFrame, String> {
	let planes: Vec<ExportedPlane> = dmabuf
		.handles()
		.zip(dmabuf.offsets())
		.zip(dmabuf.strides())
		.map(|((handle, offset), stride)| ExportedPlane {
			fd: handle.as_raw_fd(),
			offset,
			stride,
		})
		.collect();

	Ok(ExportedFrame {
		capture_credit: None,
		planes,
		format: dmabuf.format().code as u32,
		modifier: Into::<u64>::into(dmabuf.format().modifier),
		width: dmabuf.width(),
		height: dmabuf.height(),
		created_at: std::time::Instant::now(),
		composition_started_at: None,
		buffer_index,
		consumed,
		color_space: surface_color_space.unwrap_or(FrameColorSpace::Srgb),
		hdr_metadata,
	})
}

#[cfg(test)]
mod tests {
	use super::select_surface_source_size;

	#[test]
	fn viewport_destination_wins_over_buffer_size() {
		assert_eq!(
			select_surface_source_size(Some((2880, 1920)), Some((4320, 2880))),
			Some((2880, 1920))
		);
	}

	#[test]
	fn buffer_size_is_used_before_surface_view_exists() {
		assert_eq!(select_surface_source_size(None, Some((1280, 720))), Some((1280, 720)));
	}
}
