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
	children_len: u32,
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

/// Query the X11 window tree for a given window.
pub(crate) unsafe fn xcb_query_tree_window(connection: *mut libc::c_void, window: u32) -> Option<(u32, u32, u32)> {
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
	let children_len = (*reply).children_len;
	libc::free(reply as *mut libc::c_void);

	Some((root, parent, children_len))
}

/// Walk up the X11 parent chain via `xcb_query_tree` until we reach the root.
pub(crate) unsafe fn xcb_get_toplevel_window(connection: *mut libc::c_void, window: u32) -> Option<u32> {
	let mut current = window;
	loop {
		let (root, parent, _children_len) = xcb_query_tree_window(connection, current)?;
		if parent == root || parent == 0 {
			return Some(current);
		}
		current = parent;
	}
}

/// Get the `map_state` and `override_redirect` flags for an X11 window.
pub(crate) unsafe fn xcb_get_window_attributes(connection: *mut libc::c_void, window: u32) -> Option<(u8, bool)> {
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

/// Check if any child window of `window` is VIEWABLE and larger than 1×1.
pub(crate) unsafe fn xcb_get_largest_obscuring_child(
	connection: *mut libc::c_void,
	window: u32,
) -> Option<Option<(u32, u32)>> {
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

	let (query_tree, query_tree_reply) = match fns_opt {
		Some(fns) => *fns,
		None => return None,
	};

	if connection.is_null() {
		return None;
	}

	let cookie = query_tree(connection, window);
	let reply = query_tree_reply(connection, cookie, std::ptr::null_mut());
	if reply.is_null() {
		return None;
	}

	let children_len = (*reply).children_len;
	let parent_rect = xcb_get_window_rect(connection, window);
	if parent_rect.is_none() {
		libc::free(reply as *mut libc::c_void);
		return None;
	}
	let (px, py, pw, ph) = parent_rect.unwrap();

	let mut max_w: u32 = 0;
	let mut max_h: u32 = 0;

	if children_len > 0 {
		let children = (reply as *const u32).add(std::mem::size_of::<XcbQueryTreeReply>() / 4);
		for i in 0..children_len as isize {
			let child = *children.add(i as usize);
			if let Some((map_state, override_redirect)) = xcb_get_window_attributes(connection, child) {
				if map_state == XCB_MAP_STATE_VIEWABLE && !override_redirect {
					if let Some((cx, cy, cw, ch)) = xcb_get_window_rect(connection, child) {
						let rel_x = cx as i32 - px as i32;
						let rel_y = cy as i32 - py as i32;
						let clipped_w = (pw as i32 - rel_x).max(0) as u32;
						let clipped_h = (ph as i32 - rel_y).max(0) as u32;
						let final_w = cw.min(clipped_w);
						let final_h = ch.min(clipped_h);
						if final_w > max_w {
							max_w = final_w;
						}
						if final_h > max_h {
							max_h = final_h;
						}
					}
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn get_window_attributes_reply_layout_matches_xcb() {
		assert_eq!(std::mem::size_of::<XcbGetWindowAttributesReply>(), 44);
		assert_eq!(std::mem::offset_of!(XcbGetWindowAttributesReply, map_state), 26);
		assert_eq!(std::mem::offset_of!(XcbGetWindowAttributesReply, override_redirect), 27);
	}
}
