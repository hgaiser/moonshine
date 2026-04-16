#![allow(non_upper_case_globals, non_camel_case_types)]

use smithay::reexports::wayland_server;
use wayland_server::protocol::*;

pub mod __interfaces {
	use super::wayland_server;
	use wayland_server::backend as wayland_backend;
	use wayland_server::protocol::__interfaces::*;
	wayland_scanner::generate_interfaces!("src/session/compositor/protocols/gamescope-swapchain.xml");
}

pub mod __moonshine_interfaces {
	use super::wayland_server;
	use wayland_server::backend as wayland_backend;
	use wayland_server::protocol::__interfaces::*;
	wayland_scanner::generate_interfaces!("src/session/compositor/protocols/moonshine-swapchain.xml");
}

use self::__interfaces::*;
wayland_scanner::generate_server_code!("src/session/compositor/protocols/gamescope-swapchain.xml");

use self::__moonshine_interfaces::*;
wayland_scanner::generate_server_code!("src/session/compositor/protocols/moonshine-swapchain.xml");
