//! Protocol regression tests against the actual Moonshine handlers.
//!
//! A Wayland client and server exchange messages over a private socket pair.
//! Surfaces use real SHM buffers; no GPU, network listener, or running session is needed.

#[path = "popup_lifecycle_tests.rs"]
mod lifecycle;

use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::desktop::PopupManager;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface as ServerSurface;
use smithay::reexports::wayland_server::{Client, Display};
use smithay::wayland::compositor::CompositorClientState;
use wayland_client::protocol::{
	wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
	wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};

use super::input::{CompositorInputEvent, process_input};
use super::state::{ClientState as ServerClientState, MoonshineCompositor};

#[path = "popup_input_tests.rs"]
mod popup_input;

#[path = "popup_serial_tests.rs"]
mod popup_serial;

#[path = "popup_touch_serial_tests.rs"]
mod popup_touch_serial;

#[path = "popup_parent_tests.rs"]
mod popup_parent;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ClientEvent {
	Configure(u32, u32),
	PopupConfigure(u32, (i32, i32, i32, i32)),
	Repositioned(u32, u32),
	PopupDone(u32),
	FrameDone(u32),
	Presented(u32),
	PresentationDiscarded(u32),
	PointerEnter(u32, f64, f64),
	PointerMotion(Option<u32>, f64, f64),
	PointerButton(Option<u32>, u32, u32, u32),
	PointerAxis(Option<u32>, u32, f64),
	OutputEnter(u32),
	OutputScale(i32),
	KeyboardEnter(u32),
	Key(Option<u32>, u32, u32, u32),
	TouchDown(u32, u32, i32, f64, f64),
	TouchMotion(i32, f64, f64),
	TouchUp(i32),
	TouchCancel,
}

#[derive(Default)]
pub(super) struct TestClient {
	compositor: Option<wl_compositor::WlCompositor>,
	shm: Option<wl_shm::WlShm>,
	shell: Option<xdg_wm_base::XdgWmBase>,
	presentation: Option<wp_presentation::WpPresentation>,
	pub(super) seat: Option<wl_seat::WlSeat>,
	pointer: Option<wl_pointer::WlPointer>,
	keyboard: Option<wl_keyboard::WlKeyboard>,
	touch: Option<wl_touch::WlTouch>,
	pointer_focus: Option<u32>,
	keyboard_focus: Option<u32>,
	sync_done: u32,
	pub(super) events: Vec<ClientEvent>,
}

pub(super) struct ClientSurface {
	pub(super) surface: wl_surface::WlSurface,
	pub(super) xdg_surface: xdg_surface::XdgSurface,
	pub(super) toplevel: xdg_toplevel::XdgToplevel,
}

pub(super) struct ClientPopup {
	pub(super) surface: wl_surface::WlSurface,
	pub(super) xdg_surface: xdg_surface::XdgSurface,
	pub(super) popup: xdg_popup::XdgPopup,
}

pub(super) struct Harness {
	pub(super) state: MoonshineCompositor,
	display: Display<MoonshineCompositor>,
	_event_loop: EventLoop<'static, MoonshineCompositor>,
	server_client: Client,
	connection: Connection,
	queue: EventQueue<TestClient>,
	pub(super) client: TestClient,
	pub(super) qh: QueueHandle<TestClient>,
	sync_requested: u32,
	buffers: Vec<wl_buffer::WlBuffer>,
}

/// An additional independent Wayland connection to the same compositor.
/// Swapping peers lets tests inspect both clients without a second server.
pub(super) struct Peer {
	server_client: Client,
	connection: Connection,
	queue: EventQueue<TestClient>,
	client: TestClient,
	qh: QueueHandle<TestClient>,
	sync_requested: u32,
	buffers: Vec<wl_buffer::WlBuffer>,
}

impl Harness {
	pub(super) fn add_client(&mut self) -> Peer {
		let (server_socket, client_socket) = UnixStream::pair().expect("additional client socket pair");
		let server_client = self
			.display
			.handle()
			.insert_client(
				server_socket,
				Arc::new(ServerClientState {
					compositor_state: CompositorClientState::default(),
				}),
			)
			.expect("insert additional test client");
		let connection = Connection::from_socket(client_socket).expect("additional Wayland connection");
		let queue = connection.new_event_queue();
		let qh = queue.handle();
		connection.display().get_registry(&qh, ());
		let mut old_peer = Peer {
			server_client,
			connection,
			queue,
			qh,
			client: TestClient::default(),
			sync_requested: 0,
			buffers: vec![],
		};
		self.swap_client(&mut old_peer);
		self.roundtrip();
		self.roundtrip();
		old_peer
	}

	pub(super) fn swap_client(&mut self, peer: &mut Peer) {
		std::mem::swap(&mut self.server_client, &mut peer.server_client);
		std::mem::swap(&mut self.connection, &mut peer.connection);
		std::mem::swap(&mut self.queue, &mut peer.queue);
		std::mem::swap(&mut self.client, &mut peer.client);
		std::mem::swap(&mut self.qh, &mut peer.qh);
		std::mem::swap(&mut self.sync_requested, &mut peer.sync_requested);
		std::mem::swap(&mut self.buffers, &mut peer.buffers);
	}

	pub(super) fn new() -> Self {
		let display = Display::new().expect("test Wayland display");
		let event_loop = EventLoop::try_new().expect("test event loop");
		let mut dh = display.handle();
		let state = MoonshineCompositor::for_test(dh.clone(), event_loop.handle());
		let (server_socket, client_socket) = UnixStream::pair().expect("test socket pair");
		let server_client = dh
			.insert_client(
				server_socket,
				Arc::new(ServerClientState {
					compositor_state: CompositorClientState::default(),
				}),
			)
			.expect("insert test client");
		let connection = Connection::from_socket(client_socket).expect("test Wayland connection");
		let queue = connection.new_event_queue();
		let qh = queue.handle();
		connection.display().get_registry(&qh, ());
		let mut harness = Self {
			state,
			display,
			_event_loop: event_loop,
			server_client,
			connection,
			queue,
			qh,
			client: TestClient::default(),
			sync_requested: 0,
			buffers: vec![],
		};
		harness.roundtrip();
		harness.roundtrip();
		assert!(harness.client.compositor.is_some());
		assert!(harness.client.shell.is_some());
		assert!(harness.client.shm.is_some());
		harness
	}

	/// Complete one explicit sync barrier without running the GPU frame timer.
	pub(super) fn roundtrip(&mut self) {
		self.sync_requested += 1;
		let target = self.sync_requested;
		self.connection.display().sync(&self.qh, target);
		let deadline = Instant::now() + Duration::from_secs(2);
		while self.client.sync_done < target {
			assert!(Instant::now() < deadline, "Wayland test sync {target} timed out");
			self.connection.flush().expect("flush test client");
			self.display
				.dispatch_clients(&mut self.state)
				.expect("dispatch test server");
			self.state.refresh_popups();
			self.display.flush_clients().expect("flush test server");
			self.queue
				.dispatch_pending(&mut self.client)
				.expect("dispatch pending client events");
			if let Some(read) = self.queue.prepare_read() {
				let mut poll_fd = libc::pollfd {
					fd: self.connection.backend().poll_fd().as_raw_fd(),
					events: libc::POLLIN,
					revents: 0,
				};
				// Only read when the private socket is ready; tests never block on a
				// missing configure event, which must be an ordinary assertion failure.
				if unsafe { libc::poll(&mut poll_fd, 1, 0) } > 0 {
					read.read().expect("read test client events");
				}
			}
			self.queue
				.dispatch_pending(&mut self.client)
				.expect("dispatch test client events");
		}
	}

	pub(super) fn toplevel(&mut self) -> ClientSurface {
		let surface = self.client.compositor.as_ref().unwrap().create_surface(&self.qh, ());
		let xdg_surface = self
			.client
			.shell
			.as_ref()
			.unwrap()
			.get_xdg_surface(&surface, &self.qh, ());
		let toplevel = xdg_surface.get_toplevel(&self.qh, ());
		surface.commit();
		self.roundtrip();
		ClientSurface {
			surface,
			xdg_surface,
			toplevel,
		}
	}

	pub(super) fn positioner(&self, geometry: (i32, i32, i32, i32)) -> xdg_positioner::XdgPositioner {
		let (x, y, width, height) = geometry;
		let positioner = self.client.shell.as_ref().unwrap().create_positioner(&self.qh, ());
		positioner.set_size(width, height);
		positioner.set_anchor_rect(x, y, 1, 1);
		positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
		positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
		positioner
	}

	pub(super) fn popup(&mut self, parent: &xdg_surface::XdgSurface, geometry: (i32, i32, i32, i32)) -> ClientPopup {
		let popup = self.uncommitted_popup(parent, geometry);
		popup.surface.commit();
		self.roundtrip();
		popup
	}

	pub(super) fn uncommitted_popup(
		&mut self,
		parent: &xdg_surface::XdgSurface,
		geometry: (i32, i32, i32, i32),
	) -> ClientPopup {
		let surface = self.client.compositor.as_ref().unwrap().create_surface(&self.qh, ());
		let xdg_surface = self
			.client
			.shell
			.as_ref()
			.unwrap()
			.get_xdg_surface(&surface, &self.qh, ());
		let positioner = self.positioner(geometry);
		let popup = xdg_surface.get_popup(Some(parent), &positioner, &self.qh, ());
		positioner.destroy();
		ClientPopup {
			surface,
			xdg_surface,
			popup,
		}
	}

	pub(super) fn map(&mut self, surface: &wl_surface::WlSurface, width: i32, height: i32) {
		let file = tempfile::tempfile().expect("SHM backing file");
		let length = width * height * 4;
		file.set_len(length as u64).expect("SHM backing length");
		let pool = self
			.client
			.shm
			.as_ref()
			.unwrap()
			.create_pool(file.as_fd(), length, &self.qh, ());
		let buffer = pool.create_buffer(0, width, height, width * 4, wl_shm::Format::Xrgb8888, &self.qh, ());
		surface.attach(Some(&buffer), 0, 0);
		surface.damage_buffer(0, 0, width, height);
		surface.commit();
		pool.destroy();
		self.buffers.push(buffer);
		self.roundtrip();
	}

	pub(super) fn server_surface(&self, surface: &wl_surface::WlSurface) -> ServerSurface {
		self.server_client
			.object_from_protocol_id(&self.display.handle(), surface.id().protocol_id())
			.expect("resolve client surface on server")
	}

	pub(super) fn input(&mut self, event: CompositorInputEvent) {
		process_input(event, &mut self.state);
		self.roundtrip();
	}

	pub(super) fn move_pointer(&mut self, x: i16, y: i16) {
		self.input(CompositorInputEvent::MouseMoveAbsolute {
			x,
			y,
			screen_width: 800,
			screen_height: 600,
		});
	}

	pub(super) fn press_pointer(&mut self) -> u32 {
		self.input(CompositorInputEvent::MouseButtonDown { button: 0x110 });
		self.client
			.events
			.iter()
			.rev()
			.find_map(|event| match event {
				ClientEvent::PointerButton(_, serial, _, 1) => Some(*serial),
				_ => None,
			})
			.expect("client received opening button press")
	}

	pub(super) fn request_frame(&self, surface: &wl_surface::WlSurface) -> u32 {
		let callback = surface.frame(&self.qh, 0);
		let id = callback.id().protocol_id();
		surface.commit();
		id
	}

	pub(super) fn frame_done(&self, id: u32) -> bool {
		self.client.events.contains(&ClientEvent::FrameDone(id))
	}

	pub(super) fn request_presentation(&self, surface: &wl_surface::WlSurface) -> u32 {
		let feedback = self
			.client
			.presentation
			.as_ref()
			.expect("presentation global")
			.feedback(surface, &self.qh, ());
		let id = feedback.id().protocol_id();
		surface.commit();
		id
	}

	pub(super) fn presentation_received(&self, id: u32) -> bool {
		self.client.events.contains(&ClientEvent::Presented(id))
	}
}

impl Dispatch<wl_registry::WlRegistry, ()> for TestClient {
	fn event(
		state: &mut Self,
		registry: &wl_registry::WlRegistry,
		event: wl_registry::Event,
		_: &(),
		_: &Connection,
		qh: &QueueHandle<Self>,
	) {
		if let wl_registry::Event::Global {
			name,
			interface,
			version,
		} = event
		{
			match interface.as_str() {
				"wl_compositor" => state.compositor = Some(registry.bind(name, version.min(6), qh, ())),
				"wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
				"xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(6), qh, ())),
				"wp_presentation" => state.presentation = Some(registry.bind(name, 1, qh, ())),
				"wl_output" => {
					let _: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
				},
				"wl_seat" => {
					let seat: wl_seat::WlSeat = registry.bind(name, version.min(9), qh, ());
					state.pointer = Some(seat.get_pointer(qh, ()));
					state.keyboard = Some(seat.get_keyboard(qh, ()));
					state.touch = Some(seat.get_touch(qh, ()));
					state.seat = Some(seat);
				},
				_ => {},
			}
		}
	}
}

impl Dispatch<wl_callback::WlCallback, u32> for TestClient {
	fn event(
		state: &mut Self,
		callback: &wl_callback::WlCallback,
		_: wl_callback::Event,
		cookie: &u32,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if *cookie == 0 {
			state.events.push(ClientEvent::FrameDone(callback.id().protocol_id()));
		} else {
			state.sync_done = *cookie;
		}
	}
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for TestClient {
	fn event(
		state: &mut Self,
		feedback: &wp_presentation_feedback::WpPresentationFeedback,
		event: wp_presentation_feedback::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let id = feedback.id().protocol_id();
		match event {
			wp_presentation_feedback::Event::Presented { .. } => state.events.push(ClientEvent::Presented(id)),
			wp_presentation_feedback::Event::Discarded => state.events.push(ClientEvent::PresentationDiscarded(id)),
			_ => {},
		}
	}
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for TestClient {
	fn event(
		_: &mut Self,
		shell: &xdg_wm_base::XdgWmBase,
		event: xdg_wm_base::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_wm_base::Event::Ping { serial } = event {
			shell.pong(serial);
		}
	}
}

impl Dispatch<xdg_surface::XdgSurface, ()> for TestClient {
	fn event(
		state: &mut Self,
		surface: &xdg_surface::XdgSurface,
		event: xdg_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_surface::Event::Configure { serial } = event {
			state
				.events
				.push(ClientEvent::Configure(surface.id().protocol_id(), serial));
			surface.ack_configure(serial);
		}
	}
}

impl Dispatch<xdg_popup::XdgPopup, ()> for TestClient {
	fn event(
		state: &mut Self,
		popup: &xdg_popup::XdgPopup,
		event: xdg_popup::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let id = popup.id().protocol_id();
		match event {
			xdg_popup::Event::Configure { x, y, width, height } => state
				.events
				.push(ClientEvent::PopupConfigure(id, (x, y, width, height))),
			xdg_popup::Event::PopupDone => state.events.push(ClientEvent::PopupDone(id)),
			xdg_popup::Event::Repositioned { token } => state.events.push(ClientEvent::Repositioned(id, token)),
			_ => {},
		}
	}
}

impl Dispatch<wl_pointer::WlPointer, ()> for TestClient {
	fn event(
		state: &mut Self,
		_: &wl_pointer::WlPointer,
		event: wl_pointer::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wl_pointer::Event::Enter {
				surface,
				surface_x,
				surface_y,
				..
			} => {
				let id = surface.id().protocol_id();
				state.pointer_focus = Some(id);
				state.events.push(ClientEvent::PointerEnter(id, surface_x, surface_y));
			},
			wl_pointer::Event::Leave { .. } => state.pointer_focus = None,
			wl_pointer::Event::Motion {
				surface_x, surface_y, ..
			} => state
				.events
				.push(ClientEvent::PointerMotion(state.pointer_focus, surface_x, surface_y)),
			wl_pointer::Event::Button {
				serial,
				button,
				state: pressed,
				..
			} => state.events.push(ClientEvent::PointerButton(
				state.pointer_focus,
				serial,
				button,
				pressed.into(),
			)),
			wl_pointer::Event::Axis { axis, value, .. } => {
				state
					.events
					.push(ClientEvent::PointerAxis(state.pointer_focus, axis.into(), value))
			},
			_ => {},
		}
	}
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for TestClient {
	fn event(
		state: &mut Self,
		_: &wl_keyboard::WlKeyboard,
		event: wl_keyboard::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wl_keyboard::Event::Enter { surface, .. } => {
				let id = surface.id().protocol_id();
				state.keyboard_focus = Some(id);
				state.events.push(ClientEvent::KeyboardEnter(id));
			},
			wl_keyboard::Event::Leave { .. } => state.keyboard_focus = None,
			wl_keyboard::Event::Key {
				serial,
				key,
				state: pressed,
				..
			} => state
				.events
				.push(ClientEvent::Key(state.keyboard_focus, serial, key, pressed.into())),
			_ => {},
		}
	}
}

impl Dispatch<wl_touch::WlTouch, ()> for TestClient {
	fn event(
		state: &mut Self,
		_: &wl_touch::WlTouch,
		event: wl_touch::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wl_touch::Event::Down {
				serial,
				surface,
				id,
				x,
				y,
				..
			} => state
				.events
				.push(ClientEvent::TouchDown(surface.id().protocol_id(), serial, id, x, y)),
			wl_touch::Event::Motion { id, x, y, .. } => state.events.push(ClientEvent::TouchMotion(id, x, y)),
			wl_touch::Event::Up { id, .. } => state.events.push(ClientEvent::TouchUp(id)),
			wl_touch::Event::Cancel => state.events.push(ClientEvent::TouchCancel),
			_ => {},
		}
	}
}

impl Dispatch<wl_surface::WlSurface, ()> for TestClient {
	fn event(
		state: &mut Self,
		surface: &wl_surface::WlSurface,
		event: wl_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let wl_surface::Event::Enter { .. } = event {
			state.events.push(ClientEvent::OutputEnter(surface.id().protocol_id()));
		}
	}
}

impl Dispatch<wl_output::WlOutput, ()> for TestClient {
	fn event(
		state: &mut Self,
		_: &wl_output::WlOutput,
		event: wl_output::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let wl_output::Event::Scale { factor } = event {
			state.events.push(ClientEvent::OutputScale(factor));
		}
	}
}

delegate_noop!(TestClient: ignore wl_compositor::WlCompositor);
delegate_noop!(TestClient: ignore wp_presentation::WpPresentation);
delegate_noop!(TestClient: ignore wl_region::WlRegion);
delegate_noop!(TestClient: ignore wl_shm::WlShm);
delegate_noop!(TestClient: ignore wl_shm_pool::WlShmPool);
delegate_noop!(TestClient: ignore wl_buffer::WlBuffer);
delegate_noop!(TestClient: ignore wl_seat::WlSeat);
delegate_noop!(TestClient: ignore xdg_positioner::XdgPositioner);
delegate_noop!(TestClient: ignore xdg_toplevel::XdgToplevel);

#[test]
fn popup_initial_configure_is_sent_after_initial_surface_commit() {
	let mut harness = Harness::new();
	let parent = harness.toplevel();
	harness.map(&parent.surface, 800, 600);
	let popup = harness.uncommitted_popup(&parent.xdg_surface, (120, 80, 160, 100));
	harness.roundtrip();
	let popup_id = popup.popup.id().protocol_id();
	assert!(
		!harness
			.client
			.events
			.iter()
			.any(|event| matches!(event, ClientEvent::PopupConfigure(id, _) if *id == popup_id)),
		"popup must wait for initial wl_surface.commit"
	);
	popup.surface.commit();
	harness.roundtrip();
	assert!(
		harness
			.client
			.events
			.contains(&ClientEvent::PopupConfigure(popup_id, (120, 80, 160, 100))),
		"first commit must configure the requested popup geometry; events: {:?}",
		harness.client.events
	);
	assert!(
		harness
			.client
			.events
			.iter()
			.any(|event| matches!(event, ClientEvent::Configure(id, _) if *id == popup.xdg_surface.id().protocol_id())),
		"xdg_surface.configure must follow xdg_popup.configure"
	);
	let parent_surface = harness.server_surface(&parent.surface);
	assert_eq!(
		PopupManager::popups_for_surface(&parent_surface).count(),
		1,
		"popup must be registered for rendering and hit testing"
	);
}
