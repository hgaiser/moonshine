//! Compositor state and Smithay protocol handler implementations.
//!
//! `MoonshineCompositor` is the central state struct for the headless compositor.
//! All Smithay `delegate_*!` macros target this struct.

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use smithay::reexports::wayland_server::Resource;

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::GbmAllocator;
use smithay::backend::allocator::{Allocator, Buffer, Fourcc, Modifier};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Element, Id, Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{with_renderer_surface_state, CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{Bind, BufferType, ImportDma};
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::utils::send_frames_surface_tree;
use smithay::desktop::utils::{take_presentation_feedback_surface_tree, OutputPresentationFeedback};
use std::collections::HashMap;

use smithay::desktop::Space;
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
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
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Wm;

use super::cursor::{self, PointerElement, PointerRenderElement};
use super::frame::{ExportedFrame, ExportedPlane, FrameColorSpace, HdrMetadata};
use crate::config::KeyboardConfig;

/// Number of pre-allocated GBM buffers. Three allows the compositor to
/// always have a free buffer: at most two frames are queued in the
/// `sync_channel(2)` and one is being processed by the encoder.
const BUFFER_POOL_SIZE: usize = 3;

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
pub enum OutputRenderElements {
	Space(SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>),
	Pointer(PointerRenderElement<GlesRenderer>),
}

impl Element for OutputRenderElements {
	fn id(&self) -> &Id {
		match self {
			Self::Space(e) => e.id(),
			Self::Pointer(e) => e.id(),
		}
	}

	fn current_commit(&self) -> CommitCounter {
		match self {
			Self::Space(e) => e.current_commit(),
			Self::Pointer(e) => e.current_commit(),
		}
	}

	fn geometry(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Rectangle<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.geometry(scale),
			Self::Pointer(e) => e.geometry(scale),
		}
	}

	fn src(&self) -> smithay::utils::Rectangle<f64, smithay::utils::Buffer> {
		match self {
			Self::Space(e) => e.src(),
			Self::Pointer(e) => e.src(),
		}
	}

	fn location(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Point<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.location(scale),
			Self::Pointer(e) => e.location(scale),
		}
	}

	fn transform(&self) -> smithay::utils::Transform {
		match self {
			Self::Space(e) => e.transform(),
			Self::Pointer(e) => e.transform(),
		}
	}

	fn damage_since(
		&self,
		scale: smithay::utils::Scale<f64>,
		commit: Option<CommitCounter>,
	) -> DamageSet<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.damage_since(scale, commit),
			Self::Pointer(e) => e.damage_since(scale, commit),
		}
	}

	fn opaque_regions(&self, scale: smithay::utils::Scale<f64>) -> OpaqueRegions<i32, smithay::utils::Physical> {
		match self {
			Self::Space(e) => e.opaque_regions(scale),
			Self::Pointer(e) => e.opaque_regions(scale),
		}
	}

	fn alpha(&self) -> f32 {
		match self {
			Self::Space(e) => e.alpha(),
			Self::Pointer(e) => e.alpha(),
		}
	}

	fn kind(&self) -> smithay::backend::renderer::element::Kind {
		match self {
			Self::Space(e) => e.kind(),
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
	) -> Result<(), GlesError> {
		match self {
			Self::Space(e) => e.draw(frame, src, dst, damage, opaque_regions),
			Self::Pointer(e) => e.draw(frame, src, dst, damage, opaque_regions),
		}
	}

	fn underlying_storage(
		&self,
		renderer: &mut GlesRenderer,
	) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
		match self {
			Self::Space(e) => e.underlying_storage(renderer),
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

/// Central compositor state for Moonshine's headless compositor.
///
/// Runs on a dedicated calloop thread. All Smithay delegate_*! macros
/// target this struct. No physical display — frames are rendered to
/// GBM buffers and exported to the video encoder.
#[allow(dead_code)]
pub struct MoonshineCompositor {
	// -- Wayland plumbing --
	pub display_handle: DisplayHandle,
	pub compositor_state: CompositorState,
	pub shm_state: ShmState,
	pub xdg_shell_state: XdgShellState,
	pub seat_state: SeatState<Self>,
	pub output_manager_state: OutputManagerState,
	pub data_device_state: DataDeviceState,

	// -- Rendering --
	pub output: Output,
	pub damage_tracker: OutputDamageTracker,
	pub allocator: GbmAllocator<std::fs::File>,
	pub renderer: GlesRenderer,

	// -- DMA-BUF --
	pub dmabuf_state: DmabufState,
	pub dmabuf_global: DmabufGlobal,

	// -- Frame relay to encoder --
	pub frame_tx: mpsc::SyncSender<ExportedFrame>,

	// -- Input --
	pub seat: Seat<Self>,

	// -- Cursor --
	pub cursor_position: Point<f64, Logical>,
	pub cursor_status: CursorImageStatus,
	pub pointer_element: PointerElement,
	pub last_pointer_activity: Option<std::time::Instant>,

	// -- Desktop --
	pub space: Space<smithay::desktop::Window>,
	pub clock: Clock<Monotonic>,

	// -- Lifecycle --
	pub handle: LoopHandle<'static, Self>,

	// -- Frame dimensions --
	pub width: u32,
	pub height: u32,

	// -- Render format --
	pub render_fourcc: Fourcc,
	pub render_modifiers: Vec<Modifier>,

	// -- Buffer pool --
	pub buffer_pool: Vec<GbmBufferSlot>,
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
	/// Cached cursor position from the last sent frame, to detect cursor-only
	/// changes without a surface commit.
	pub last_cursor_position: Point<f64, Logical>,

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
	/// Name of the compositor's Wayland socket in XDG_RUNTIME_DIR.
	pub wayland_display: String,

	/// Whether HDR mode is active for this session.
	pub hdr: bool,

	// -- WSI layer --
	/// Override surface from gamescope_swapchain.
	/// When set, this surface is rendered instead of the original X11 window.
	/// The u32 is the associated X11 window ID (0 for native Wayland).
	pub override_surface: Option<(WlSurface, u32)>,

	/// X11 window ID of the currently focused window (from Smithay's keyboard focus).
	/// Used by the WSI layer to match override surfaces to focused windows.
	pub focused_x11_window: Option<u32>,

	/// The actual Window that currently has keyboard focus (X11 or Wayland).
	/// Used to properly deactivate the old window when focus changes,
	/// especially for Wayland→Wayland transitions where focused_x11_window
	/// would be None for both old and new.
	pub focused_window: Option<smithay::desktop::Window>,

	/// Currently active override window (dropdown, menu, tooltip).
	/// Override windows are visually raised and may receive keyboard input
	/// while the primary focus remains on the main game window.
	/// Gamescope: `steamcompmgr_win_t::overrideWindow`
	pub override_window: Option<smithay::desktop::Window>,

	/// Currently active Steam overlay window (width > 1200 + STEAM_OVERLAY).
	/// Gamescope: `focus_t::overlayWindow` — the main Steam overlay window.
	pub overlay_window: Option<smithay::desktop::Window>,

	/// Currently active Steam notification window (width <= 1200 + STEAM_OVERLAY).
	/// Gamescope: `focus_t::notificationWindow` — small Steam notification popups.
	pub notification_window: Option<smithay::desktop::Window>,

	/// Currently active external overlay window (e.g., Discord, OBS).
	/// Gamescope: `focus_t::externalOverlayWindow` — non-Steam overlays.
	pub external_overlay_window: Option<smithay::desktop::Window>,

	/// Pointer focus window — where mouse/pointer events are routed.
	/// Separate from keyboard focus when an overlay has `inputFocusMode != 0`.
	/// Gamescope: `focus_t::inputFocusWindow` — where pointer/mouse events go.
	pub pointer_focus_window: Option<smithay::desktop::Window>,

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

	/// Set of X11 window IDs that have been identified as system tray icons
	/// via _NET_SYSTEM_TRAY_OPCODE REQUEST_DOCK messages. These windows are
	/// excluded from focus candidates via WindowFlags::SYS_TRAY_ICON.
	pub sys_tray_icons: std::collections::HashSet<u32>,

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
}

/// Client state required by Smithay's compositor.
pub struct ClientState {
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
		frame_tx: mpsc::SyncSender<ExportedFrame>,
		width: u32,
		height: u32,
		render_fourcc: Fourcc,
		render_modifiers: Vec<Modifier>,
		xdisplay_tx: mpsc::SyncSender<super::CompositorReady>,
		render_node: &std::path::Path,
		hdr: bool,
		keyboard_config: KeyboardConfig,
	) -> (Self, Display<Self>) {
		let compositor_state = CompositorState::new::<Self>(&display_handle);
		let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
		let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
		let mut seat_state = SeatState::new();
		let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
		let data_device_state = DataDeviceState::new::<Self>(&display_handle);
		let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
		RelativePointerManagerState::new::<Self>(&display_handle);
		PointerConstraintsState::new::<Self>(&display_handle);
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

		// Create seat with keyboard and pointer.
		let mut seat = seat_state.new_wl_seat(&display_handle, "moonshine");
		seat.add_keyboard(xkb_config, 200, 25)
			.expect("Failed to add keyboard to seat");
		seat.add_pointer();

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

		// Advertise wp_linux_dmabuf_v1 (version 5 with device feedback) so
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
		let color_management = if hdr {
			Some(super::color_management::ColorManagementState::new(&display_handle, hdr))
		} else {
			None
		};

		// Register swapchain protocol globals for WSI layer support.
		// Moonshine globals are always needed (for XWayland bypass, refresh_cycle, retire handling).
		// Gamescope globals are gated on HDR to avoid advertising HDR capability on SDR sessions.
		super::gamescope_swapchain::register_moonshine_globals(&display_handle);
		if hdr {
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
				output,
				damage_tracker,
				allocator,
				renderer,
				dmabuf_state,
				dmabuf_global,
				frame_tx,
				seat,
				cursor_position: Point::from((width as f64 / 2.0, height as f64 / 2.0)),
				cursor_status: CursorImageStatus::default_named(),
				pointer_element,
				last_pointer_activity: None,
				space,
				clock,
				handle,
				width,
				height,
				render_fourcc,
				render_modifiers,
				buffer_pool,
				next_buffer_index: 0,
				buffer_last_rendered_at: [None; BUFFER_POOL_SIZE],
				render_count: 0,
				screen_dirty: true,
				last_frame_sent_at: std::time::Instant::now(),
				last_cursor_position: Point::from((width as f64 / 2.0, height as f64 / 2.0)),
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
				override_surface: None,
				focused_x11_window: None,
				focused_window: None,
				override_window: None,
				overlay_window: None,
				notification_window: None,
				external_overlay_window: None,
				pointer_focus_window: None,
				damage_sequence_counter: 0,
				map_sequence_counter: 0,
				last_keyboard_focus_window: None,
				x11_focus: None,
				focus_state: super::focus::FocusState::default(),
				window_metadata: HashMap::new(),
				transient_children: std::collections::HashMap::new(),
				sys_tray_icons: std::collections::HashSet::new(),
				held_scanout_buffers: Vec::new(),
				scanout_buffer_map: std::collections::HashMap::new(),
				scanout_next_index: BUFFER_POOL_SIZE,
			},
			display,
		)
	}

	/// Render the current scene and export the frame to the encoder.
	pub fn render_and_export(&mut self) {
		// Detect cursor-only movement as a screen change.
		if self.cursor_position != self.last_cursor_position {
			self.screen_dirty = true;
			self.last_cursor_position = self.cursor_position;
		}

		// Skip rendering when the screen is static and we already sent a
		// keepalive frame within the last second.
		if !self.screen_dirty && self.last_frame_sent_at.elapsed() < std::time::Duration::from_secs(1) {
			return;
		}

		// Release held scanout buffers that the encoder has finished reading.
		// Drop their entries from the buffer→index map; if the same wl_buffer
		// is re-attached later it'll get a fresh index.
		self.held_scanout_buffers.retain(|(consumed, buffer_id, _)| {
			if consumed.load(Ordering::Acquire) {
				self.scanout_buffer_map.remove(buffer_id);
				false
			} else {
				true
			}
		});

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
		// Direct scanout bypasses the GLES compositor entirely, so the
		// cursor cannot be blended onto the frame.  Skip direct scanout
		// when the cursor is visible so that the GLES path composites the
		// cursor on top.
		let cursor_visible = self
			.last_pointer_activity
			.is_some_and(|t| t.elapsed() <= std::time::Duration::from_secs(3));
		if !cursor_visible {
			if self.is_override_active() {
				if self.try_direct_scanout_override() {
					tracing::trace!("Frame via direct scanout (override path)");
					return;
				}
			} else if self.try_direct_scanout() {
				tracing::trace!("Frame via direct scanout (not override path)");
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
		let exported_frame = match export_dmabuf(
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

		// Check before bind() to avoid borrow conflict with self.renderer.
		let override_active = self.is_override_active();

		// Bind the pre-allocated Dmabuf as a render target.
		let bind_result = self.renderer.bind(&mut self.buffer_pool[idx].dmabuf);
		let mut framebuffer = match bind_result {
			Ok(fb) => fb,
			Err(e) => {
				tracing::error!("Failed to bind Dmabuf for rendering: {e}");
				consumed.store(true, Ordering::Release);
				return;
			},
		};

		// Collect render elements from the space.
		let num_space_elements = self.space.elements().count();
		let space_elements: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> =
			match smithay::desktop::space::space_render_elements(&mut self.renderer, [&self.space], &self.output, 1.0) {
				Ok(elements) => elements,
				Err(e) => {
					tracing::error!("Failed to collect render elements: {e}");
					return;
				},
			};

		// Build cursor render elements.
		// Reset to the default named cursor if the client cursor surface is dead.
		let mut reset = false;
		if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
			reset = !surface.alive();
		}
		if reset {
			self.cursor_status = CursorImageStatus::default_named();
		}

		// Hide cursor if no pointer activity yet or inactive for 3 seconds.
		let cursor_status = if self
			.last_pointer_activity
			.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(3))
		{
			CursorImageStatus::Hidden
		} else {
			self.cursor_status.clone()
		};

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

		let scale = smithay::utils::Scale::from(1.0);
		let cursor_pos = self.cursor_position;
		let cursor_elements: Vec<OutputRenderElements> = self.pointer_element.render_elements(
			&mut self.renderer,
			(cursor_pos - cursor_hotspot.to_f64()).to_physical(scale).to_i32_round(),
			scale,
			1.0,
		);

		// Combine elements in front-to-back order: cursor first (on top), then space.
		let mut elements: Vec<OutputRenderElements> = Vec::with_capacity(cursor_elements.len() + space_elements.len());
		elements.extend(cursor_elements);

		// If the WSI layer created an override surface (via
		// override_window_content), render it instead of the XWayland
		// space elements — but only when the override's X11 window matches
		// the currently focused window (or is 0 with no X11 focus).
		if override_active {
			let Some((override_surface, _)) = self.override_surface.as_ref() else {
				tracing::warn!("override_active but override_surface is None");
				return;
			};
			let override_elements: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> =
				render_elements_from_surface_tree(
					&mut self.renderer,
					override_surface,
					(0, 0),
					1.0,
					1.0,
					Kind::Unspecified,
				);
			if override_elements.is_empty() {
				tracing::debug!("override active but surface has no committed buffer — rendering black");
			}
			elements.extend(override_elements.into_iter().map(OutputRenderElements::Space));
		} else {
			if self.override_surface.as_ref().is_some_and(|(s, _)| !s.alive()) {
				tracing::debug!("Override surface is dead, clearing.");
				self.override_surface = None;
			}
			elements.extend(space_elements.into_iter().map(OutputRenderElements::Space));
		}

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

		let render_result = self.damage_tracker.render_output(
			&mut self.renderer,
			&mut framebuffer,
			buffer_age,
			&elements,
			[0.0, 0.0, 0.0, 1.0], // black clear color
		);

		// Update the buffer's render count for future age calculations.
		self.buffer_last_rendered_at[idx] = Some(self.render_count);
		self.render_count += 1;

		match render_result {
			Ok(_) | Err(smithay::backend::renderer::damage::Error::OutputNoMode(_)) => {},
			Err(e) => {
				tracing::error!("Failed to render output: {e}");
				return;
			},
		}

		// Drop framebuffer before sending, to release the mutable borrow on dmabuf.
		drop(framebuffer);

		// Update created_at to reflect the actual render completion time.
		// The ExportedFrame was built before renderer.bind() (borrow workaround),
		// so the original timestamp is too early.
		let mut exported_frame = exported_frame;
		exported_frame.created_at = std::time::Instant::now();

		// Send the pre-built frame to the encoder.
		// The rendering happened after export_dmabuf duplicated the fds,
		// but the fds reference the same DMA-BUF — the encoder will see
		// the freshly rendered content.
		match self.frame_tx.try_send(exported_frame) {
			Err(mpsc::TrySendError::Disconnected(_)) => {
				tracing::debug!("Frame channel disconnected, compositor stopping.");
			},
			Err(mpsc::TrySendError::Full(_)) => {
				// Channel full — release the buffer back to the pool.
				consumed.store(true, Ordering::Release);
			},
			Ok(()) => {
				// Frame accepted — reset dirty tracking.
				self.screen_dirty = false;
				self.last_frame_sent_at = std::time::Instant::now();
			},
		}

		// Send frame callbacks to clients so they know to submit the
		// next buffer.
		self.space.elements().for_each(|window| {
			window.send_frame(
				&self.output,
				self.clock.now(),
				Some(std::time::Duration::ZERO),
				|_, _| Some(self.output.clone()),
			);
		});

		// Also send frame callbacks to the override surface if active,
		// so the NVIDIA driver's Wayland WSI unblocks and presents the
		// next frame.
		if let Some((ref override_surface, _)) = self.override_surface {
			if override_surface.alive() {
				send_frames_surface_tree(
					override_surface,
					&self.output,
					self.clock.now(),
					Some(std::time::Duration::ZERO),
					|_, _| Some(self.output.clone()),
				);

				// Drain and respond to wp_presentation_feedback callbacks
				// so the NVIDIA driver's WaitForPresentKHR can return.
				let mut feedback = OutputPresentationFeedback::new(&self.output);
				take_presentation_feedback_surface_tree(
					override_surface,
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
					smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty(),
				);
			}
		}

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
	/// - Exactly one window in the compositor space
	/// - The window's committed buffer is a DMA-BUF (not SHM)
	fn try_direct_scanout(&mut self) -> bool {
		// Must have exactly one window, no overlapping surfaces.
		let windows: Vec<_> = self.space.elements().cloned().collect();
		if windows.len() != 1 {
			tracing::trace!("Direct scanout: {} windows (need 1)", windows.len());
			return false;
		}

		let window = &windows[0];
		let toplevel = match window.toplevel() {
			Some(t) => t,
			None => {
				tracing::trace!("Direct scanout: window has no toplevel");
				return false;
			},
		};

		// The window must be positioned at the origin to avoid an offset frame.
		if let Some(geo) = self.space.element_geometry(window) {
			if geo.loc != Point::from((0, 0)) {
				tracing::trace!("Direct scanout: window not at origin ({:?})", geo.loc);
				return false;
			}
		}

		let wl_surface = toplevel.wl_surface().clone();

		// Get the committed buffer and check if it's a DMA-BUF.
		let scanout_buffer = with_renderer_surface_state(&wl_surface, |state| {
			let buffer = state.buffer()?;
			if !matches!(smithay::backend::renderer::buffer_type(buffer), Some(BufferType::Dma)) {
				tracing::trace!("Direct scanout: buffer is not DMA-BUF");
				return None;
			}
			Some(buffer.clone())
		});

		let Some(Some(buffer)) = scanout_buffer else {
			tracing::trace!("Direct scanout: no committed buffer");
			return false;
		};
		let Ok(client_dmabuf) = dmabuf::get_dmabuf(&buffer) else {
			tracing::trace!("Direct scanout: failed to get DMA-BUF from buffer");
			return false;
		};
		let client_dmabuf = client_dmabuf.clone();

		// The surface buffer must exactly match the output dimensions with
		// no scaling or offset, otherwise the encoder would receive a
		// partial or stretched frame.
		if client_dmabuf.width() != self.width || client_dmabuf.height() != self.height {
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
			.map(|cm| cm.frame_color_space())
			.unwrap_or(FrameColorSpace::Srgb);
		let hdr_metadata = self.color_management.as_ref().and_then(|cm| cm.hdr_metadata());

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
			planes,
			format: client_dmabuf.format().code as u32,
			modifier: Into::<u64>::into(client_dmabuf.format().modifier),
			width: client_dmabuf.width(),
			height: client_dmabuf.height(),
			created_at: std::time::Instant::now(),
			buffer_index,
			consumed: consumed.clone(),
			color_space,
			hdr_metadata,
		};

		// Hold the client Buffer alive until the encoder finishes reading.
		self.held_scanout_buffers.push((consumed.clone(), buffer_id, buffer));

		match self.frame_tx.try_send(exported_frame) {
			Err(mpsc::TrySendError::Disconnected(_)) => {
				tracing::debug!("Frame channel disconnected, compositor stopping.");
			},
			Err(mpsc::TrySendError::Full(_)) => {
				consumed.store(true, Ordering::Release);
			},
			Ok(()) => {
				self.screen_dirty = false;
				self.last_frame_sent_at = std::time::Instant::now();
			},
		}

		// Send frame callbacks to the client.
		window.send_frame(
			&self.output,
			self.clock.now(),
			Some(std::time::Duration::ZERO),
			|_, _| Some(self.output.clone()),
		);

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
	fn try_direct_scanout_override(&mut self) -> bool {
		let override_surface = match self.override_surface.as_ref() {
			Some((s, _)) if s.alive() => s.clone(),
			_ => return false,
		};

		let scanout_buffer = with_renderer_surface_state(&override_surface, |state| {
			let buffer = state.buffer()?;
			if !matches!(smithay::backend::renderer::buffer_type(buffer), Some(BufferType::Dma)) {
				tracing::trace!("Override scanout: buffer is not DMA-BUF");
				return None;
			}
			Some(buffer.clone())
		});
		let Some(Some(buffer)) = scanout_buffer else {
			tracing::trace!("Override scanout: no committed buffer");
			return false;
		};
		let Ok(client_dmabuf) = dmabuf::get_dmabuf(&buffer) else {
			tracing::trace!("Override scanout: failed to get DMA-BUF from buffer");
			return false;
		};
		let client_dmabuf = client_dmabuf.clone();

		if client_dmabuf.width() != self.width || client_dmabuf.height() != self.height {
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
			tracing::debug!(
				buffer_index = self.scanout_next_index - 1,
				fourcc = ?client_dmabuf.format().code,
				modifier = format!("{:#x}", Into::<u64>::into(client_dmabuf.format().modifier)).as_str(),
				num_planes = client_dmabuf.num_planes(),
				width = client_dmabuf.width(),
				height = client_dmabuf.height(),
				"Override scanout: importing new buffer",
			);
			idx
		});

		let consumed = Arc::new(AtomicBool::new(false));

		let color_space = self
			.color_management
			.as_ref()
			.map(|cm| cm.frame_color_space())
			.unwrap_or(FrameColorSpace::Srgb);
		let hdr_metadata = self.color_management.as_ref().and_then(|cm| cm.hdr_metadata());

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
			planes,
			format: client_dmabuf.format().code as u32,
			modifier: Into::<u64>::into(client_dmabuf.format().modifier),
			width: client_dmabuf.width(),
			height: client_dmabuf.height(),
			created_at: std::time::Instant::now(),
			buffer_index,
			consumed: consumed.clone(),
			color_space,
			hdr_metadata,
		};

		self.held_scanout_buffers.push((consumed.clone(), buffer_id, buffer));

		match self.frame_tx.try_send(exported_frame) {
			Err(mpsc::TrySendError::Disconnected(_)) => {
				tracing::debug!("Frame channel disconnected, compositor stopping.");
			},
			Err(mpsc::TrySendError::Full(_)) => {
				consumed.store(true, Ordering::Release);
			},
			Ok(()) => {
				self.screen_dirty = false;
				self.last_frame_sent_at = std::time::Instant::now();
			},
		}

		// Frame callbacks must go to the override surface (the game's WSI
		// layer is waiting on these to unblock vkQueuePresentKHR). Without
		// this the game would block forever after the first frame.
		send_frames_surface_tree(
			&override_surface,
			&self.output,
			self.clock.now(),
			Some(std::time::Duration::ZERO),
			|_, _| Some(self.output.clone()),
		);

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
		// Key the override against the currently focused X11 window rather than
		// the rendering child window reported by the WSI layer.  Wine/DXVK
		// renders via a child window whose XID differs from the WM-visible
		// top-level window tracked in `focused_x11_window`.  Using the child
		// window XID as the key would cause `is_override_active()` to never
		// match, leaving the override permanently inactive.
		//
		// When `focused_x11_window` is set, use it as the match key.
		// Fall back to the provided `x11_window` if there is no X11 focus
		// (e.g. for native Wayland apps using a temporary X11 sub-window).
		let focus_key = self.focused_x11_window.unwrap_or(x11_window);

		// Clear stale HDR state from the previous override surface when the
		// surface changes.  DXVK sometimes creates a new X11 window (and thus
		// a new wl_surface) when toggling HDR mode, so the old surface's
		// gamescope_current entry must be evicted explicitly — it won't be
		// cleaned up by create_swapchain (which only sees the new surface).
		if let Some(old_surface) = self.override_surface.as_ref().map(|(s, _)| s.clone()) {
			if old_surface != surface {
				if let Some(cm) = &mut self.color_management {
					cm.clear_gamescope_current(&old_surface);
				}
			}
		}

		tracing::debug!(x11_window, focus_key, "Storing override surface for X11 window");
		self.override_surface = Some((surface, focus_key));
	}

	/// Returns `true` when the WSI layer has an active override surface
	/// for the currently focused window.
	pub fn is_override_active(&self) -> bool {
		self.override_surface.as_ref().is_some_and(|(s, x11_win)| {
			s.alive()
				&& match *x11_win {
					0 => self.focused_x11_window.is_none(),
					id => self.focused_x11_window == Some(id),
				}
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
			if let Some(meta) = self.window_metadata.get(&override_win) {
				if let Some(parent_id) = meta.transient_for {
					if let Some(children) = self.transient_children.get_mut(&parent_id) {
						children.retain(|w| *w != override_win);
						if children.is_empty() {
							self.transient_children.remove(&parent_id);
						}
					}
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
		if self.notification_window.as_ref() == Some(&window) {
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
		if let Some(x11) = window.x11_surface() {
			if let Some(wl_surface) = x11.wl_surface() {
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
						time: self.clock.now().as_millis(),
					},
				);
				pointer.frame(self);
			}
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
			[("WAYLAND_DEBUG", "1")],
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

					let wm = smithay::xwayland::X11Wm::start_wm(data.handle.clone(), x11_socket, client.clone())
						.expect("Failed to start X11 window manager.");

					tracing::debug!(display_number, "XWayland ready.");

					data.xwm = Some(wm);
					data.xdisplay = Some(display_number);

					// Open an X11 connection to the XWayland display for
					// reading root window properties (Steam focus control).
					if data.x11_focus.is_none() {
						data.x11_focus = super::x11_focus::X11Focus::open(display_number);
					}

					// Notify the session thread that XWayland is ready.
					if let Some(tx) = data.xdisplay_tx.take() {
						let _ = tx.send(super::CompositorReady {
							xdisplay: display_number,
							wayland_display: data.wayland_display.clone(),
							hdr: data.hdr,
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

	/// Shut down the application and XWayland server.
	///
	/// Stops the application's systemd scope first so that X11 clients
	/// disconnect, then drops the X11 window manager connection. This
	/// ensures all clients are gone before Xwayland's `-terminate` flag
	/// triggers a clean exit.
	pub fn shutdown_session_processes(&mut self) {
		if let Some(token) = self.wayland_socket_token.take() {
			self.handle.remove(token);
			tracing::debug!(wayland_display = %self.wayland_display, "Removed Wayland listening socket source");
		}

		// Stop the application so all X11 clients disconnect from Xwayland.
		let status = std::process::Command::new("systemctl")
			.args(["--user", "stop", "moonshine-session.scope"])
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status();
		match status {
			Ok(s) if s.success() => tracing::debug!("Stopped moonshine-session.scope"),
			Ok(s) => tracing::debug!("systemctl stop exited with {s}"),
			Err(e) => tracing::warn!("Failed to stop moonshine-session.scope: {e}"),
		}

		// Clear the X11 focus control connection so reevaluate_focus
		// can't read from a dead X11 display during cleanup.
		if self.x11_focus.take().is_some() {
			tracing::debug!("Cleared X11 focus control connection");
		}

		// Drop the X11 window manager, closing the privileged WM
		// connection to Xwayland. Combined with the client disconnect
		// above, Xwayland will see no remaining connections.
		if self.xwm.take().is_some() {
			tracing::debug!("Dropped X11 window manager");
		}
	}
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
		planes,
		format: dmabuf.format().code as u32,
		modifier: Into::<u64>::into(dmabuf.format().modifier),
		width: dmabuf.width(),
		height: dmabuf.height(),
		created_at: std::time::Instant::now(),
		buffer_index,
		consumed,
		color_space: surface_color_space.unwrap_or(FrameColorSpace::Srgb),
		hdr_metadata,
	})
}
