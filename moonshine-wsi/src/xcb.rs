//! X11/XCB runtime bindings loaded via `dlsym`.
//!
//! This module isolates all X11/XCB FFI from the rest of the layer.
//! Functions are resolved lazily on first use and cached in `OnceLock`.

// ---------------------------------------------------------------------------
// XCB geometry types
// ---------------------------------------------------------------------------

#[repr(C)]
struct XcbGetGeometryCookie {
	sequence: u32,
}

#[repr(C)]
struct XcbGetGeometryReply {
	response_type: u8,
	depth: u8,
	sequence: u16,
	length: u32,
	root: u32,
	x: i16,
	y: i16,
	width: u16,
	height: u16,
	border_width: u16,
}

type FnXcbGetGeometry = unsafe extern "C" fn(*mut libc::c_void, u32) -> XcbGetGeometryCookie;
type FnXcbGetGeometryReply =
	unsafe extern "C" fn(*mut libc::c_void, XcbGetGeometryCookie, *mut *mut libc::c_void) -> *mut XcbGetGeometryReply;

// ---------------------------------------------------------------------------
// XGetXCBConnection (libX11-xcb)
// ---------------------------------------------------------------------------

/// Convert a libX11 `Display*` to an `xcb_connection_t*` (opaque) using
/// `XGetXCBConnection` from `libX11-xcb`.
pub(crate) unsafe fn xlib_to_xcb_connection(dpy: *mut std::ffi::c_void) -> *mut libc::c_void {
	unsafe {
		use std::sync::OnceLock;
		type FnXGetXCBConnection = unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void;

		static XCB_CONN_SYM: OnceLock<Option<FnXGetXCBConnection>> = OnceLock::new();

		let sym = XCB_CONN_SYM.get_or_init(|| {
			// Clear any lingering error before dlopen.
			libc::dlerror();
			let lib = libc::dlopen(c"libX11-xcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
			if lib.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!(" dlopen(libX11-xcb.so.1) failed: {}", err.to_string_lossy());
				} else {
					crate::log_error!(" dlopen(libX11-xcb.so.1) failed: unknown error");
				}
				return None;
			}
			// Clear before dlsym to distinguish symbol-not-found from prior errors.
			libc::dlerror();
			let sym = libc::dlsym(lib, c"XGetXCBConnection".as_ptr());
			if sym.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!(" dlsym(XGetXCBConnection) failed: {}", err.to_string_lossy());
				} else {
					crate::log_error!(" dlsym(XGetXCBConnection) failed: symbol not found");
				}
				return None;
			}
			Some(std::mem::transmute(sym))
		});

		match sym {
			Some(f) => f(dpy),
			None => std::ptr::null_mut(),
		}
	}
}

// ---------------------------------------------------------------------------
// xcb_get_geometry (libxcb)
// ---------------------------------------------------------------------------

/// Query the current geometry of an X11 window via XCB.
///
/// Returns `(width, height)` on success, `None` if the connection pointer is
/// null or the XCB call fails.  Uses `dlsym` to locate `xcb_get_geometry`
/// and `xcb_get_geometry_reply` at runtime so we don't need a link-time
/// dependency on libxcb.
pub(crate) unsafe fn xcb_get_window_extent(connection: *mut libc::c_void, window: u32) -> Option<(u32, u32)> {
	unsafe {
		use std::sync::OnceLock;

		static XCB_FNS: OnceLock<Option<(FnXcbGetGeometry, FnXcbGetGeometryReply)>> = OnceLock::new();

		let fns = XCB_FNS.get_or_init(|| {
			// Clear any lingering error before dlopen.
			libc::dlerror();
			let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
			if lib.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!(" dlopen(libxcb.so.1) failed: {}", err.to_string_lossy());
				} else {
					crate::log_error!(" dlopen(libxcb.so.1) failed: unknown error");
				}
				return None;
			}
			// Clear before dlsym.
			libc::dlerror();
			let get_geom = libc::dlsym(lib, c"xcb_get_geometry".as_ptr());
			libc::dlerror();
			let get_reply = libc::dlsym(lib, c"xcb_get_geometry_reply".as_ptr());
			if get_geom.is_null() || get_reply.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!(" dlsym(xcb_get_geometry) failed: {}", err.to_string_lossy());
				} else {
					crate::log_error!(" dlsym(xcb_get_geometry) failed: symbol not found");
				}
				return None;
			}
			Some((std::mem::transmute(get_geom), std::mem::transmute(get_reply)))
		});

		let (get_geometry, get_geometry_reply) = (*fns)?;

		if connection.is_null() {
			return None;
		}

		let cookie = get_geometry(connection, window);
		let reply = get_geometry_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let w = (*reply).width as u32;
		let h = (*reply).height as u32;
		libc::free(reply as *mut libc::c_void);

		if w == 0 || h == 0 {
			return None;
		}

		Some((w, h))
	}
}

// ---------------------------------------------------------------------------
// XCB query_tree types
// ---------------------------------------------------------------------------

#[repr(C)]
struct XcbQueryTreeCookie {
	sequence: u32,
}

#[repr(C)]
struct XcbQueryTreeReply {
	response_type: u8,
	pad0: u8,
	sequence: u16,
	length: u32,
	root: u32,
	parent: u32,
	children_len: u16,
	pad: [u8; 14],
}

type FnXcbQueryTree = unsafe extern "C" fn(*mut libc::c_void, u32) -> XcbQueryTreeCookie;
type FnXcbQueryTreeReply =
	unsafe extern "C" fn(*mut libc::c_void, XcbQueryTreeCookie, *mut *mut libc::c_void) -> *mut XcbQueryTreeReply;

// ---------------------------------------------------------------------------
// XCB get_window_attributes types
// ---------------------------------------------------------------------------

#[repr(C)]
struct XcbGetWindowAttributesCookie {
	sequence: u32,
}

#[repr(C)]
struct XcbGetWindowAttributesReply {
	response_type: u8,
	backing_store: u8,
	sequence: u16,
	length: u32,
	visual: u32,
	_class: u16,
	bit_gravity: u8,
	win_gravity: u8,
	backing_planes: u32,
	backing_pixel: u32,
	save_under: u8,
	map_is_installed: u8,
	map_state: u8,
	override_redirect: u8,
	colormap: u32,
	all_event_masks: u32,
	your_event_mask: u32,
	do_not_propagate_mask: u16,
	pad0: [u8; 2],
}

type FnXcbGetWindowAttributes = unsafe extern "C" fn(*mut libc::c_void, u32) -> XcbGetWindowAttributesCookie;
type FnXcbGetWindowAttributesReply = unsafe extern "C" fn(
	*mut libc::c_void,
	XcbGetWindowAttributesCookie,
	*mut *mut libc::c_void,
) -> *mut XcbGetWindowAttributesReply;

const XCB_MAP_STATE_VIEWABLE: u8 = 2;

// ---------------------------------------------------------------------------
// Shared libxcb function pointers (query_tree + get_window_attributes)
// ---------------------------------------------------------------------------

type XcbQueryTreeFns = (
	FnXcbQueryTree,
	FnXcbQueryTreeReply,
	FnXcbGetWindowAttributes,
	FnXcbGetWindowAttributesReply,
);

fn load_xcb_query_tree_fns() -> Option<XcbQueryTreeFns> {
	use std::sync::OnceLock;
	static FNS: OnceLock<Option<XcbQueryTreeFns>> = OnceLock::new();

	unsafe {
		*FNS.get_or_init(|| {
			libc::dlerror();
			let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
			if lib.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!("dlopen(libxcb.so.1) failed: {}", err.to_string_lossy());
				}
				return None;
			}

			libc::dlerror();
			let query_tree = libc::dlsym(lib, c"xcb_query_tree".as_ptr());
			libc::dlerror();
			let query_tree_reply = libc::dlsym(lib, c"xcb_query_tree_reply".as_ptr());
			libc::dlerror();
			let get_wa = libc::dlsym(lib, c"xcb_get_window_attributes".as_ptr());
			libc::dlerror();
			let get_wa_reply = libc::dlsym(lib, c"xcb_get_window_attributes_reply".as_ptr());

			if query_tree.is_null() || query_tree_reply.is_null() || get_wa.is_null() || get_wa_reply.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!(
						"dlsym(xcb_query_tree/xcb_get_window_attributes) failed: {}",
						err.to_string_lossy()
					);
				}
				return None;
			}

			Some((
				std::mem::transmute(query_tree),
				std::mem::transmute(query_tree_reply),
				std::mem::transmute(get_wa),
				std::mem::transmute(get_wa_reply),
			))
		})
	}
}

/// Query the full geometry of an X11 window via XCB.
pub(crate) unsafe fn xcb_get_window_rect(connection: *mut libc::c_void, window: u32) -> Option<(i16, i16, u32, u32)> {
	unsafe {
		use std::sync::OnceLock;
		static XCB_FNS: OnceLock<Option<(FnXcbGetGeometry, FnXcbGetGeometryReply)>> = OnceLock::new();

		let fns = XCB_FNS.get_or_init(|| {
			libc::dlerror();
			let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
			if lib.is_null() {
				return None;
			}
			libc::dlerror();
			let get_geom = libc::dlsym(lib, c"xcb_get_geometry".as_ptr());
			libc::dlerror();
			let get_reply = libc::dlsym(lib, c"xcb_get_geometry_reply".as_ptr());
			if get_geom.is_null() || get_reply.is_null() {
				return None;
			}
			Some((std::mem::transmute(get_geom), std::mem::transmute(get_reply)))
		});

		let (get_geometry, get_geometry_reply) = (*fns)?;
		if connection.is_null() {
			return None;
		}

		let cookie = get_geometry(connection, window);
		let reply = get_geometry_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let x = (*reply).x;
		let y = (*reply).y;
		let w = (*reply).width as u32;
		let h = (*reply).height as u32;
		libc::free(reply as *mut libc::c_void);

		Some((x, y, w, h))
	}
}

/// Query the X11 window tree for a given window.
pub(crate) unsafe fn xcb_query_tree_window(connection: *mut libc::c_void, window: u32) -> Option<(u32, u32, u32)> {
	unsafe {
		let fns = load_xcb_query_tree_fns()?;
		let (query_tree, query_tree_reply, _, _) = fns;

		if connection.is_null() {
			return None;
		}

		let cookie = query_tree(connection, window);
		let reply = query_tree_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let root = (*reply).root;
		let parent = (*reply).parent;
		let children_len = u32::from((*reply).children_len);
		libc::free(reply as *mut libc::c_void);

		Some((root, parent, children_len))
	}
}

/// Get the `map_state` and `override_redirect` flags for an X11 window.
pub(crate) unsafe fn xcb_get_window_attributes(connection: *mut libc::c_void, window: u32) -> Option<(u8, bool)> {
	unsafe {
		let fns = load_xcb_query_tree_fns()?;
		let (_, _, get_wa, get_wa_reply) = fns;

		if connection.is_null() {
			return None;
		}

		let cookie = get_wa(connection, window);
		let reply = get_wa_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let map_state = (*reply).map_state;
		let override_redirect = (*reply).override_redirect != 0;
		libc::free(reply as *mut libc::c_void);

		Some((map_state, override_redirect))
	}
}

/// Find visible obstructions, excluding the rendering branch. Child coordinates
/// are parent-relative; clip to the parent before assessing coverage.
pub(crate) unsafe fn xcb_get_largest_obscuring_child(
	connection: *mut libc::c_void,
	window: u32,
	content_child: Option<u32>,
) -> Option<Option<(u32, u32)>> {
	unsafe {
		use std::sync::OnceLock;
		static FNS: OnceLock<Option<(FnXcbQueryTree, FnXcbQueryTreeReply)>> = OnceLock::new();

		let fns_opt = FNS.get_or_init(|| {
			libc::dlerror();
			let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
			if lib.is_null() {
				return None;
			}
			libc::dlerror();
			let qt = libc::dlsym(lib, c"xcb_query_tree".as_ptr());
			libc::dlerror();
			let qtr = libc::dlsym(lib, c"xcb_query_tree_reply".as_ptr());
			if qt.is_null() || qtr.is_null() {
				return None;
			}
			Some((std::mem::transmute(qt), std::mem::transmute(qtr)))
		});

		let (query_tree, query_tree_reply) = (*fns_opt)?;

		if connection.is_null() {
			return None;
		}

		let cookie = query_tree(connection, window);
		let reply = query_tree_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let children_len = u32::from((*reply).children_len);
		let parent_rect = xcb_get_window_rect(connection, window);
		if parent_rect.is_none() {
			libc::free(reply as *mut libc::c_void);
			return None;
		}
		let (_, _, pw, ph) = parent_rect.unwrap();

		let mut max_w: u32 = 0;
		let mut max_h: u32 = 0;

		if children_len > 0 {
			let children = (reply as *const u32).add(std::mem::size_of::<XcbQueryTreeReply>() / 4);
			for i in 0..children_len as isize {
				let child = *children.add(i as usize);
				// The rendering window's ancestor is content, not an obstruction.
				if Some(child) == content_child {
					continue;
				}
				if let Some((map_state, override_redirect)) = xcb_get_window_attributes(connection, child)
					&& map_state == XCB_MAP_STATE_VIEWABLE
					&& !override_redirect
					&& let Some((cx, cy, cw, ch)) = xcb_get_window_rect(connection, child)
				{
					// XCB child geometry is relative to its parent, not root space.
					let final_w = clipped_child_span(cx, cw, pw);
					let final_h = clipped_child_span(cy, ch, ph);
					if final_w > max_w {
						max_w = final_w;
					}
					if final_h > max_h {
						max_h = final_h;
					}
				}
			}
		}

		libc::free(reply as *mut libc::c_void);

		if max_w <= 1 && max_h <= 1 {
			Some(None)
		} else {
			Some(Some((max_w, max_h)))
		}
	}
}

fn clipped_child_span(position: i16, size: u32, parent_size: u32) -> u32 {
	let start = i64::from(position).max(0);
	let end = (i64::from(position) + i64::from(size)).min(i64::from(parent_size));
	(end - start).max(0) as u32
}

// ---------------------------------------------------------------------------
// XCB property helpers (libxcb)
// ---------------------------------------------------------------------------

/// Predefined `XCB_ATOM_CARDINAL`.
const XCB_ATOM_CARDINAL: u32 = 6;
/// Predefined `XCB_ATOM_WM_CLASS`.
pub(crate) const XCB_ATOM_WM_CLASS: u32 = 67;
/// `XCB_GET_PROPERTY_TYPE_ANY`: match a property regardless of its type.
const XCB_GET_PROPERTY_TYPE_ANY: u32 = 0;

#[repr(C)]
struct XcbCookie {
	sequence: u32,
}

#[repr(C)]
struct XcbInternAtomReply {
	response_type: u8,
	pad0: u8,
	sequence: u16,
	length: u32,
	atom: u32,
}

#[repr(C)]
struct XcbGetPropertyReply {
	response_type: u8,
	format: u8,
	sequence: u16,
	length: u32,
	type_: u32,
	bytes_after: u32,
	value_len: u32,
	pad0: [u8; 12],
}

type FnXcbInternAtom = unsafe extern "C" fn(*mut libc::c_void, u8, u16, *const std::ffi::c_char) -> XcbCookie;
type FnXcbInternAtomReply =
	unsafe extern "C" fn(*mut libc::c_void, XcbCookie, *mut *mut libc::c_void) -> *mut XcbInternAtomReply;
type FnXcbGetProperty = unsafe extern "C" fn(*mut libc::c_void, u8, u32, u32, u32, u32, u32) -> XcbCookie;
type FnXcbGetPropertyReply =
	unsafe extern "C" fn(*mut libc::c_void, XcbCookie, *mut *mut libc::c_void) -> *mut XcbGetPropertyReply;

type XcbPropertyFns = (
	FnXcbInternAtom,
	FnXcbInternAtomReply,
	FnXcbGetProperty,
	FnXcbGetPropertyReply,
);

fn load_xcb_property_fns() -> Option<XcbPropertyFns> {
	use std::sync::OnceLock;
	static FNS: OnceLock<Option<XcbPropertyFns>> = OnceLock::new();

	unsafe {
		*FNS.get_or_init(|| {
			libc::dlerror();
			let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
			if lib.is_null() {
				return None;
			}

			libc::dlerror();
			let intern = libc::dlsym(lib, c"xcb_intern_atom".as_ptr());
			libc::dlerror();
			let intern_reply = libc::dlsym(lib, c"xcb_intern_atom_reply".as_ptr());
			libc::dlerror();
			let get_prop = libc::dlsym(lib, c"xcb_get_property".as_ptr());
			libc::dlerror();
			let get_prop_reply = libc::dlsym(lib, c"xcb_get_property_reply".as_ptr());

			if intern.is_null() || intern_reply.is_null() || get_prop.is_null() || get_prop_reply.is_null() {
				let err_ptr = libc::dlerror();
				if !err_ptr.is_null() {
					let err = std::ffi::CStr::from_ptr(err_ptr);
					crate::log_error!("dlsym(xcb property fns) failed: {}", err.to_string_lossy());
				}
				return None;
			}

			Some((
				std::mem::transmute(intern),
				std::mem::transmute(intern_reply),
				std::mem::transmute(get_prop),
				std::mem::transmute(get_prop_reply),
			))
		})
	}
}

/// Intern an X11 atom by name.
unsafe fn xcb_intern_atom(connection: *mut libc::c_void, name: &str) -> Option<u32> {
	unsafe {
		let (intern, intern_reply, _, _) = load_xcb_property_fns()?;
		if connection.is_null() {
			return None;
		}

		let cookie = intern(
			connection,
			0,
			name.len() as u16,
			name.as_ptr() as *const std::ffi::c_char,
		);
		let reply = intern_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let atom = (*reply).atom;
		libc::free(reply as *mut libc::c_void);
		Some(atom)
	}
}

/// Read a `CARDINAL` (u32) property from a window by name.
///
/// Returns `None` when the property is absent or has an unexpected type, which
/// is the expected case for non-Wine windows.
pub(crate) unsafe fn xcb_get_window_property_u32(
	connection: *mut libc::c_void,
	window: u32,
	name: &str,
) -> Option<u32> {
	unsafe {
		let atom = xcb_intern_atom(connection, name)?;
		let (_, _, get_prop, get_prop_reply) = load_xcb_property_fns()?;

		let cookie = get_prop(connection, 0, window, atom, XCB_ATOM_CARDINAL, 0, 1);
		let reply = get_prop_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return None;
		}

		let value = if (*reply).type_ == XCB_ATOM_CARDINAL && (*reply).format == 32 && (*reply).value_len >= 1 {
			let data = (reply as *const u8).add(std::mem::size_of::<XcbGetPropertyReply>()) as *const u32;
			Some(*data)
		} else {
			None
		};

		libc::free(reply as *mut libc::c_void);
		value
	}
}

/// Returns `true` if the window has a property with the given (predefined) atom.
pub(crate) unsafe fn xcb_window_has_property(connection: *mut libc::c_void, window: u32, atom: u32) -> bool {
	unsafe {
		let (_, _, get_prop, get_prop_reply) = match load_xcb_property_fns() {
			Some(fns) => fns,
			None => return false,
		};
		if connection.is_null() {
			return false;
		}

		let cookie = get_prop(connection, 0, window, atom, XCB_GET_PROPERTY_TYPE_ANY, 0, 0);
		let reply = get_prop_reply(connection, cookie, std::ptr::null_mut());
		if reply.is_null() {
			return false;
		}

		// XCB_NONE (0) means the property does not exist.
		let has_property = (*reply).type_ != 0;
		libc::free(reply as *mut libc::c_void);
		has_property
	}
}

/// Separate event connection: never select masks or consume events on the
/// application's connection. Nonblocking dispatch performs no X11 round trips
/// unless a topology/property event invalidates the cached bypass decision.
pub(crate) struct BypassWatch {
	connection: *mut libc::c_void,
	fns: &'static EventFns,
	window: u32,
	watched: std::collections::HashSet<u32>,
	decision: Result<(), crate::swapchain::BypassReject>,
	allow_flip_atom: u32,
	initialized: bool,
}

// SAFETY: only accessed behind the surface's mutex; owns its XCB connection.
unsafe impl Send for BypassWatch {}

/// PropertyNotify wire prefix; atom is at byte 8 (byte 16 is its state).
#[repr(C)]
struct XcbPropertyNotifyEvent {
	response_type: u8,
	pad0: u8,
	sequence: u16,
	window: u32,
	atom: u32,
	time: u32,
	state: u8,
	pad1: [u8; 15],
}

type FnConnect = unsafe extern "C" fn(*const libc::c_char, *mut i32) -> *mut libc::c_void;
type FnDisconnect = unsafe extern "C" fn(*mut libc::c_void);
type FnPollEvent = unsafe extern "C" fn(*mut libc::c_void) -> *mut u8;
type FnChangeAttributes = unsafe extern "C" fn(*mut libc::c_void, u32, u32, *const u32) -> XcbCookie;
type FnRequestCheck = unsafe extern "C" fn(*mut libc::c_void, XcbCookie) -> *mut libc::c_void;
type FnConnectionError = unsafe extern "C" fn(*mut libc::c_void) -> i32;

struct EventFns {
	connect: FnConnect,
	disconnect: FnDisconnect,
	poll_event: FnPollEvent,
	change_attributes: FnChangeAttributes,
	request_check: FnRequestCheck,
	connection_error: FnConnectionError,
}

fn event_fns() -> Option<&'static EventFns> {
	use std::sync::OnceLock;
	static FNS: OnceLock<Option<EventFns>> = OnceLock::new();
	FNS.get_or_init(|| unsafe {
		let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
		if lib.is_null() {
			return None;
		}
		macro_rules! symbol {
			($name:expr, $ty:ty) => {{
				let ptr = libc::dlsym(lib, $name.as_ptr());
				if ptr.is_null() {
					return None;
				}
				std::mem::transmute::<*mut libc::c_void, $ty>(ptr)
			}};
		}
		Some(EventFns {
			connect: symbol!(c"xcb_connect", FnConnect),
			disconnect: symbol!(c"xcb_disconnect", FnDisconnect),
			poll_event: symbol!(c"xcb_poll_for_event", FnPollEvent),
			change_attributes: symbol!(c"xcb_change_window_attributes_checked", FnChangeAttributes),
			request_check: symbol!(c"xcb_request_check", FnRequestCheck),
			connection_error: symbol!(c"xcb_connection_has_error", FnConnectionError),
		})
	})
	.as_ref()
}

impl BypassWatch {
	pub(crate) unsafe fn new(app_connection: *mut libc::c_void, window: u32) -> Option<Self> {
		unsafe {
			let fns = event_fns()?;
			let connection = (fns.connect)(std::ptr::null(), std::ptr::null_mut());
			if connection.is_null() {
				return None;
			}
			let mut watch = Self {
				connection,
				fns,
				window,
				watched: Default::default(),
				decision: Err(crate::swapchain::BypassReject::Unavailable),
				allow_flip_atom: 0,
				initialized: false,
			};
			// DISPLAY must refer to the application's server. Validate the XID and
			// root before trusting this connection; otherwise retain plain XCB.
			let app_root = xcb_query_tree_window(app_connection, window).map(|t| t.0);
			let own_root = xcb_query_tree_window(connection, window).map(|t| t.0);
			if app_root.is_none() || own_root != app_root || (fns.connection_error)(connection) != 0 {
				return None;
			}
			watch.allow_flip_atom = xcb_intern_atom(connection, "_WINE_ALLOW_FLIP")?;
			watch.refresh();
			Some(watch)
		}
	}

	unsafe fn subscribe(&mut self, window: u32) -> Option<()> {
		unsafe {
			if self.watched.contains(&window) {
				return Some(());
			}
			// StructureNotify, SubstructureNotify, PropertyChange (nonexclusive).
			let mask = (1u32 << 17) | (1 << 19) | (1 << 22);
			let cookie = (self.fns.change_attributes)(self.connection, window, 1 << 11, &mask);
			let error = (self.fns.request_check)(self.connection, cookie);
			if !error.is_null() {
				libc::free(error);
				return None;
			}
			self.watched.insert(window);
			Some(())
		}
	}

	unsafe fn subscribe_tree(&mut self) -> Option<()> {
		unsafe {
			let mut current = self.window;
			let mut ancestors = std::collections::HashSet::new();
			loop {
				if !ancestors.insert(current) {
					return None;
				}
				self.subscribe(current)?;
				let (root, parent, _) = xcb_query_tree_window(self.connection, current)?;
				if parent == root || parent == 0 {
					break;
				}
				current = parent;
			}
			// Child properties/map/geometry changes can affect obscuring policy.
			let (query, reply_fn, _, _) = load_xcb_query_tree_fns()?;
			let reply = reply_fn(self.connection, query(self.connection, current), std::ptr::null_mut());
			if reply.is_null() {
				return None;
			}
			let children = std::slice::from_raw_parts(
				(reply as *const u8).add(32).cast::<u32>(),
				(*reply).children_len as usize,
			)
			.to_vec();
			libc::free(reply.cast());
			for child in children {
				self.subscribe(child)?;
			}
			Some(())
		}
	}

	unsafe fn refresh(&mut self) {
		unsafe {
			let previous = self.decision;
			self.decision = if self.subscribe_tree().is_some() {
				crate::swapchain::query_bypass_policy(self.connection, self.window)
			} else {
				Err(crate::swapchain::BypassReject::Unavailable)
			};
			if !self.initialized || self.decision != previous {
				self.initialized = true;
				match self.decision {
					Ok(()) => crate::log_debug!("XWayland bypass allowed: window={}", self.window),
					Err(reason) => crate::log_debug!(
						"XWayland bypass rejected: {} (window={})",
						reason.message(),
						self.window
					),
				}
			}
		}
	}

	pub(crate) fn allowed(&mut self) -> bool {
		unsafe {
			let mut changed = false;
			// Bound dispatch work under event storms; remaining events are handled
			// on the next dispatch. This reads events, not window-query replies.
			for _ in 0..256 {
				let event = (self.fns.poll_event)(self.connection);
				if event.is_null() {
					break;
				}
				let kind = *event & 0x7f;
				if kind == 17 {
					// DestroyNotify's window field: allow a reused XID to subscribe again.
					self.watched
						.remove(&std::ptr::read_unaligned(event.add(8).cast::<u32>()));
				}
				let relevant_property = kind == 28
					&& matches!(
						(*event.cast::<XcbPropertyNotifyEvent>()).atom,
						atom if atom == self.allow_flip_atom || atom == XCB_ATOM_WM_CLASS
					);
				changed |= kind == 0 || (16..=22).contains(&kind) || relevant_property;
				libc::free(event.cast());
			}
			if (self.fns.connection_error)(self.connection) != 0 {
				if self.decision != Err(crate::swapchain::BypassReject::Unavailable) {
					crate::log_debug!(
						"XWayland bypass rejected: X11 event connection lost (window={})",
						self.window
					);
				}
				self.decision = Err(crate::swapchain::BypassReject::Unavailable);
			} else if changed {
				self.refresh();
			}
		}
		self.decision.is_ok()
	}
}

impl Drop for BypassWatch {
	fn drop(&mut self) {
		// SAFETY: owns this connection; no other reader or pending worker exists.
		unsafe {
			(self.fns.disconnect)(self.connection);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn property_notify_layout_reads_the_atom_not_the_state() {
		assert_eq!(std::mem::size_of::<XcbPropertyNotifyEvent>(), 32);
		assert_eq!(std::mem::offset_of!(XcbPropertyNotifyEvent, atom), 8);
		assert_eq!(std::mem::offset_of!(XcbPropertyNotifyEvent, state), 16);
	}

	#[test]
	fn child_clipping_uses_parent_relative_coordinates() {
		assert_eq!(clipped_child_span(0, 1920, 1920), 1920);
		assert_eq!(clipped_child_span(1919, 10, 1920), 1);
		assert_eq!(clipped_child_span(-10, 20, 1920), 10);
		assert_eq!(clipped_child_span(-20, 10, 1920), 0);
		assert_eq!(clipped_child_span(1920, 10, 1920), 0);
	}

	#[test]
	fn unchanged_presentations_reuse_policy_without_x11_requests() {
		use std::sync::atomic::{AtomicUsize, Ordering};
		static REQUESTS: AtomicUsize = AtomicUsize::new(0);
		unsafe extern "C" fn connect(_: *const libc::c_char, _: *mut i32) -> *mut libc::c_void {
			std::ptr::null_mut()
		}
		unsafe extern "C" fn disconnect(_: *mut libc::c_void) {}
		unsafe extern "C" fn poll_event(_: *mut libc::c_void) -> *mut u8 {
			std::ptr::null_mut()
		}
		unsafe extern "C" fn attributes(_: *mut libc::c_void, _: u32, _: u32, _: *const u32) -> XcbCookie {
			REQUESTS.fetch_add(1, Ordering::Relaxed);
			XcbCookie { sequence: 0 }
		}
		unsafe extern "C" fn check(_: *mut libc::c_void, _: XcbCookie) -> *mut libc::c_void {
			std::ptr::null_mut()
		}
		unsafe extern "C" fn error(_: *mut libc::c_void) -> i32 {
			0
		}
		static FNS: EventFns = EventFns {
			connect,
			disconnect,
			poll_event,
			change_attributes: attributes,
			request_check: check,
			connection_error: error,
		};
		for decision in [Ok(()), Err(crate::swapchain::BypassReject::WineNoFlip)] {
			let mut watch = BypassWatch {
				connection: std::ptr::null_mut(),
				fns: &FNS,
				window: 1,
				watched: Default::default(),
				decision,
				allow_flip_atom: 100,
				initialized: true,
			};
			for _ in 0..100 {
				assert_eq!(watch.allowed(), decision.is_ok());
			}
		}
		assert_eq!(REQUESTS.load(Ordering::Relaxed), 0);
	}

	#[test]
	fn query_tree_reply_layout_matches_xcb() {
		assert_eq!(std::mem::size_of::<XcbQueryTreeReply>(), 32);
		assert_eq!(std::mem::offset_of!(XcbQueryTreeReply, children_len), 16);
	}

	#[test]
	fn get_property_reply_layout_matches_xcb() {
		assert_eq!(std::mem::size_of::<XcbGetPropertyReply>(), 32);
		assert_eq!(std::mem::offset_of!(XcbGetPropertyReply, type_), 8);
		assert_eq!(std::mem::offset_of!(XcbGetPropertyReply, value_len), 16);
	}

	#[test]
	fn get_window_attributes_reply_layout_matches_xcb() {
		assert_eq!(std::mem::size_of::<XcbGetWindowAttributesReply>(), 44);
		assert_eq!(std::mem::offset_of!(XcbGetWindowAttributesReply, map_state), 26);
		assert_eq!(std::mem::offset_of!(XcbGetWindowAttributesReply, override_redirect), 27);
	}
}
