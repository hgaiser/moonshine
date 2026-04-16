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
