//! GPU-free construction for tests of the production compositor protocol handlers.

use super::*;
use smithay::output::{Mode, PhysicalProperties, Subpixel};
use smithay::utils::Transform;

impl MoonshineCompositor {
	pub(crate) fn for_test(display_handle: DisplayHandle, handle: LoopHandle<'static, Self>) -> Self {
		let compositor_state = CompositorState::new_v6::<Self>(&display_handle);
		let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
		let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
		let mut seat_state = SeatState::new();
		let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
		let data_device_state = DataDeviceState::new::<Self>(&display_handle);
		let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
		RelativePointerManagerState::new::<Self>(&display_handle);
		PointerConstraintsState::new::<Self>(&display_handle);
		TabletManagerState::new::<Self>(&display_handle);
		smithay::wayland::presentation::PresentationState::new::<Self>(&display_handle, 1);
		let viewporter_state = smithay::wayland::viewporter::ViewporterState::new::<Self>(&display_handle);
		let mut seat = seat_state.new_wl_seat(&display_handle, "moonshine-test");
		seat.add_keyboard(XkbConfig::default(), 200, 25).expect("test keyboard");
		seat.add_pointer();
		seat.add_touch();
		let pen_tablet_descriptor = TabletDescriptor {
			name: "Moonlight Pen".into(),
			usb_id: None,
			syspath: None,
		};
		seat.tablet_seat()
			.add_tablet::<Self>(&display_handle, &pen_tablet_descriptor);
		let output = Output::new(
			"moonshine-test".into(),
			PhysicalProperties {
				size: (0, 0).into(),
				subpixel: Subpixel::Unknown,
				make: "Moonshine".into(),
				model: "Protocol test".into(),
				serial_number: "test".into(),
			},
		);
		let mode = Mode {
			size: (800, 600).into(),
			refresh: 60_000,
		};
		output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
		output.set_preferred(mode);
		output.create_global::<Self>(&display_handle);
		let damage_tracker = OutputDamageTracker::from_output(&output);
		let mut space = Space::default();
		space.map_output(&output, (0, 0));
		let mut dmabuf_state = DmabufState::new();
		let dmabuf_global = dmabuf_state.create_global::<Self>(&display_handle, vec![]);
		let (frame_tx, _frame_rx) = mpsc::sync_channel(1);
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
			renderer: None,
			dmabuf_state,
			dmabuf_global,
			frame_tx,
			seat,
			cursor_position: (400.0, 300.0).into(),
			cursor_status: CursorImageStatus::default_named(),
			pointer_element: PointerElement::default(),
			last_pointer_activity: None,
			pen_tablet_descriptor,
			active_pen_tool_kind: None,
			pen_buttons: 0,
			space,
			popups: smithay::desktop::PopupManager::default(),
			popup_grab: None,
			popup_pointer_target: None,
			input_serials: Default::default(),
			clock: Clock::new(),
			handle,
			width: 800,
			height: 600,
			render_fourcc: Fourcc::Xrgb8888,
			render_modifiers: vec![],
			buffer_pool: vec![],
			next_buffer_index: 0,
			buffer_last_rendered_at: [None; BUFFER_POOL_SIZE],
			render_count: 0,
			screen_dirty: true,
			last_frame_sent_at: std::time::Instant::now(),
			last_cursor_position: (400.0, 300.0).into(),
			viewporter_state,
			color_management: None,
			deferred_info_done: vec![],
			xwayland_shell_state,
			xwm: None,
			xdisplay: None,
			xdisplay_tx: None,
			wayland_socket_token: None,
			wayland_display: String::new(),
			hdr: false,
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
			focus_state: super::super::focus::FocusState::default(),
			window_metadata: HashMap::new(),
			transient_children: HashMap::new(),
			sys_tray_icons: std::collections::HashSet::new(),
			held_scanout_buffers: vec![],
			scanout_buffer_map: HashMap::new(),
			scanout_next_index: BUFFER_POOL_SIZE,
			last_scanout_buffer_desc: None,
		}
	}
}
