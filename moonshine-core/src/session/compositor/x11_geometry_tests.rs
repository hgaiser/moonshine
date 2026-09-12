use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
	AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, MapState, PropMode,
	WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::*;

struct GameProcess(Child);

impl Drop for GameProcess {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

fn wait_until(description: &str, mut condition: impl FnMut() -> bool) {
	let deadline = Instant::now() + Duration::from_secs(5);
	while !condition() {
		assert!(Instant::now() < deadline, "timed out: {description}");
		std::thread::sleep(Duration::from_millis(10));
	}
}

#[test]
#[ignore = "requires a DRM render node, Xwayland and XDG_RUNTIME_DIR"]
fn test_x11_geometry_lifecycle() {
	let stop = ShutdownManager::new();
	let cleanup = stop.trigger_shutdown_token(SessionShutdownReason::UserStopped);
	let (compositor, _handles) = Compositor::new(
		CompositorConfig {
			hdr: false,
			..Default::default()
		},
		CompositorContext {
			width: 1280,
			height: 720,
			refresh_rate: 60,
			hdr: false,
		},
		stop.clone(),
	);
	let compositor = compositor.launch().expect("start test compositor");
	let display = format!(":{}", compositor.ready().xdisplay);
	let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
	let root = conn.setup().roots[screen].root;
	let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
	let wm_state = atom(b"_NET_WM_STATE");
	let fullscreen = atom(b"_NET_WM_STATE_FULLSCREEN");
	let pid = atom(b"_NET_WM_PID");
	let game = GameProcess(Command::new("sleep").arg0("AppId=12345").arg("60").spawn().unwrap());

	let window = conn.generate_id().unwrap();
	conn.create_window(
		x11rb::COPY_DEPTH_FROM_PARENT,
		window,
		root,
		0,
		0,
		800,
		600,
		0,
		WindowClass::INPUT_OUTPUT,
		0,
		&CreateWindowAux::new().background_pixel(0x808080),
	)
	.unwrap()
	.check()
	.unwrap();
	conn.map_window(window).unwrap().check().unwrap();
	conn.flush().unwrap();
	wait_until("window mapped", || {
		conn.get_window_attributes(window).unwrap().reply().unwrap().map_state == MapState::VIEWABLE
	});
	let size = || {
		let geo = conn.get_geometry(window).unwrap().reply().unwrap();
		(geo.width, geo.height)
	};
	assert_eq!(size(), (800, 600), "mapping must preserve client size");

	// Classification must not turn a windowed client into a fullscreen one.
	conn.change_property32(PropMode::REPLACE, window, pid, AtomEnum::CARDINAL, &[game.0.id()])
		.unwrap()
		.check()
		.unwrap();
	conn.configure_window(window, &ConfigureWindowAux::new().width(900).height(650))
		.unwrap()
		.check()
		.unwrap();
	conn.flush().unwrap();
	wait_until("windowed configure request", || size() == (900, 650));

	// A fresh mapping without _NET_WM_PID exercises fullscreen before classification.
	conn.unmap_window(window).unwrap().check().unwrap();
	wait_until("window unmapped", || {
		conn.get_window_attributes(window).unwrap().reply().unwrap().map_state == MapState::UNMAPPED
	});
	conn.delete_property(window, pid).unwrap().check().unwrap();
	conn.map_window(window).unwrap().check().unwrap();
	wait_until("window remapped", || {
		conn.get_window_attributes(window).unwrap().reply().unwrap().map_state == MapState::VIEWABLE
	});
	let set_fullscreen = |enabled: bool| {
		let event = ClientMessageEvent::new(32, window, wm_state, [u32::from(enabled), fullscreen, 0, 1, 0]);
		conn.send_event(
			false,
			root,
			EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
			event,
		)
		.unwrap()
		.check()
		.unwrap();
		conn.flush().unwrap();
		wait_until("fullscreen acknowledgement", || {
			let reply = conn
				.get_property(false, window, wm_state, AtomEnum::ATOM, 0, 32)
				.unwrap()
				.reply()
				.unwrap();
			reply
				.value32()
				.is_some_and(|mut values| values.any(|value| value == fullscreen))
				== enabled
		});
	};
	set_fullscreen(true);
	wait_until("fullscreen without an app ID", || size() == (1280, 720));

	conn.configure_window(window, &ConfigureWindowAux::new().width(700).height(500))
		.unwrap()
		.check()
		.unwrap();
	conn.flush().unwrap();
	// Observe a settling interval: accepting 700x500 even briefly is a regression.
	let deadline = Instant::now() + Duration::from_millis(200);
	while Instant::now() < deadline {
		assert_eq!(size(), (1280, 720), "fullscreen configure must retain output size");
		std::thread::sleep(Duration::from_millis(10));
	}

	set_fullscreen(false);
	conn.configure_window(window, &ConfigureWindowAux::new().width(800).height(600))
		.unwrap()
		.check()
		.unwrap();
	conn.flush().unwrap();
	wait_until("windowed geometry after leaving fullscreen", || size() == (800, 600));
	conn.destroy_window(window).unwrap().check().unwrap();
	drop(conn);
	drop(cleanup);
	wait_until("compositor shutdown", || stop.is_shutdown_completed());
}
