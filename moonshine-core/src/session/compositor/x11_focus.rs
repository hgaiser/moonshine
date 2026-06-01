//! X11 property reading for Steam focus control.
//!
//! Reads `GAMESCOPECTRL_BASELAYER_WINDOW` and
//! `GAMESCOPECTRL_BASELAYER_APPID` from the XWayland root window.
//! Also reads per-window properties like `_NET_WM_PID` (for app_id detection),
//! `STEAM_INPUT_FOCUS` (for separating keyboard/pointer focus),
//! `STEAM_OVERLAY` (for overlay window classification),
//! `STEAM_STREAMING_CLIENT` (for skipping streaming client windows),
//! `STEAM_STREAMING_CLIENT_VIDEO` (for skipping streaming video windows),
//! and `STEAM_GAMESCOPE_VROVERLAY_TARGET` (for skipping VR overlay targets).
//!
//! Uses dlsym to load X11 at runtime — no link-time dependency.

use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::OnceLock;

use libc::{c_long as libc_c_long, c_ulong as libc_c_ulong, dlopen, dlsym, pid_t, RTLD_LAZY};

// ---------------------------------------------------------------------------
// X11 FFI types
// ---------------------------------------------------------------------------

/// Opaque X11 Display pointer (same as `Display*` from Xlib).
type XDisplay = c_void;

/// X11 Atom type.
/// On LP64 (64-bit Linux), Xlib defines `Atom` as `unsigned long` (8 bytes).
/// Using u32 here would silently truncate arguments and corrupt the argument
/// layout in every XGetWindowProperty / XChangeProperty call.
type Atom = libc_c_ulong;

/// X11 Window/XID type.
/// On LP64 (64-bit Linux), Xlib defines `Window` as `unsigned long` (8 bytes).
/// Using u32 here would silently truncate arguments and corrupt the argument
/// layout in every XGetWindowProperty / XSetInputFocus call.
type Window = libc_c_ulong;

/// Steam app ID — distinct from X11 window IDs to prevent type confusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AppId(pub u32);

// Predefined X11 atom constants (from X11/Xatom.h).
// These are built-in atoms with fixed numeric IDs — they do not need interning.
const XA_CARDINAL: Atom = 6;

// ---------------------------------------------------------------------------
// X11 function pointer types
// ---------------------------------------------------------------------------

type FnXOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut XDisplay;
type FnXCloseDisplay = unsafe extern "C" fn(*mut XDisplay) -> c_int;
type FnXInternAtom = unsafe extern "C" fn(*mut XDisplay, *const c_char, c_int) -> Atom;
type FnXErrorHandler = unsafe extern "C" fn(*mut XDisplay, *mut c_void) -> c_int;
type FnXSetErrorHandler = unsafe extern "C" fn(Option<FnXErrorHandler>) -> Option<FnXErrorHandler>;
type FnXGetWindowProperty = unsafe extern "C" fn(
	*mut XDisplay,
	Window,
	Atom,
	libc_c_long,
	libc_c_ulong,
	c_int,
	Atom,
	*mut Atom,
	*mut c_int,
	*mut libc_c_ulong,
	*mut libc_c_ulong,
	*mut *mut u8,
) -> c_int;
type FnXFree = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnXSetInputFocus = unsafe extern "C" fn(*mut XDisplay, Window, c_int, libc_c_ulong) -> c_int;
type FnXFlush = unsafe extern "C" fn(*mut XDisplay) -> c_int;
type FnXDefaultRootWindow = unsafe extern "C" fn(*mut XDisplay) -> Window;
type FnXChangeProperty = unsafe extern "C" fn(
	*mut XDisplay,
	Window,
	Atom,  // property
	Atom,  // type
	c_int, // format
	c_int, // mode (PropModeReplace = 1, PropModeAppend = 2)
	*const c_void,
	c_int,
) -> c_int;
type FnXDeleteProperty = unsafe extern "C" fn(*mut XDisplay, Window, Atom) -> c_int;

// ---------------------------------------------------------------------------
// XRes FFI types (libXRes.so.1)
// ---------------------------------------------------------------------------

/// XResClientIdSpec — query spec for XResQueryClientIds.
/// Matches C struct: { XID client; unsigned int mask; } (16 bytes with padding).
#[repr(C)]
struct XResClientIdSpec {
	client: libc_c_ulong,
	mask: u32,
	_padding: u32,
}

/// XResClientIdValue — result from XResQueryClientIds.
/// Matches C struct: { XResClientIdSpec spec; long length; void *value; } (32 bytes).
#[repr(C)]
struct XResClientIdValue {
	spec: XResClientIdSpec,
	length: libc_c_long,
	value: *mut c_void,
}

/// XRes client ID mask constants.
const XRES_CLIENT_ID_PID_MASK: u32 = 0x2; // 1 << XRES_CLIENT_ID_PID

/// XLib Success status.
const XLIB_SUCCESS: c_int = 0;

type FnXResQueryClientIds = unsafe extern "C" fn(
	*mut XDisplay,
	libc_c_long,                 // num_specs
	*const XResClientIdSpec,     // client_specs
	*mut libc_c_long,            // num_ids (out)
	*mut *mut XResClientIdValue, // client_ids (out)
) -> c_int;

type FnXResGetClientPid = unsafe extern "C" fn(*const XResClientIdValue) -> pid_t;

type FnXResClientIdsDestroy = unsafe extern "C" fn(
	libc_c_long,            // num_ids
	*mut XResClientIdValue, // client_ids
);

// ---------------------------------------------------------------------------
// Lazy-loaded function pointers
// ---------------------------------------------------------------------------

/// Opaque library handle — stored as isize so it can be Send+Sync.
/// Raw pointers are neither, but isize is just an integer.
struct LoadedXlib {
	/// Library handle from `dlopen(libX11.so.6)`. Stored but not used for
	/// `dlclose` — the compositor runs for the session lifetime.
	#[allow(dead_code)]
	lib: isize,
	xopendisplay: Option<FnXOpenDisplay>,
	xclosedisplay: Option<FnXCloseDisplay>,
	xinternatom: Option<FnXInternAtom>,
	xgetwindowproperty: Option<FnXGetWindowProperty>,
	xseterrorhandler: Option<FnXSetErrorHandler>,
	xfree: Option<FnXFree>,
	xsetinputfocus: Option<FnXSetInputFocus>,
	xflush: Option<FnXFlush>,
	xdefaultrootwindow: Option<FnXDefaultRootWindow>,
	xchangeproperty: Option<FnXChangeProperty>,
	xdeleteproperty: Option<FnXDeleteProperty>,
}

static LOADED_XLIB: OnceLock<LoadedXlib> = OnceLock::new();

struct LoadedXRes {
	/// Library handle from `dlopen(libXRes.so.1)`. Stored but not used for
	/// `dlclose` — the compositor runs for the session lifetime and the
	/// library should remain loaded.
	#[allow(dead_code)]
	lib: isize,
	xresqueryclientids: Option<FnXResQueryClientIds>,
	xresgetclientpid: Option<FnXResGetClientPid>,
	xresclientidsdestroy: Option<FnXResClientIdsDestroy>,
}

static LOADED_XRES: OnceLock<LoadedXRes> = OnceLock::new();

/// Convert a display number to a gamescope XWayland server ID.
///
/// Gamescope encodes the display number as `(display_number + 100) << 8 | display_number`.
/// This is used by the gamescope-swapchain protocol for WSI clients to discover
/// the XWayland server.
fn display_id_to_server_id(display_number: u32) -> u32 {
	((display_number + 100) << 8) | display_number
}

/// Convert a `dlsym` result (`*mut c_void`) to an `Option<FnType>`.
///
/// # Safety
/// `dlsym` returns `*mut c_void` for all symbol types including functions.
/// On all mainstream platforms (x86_64, aarch64), data pointers and function
/// pointers have identical size (64 bits) and alignment, making this transmute
/// safe. This is the standard pattern used by `libloading` internally.
macro_rules! sym {
	($ptr:expr, $ty:ty) => {{
		#[allow(unused_unsafe)]
		unsafe {
			if ($ptr).is_null() {
				None
			} else {
				Some(std::mem::transmute::<*mut c_void, $ty>($ptr))
			}
		}
	}};
}

fn load_xres() {
	LOADED_XRES.get_or_init(|| unsafe {
		libc::dlerror();
		let lib_ptr = dlopen(c"libXRes.so.1".as_ptr(), RTLD_LAZY);
		if lib_ptr.is_null() {
			let err = libc::dlerror();
			if !err.is_null() {
				tracing::debug!(
					target: "focus",
					"dlopen(libXRes.so.1) failed: {}",
					std::ffi::CStr::from_ptr(err).to_string_lossy()
				);
			}
			return LoadedXRes {
				lib: 0,
				xresqueryclientids: None,
				xresgetclientpid: None,
				xresclientidsdestroy: None,
			};
		}

		libc::dlerror();
		let query_ptr = dlsym(lib_ptr, c"XResQueryClientIds".as_ptr());
		libc::dlerror();
		let pid_ptr = dlsym(lib_ptr, c"XResGetClientPid".as_ptr());
		libc::dlerror();
		let destroy_ptr = dlsym(lib_ptr, c"XResClientIdsDestroy".as_ptr());

		LoadedXRes {
			lib: lib_ptr as isize,
			xresqueryclientids: sym!(query_ptr, FnXResQueryClientIds),
			xresgetclientpid: sym!(pid_ptr, FnXResGetClientPid),
			xresclientidsdestroy: sym!(destroy_ptr, FnXResClientIdsDestroy),
		}
	});
}

fn with_xres<F, R>(f: F) -> Option<R>
where
	F: FnOnce(&LoadedXRes) -> Option<R>,
{
	LOADED_XRES.get().and_then(f)
}

fn with_xlib<F, R>(f: F) -> Option<R>
where
	F: FnOnce(&LoadedXlib) -> Option<R>,
{
	LOADED_XLIB.get().and_then(f)
}

/// Silent X11 error handler — suppresses all X11 errors silently.
/// Returns 0 to prevent the default error handler from printing.
extern "C" fn silent_x11_error(_dpy: *mut XDisplay, _err: *mut c_void) -> c_int {
	0
}

fn load_xlib() {
	LOADED_XLIB.get_or_init(|| unsafe {
		libc::dlerror();
		let lib_ptr = dlopen(c"libX11.so.6".as_ptr(), RTLD_LAZY);
		if lib_ptr.is_null() {
			let err = libc::dlerror();
			if !err.is_null() {
				tracing::warn!(
					target: "focus",
					"dlopen(libX11.so.6) failed: {}",
					std::ffi::CStr::from_ptr(err).to_string_lossy()
				);
			}
			return LoadedXlib {
				lib: 0,
				xopendisplay: None,
				xclosedisplay: None,
				xinternatom: None,
				xgetwindowproperty: None,
				xseterrorhandler: None,
				xfree: None,
				xsetinputfocus: None,
				xflush: None,
				xdefaultrootwindow: None,
				xchangeproperty: None,
				xdeleteproperty: None,
			};
		}

		libc::dlerror();
		let xopendisplay_ptr = dlsym(lib_ptr, c"XOpenDisplay".as_ptr());
		libc::dlerror();
		let xclosedisplay_ptr = dlsym(lib_ptr, c"XCloseDisplay".as_ptr());
		libc::dlerror();
		let xinternatom_ptr = dlsym(lib_ptr, c"XInternAtom".as_ptr());
		libc::dlerror();
		let xgetwindowproperty_ptr = dlsym(lib_ptr, c"XGetWindowProperty".as_ptr());
		libc::dlerror();
		let xseterrorhandler_ptr = dlsym(lib_ptr, c"XSetErrorHandler".as_ptr());
		libc::dlerror();
		let xfree_ptr = dlsym(lib_ptr, c"XFree".as_ptr());
		libc::dlerror();
		let xsetinputfocus_ptr = dlsym(lib_ptr, c"XSetInputFocus".as_ptr());
		libc::dlerror();
		let xflush_ptr = dlsym(lib_ptr, c"XFlush".as_ptr());
		libc::dlerror();
		let xdefaultrootwindow_ptr = dlsym(lib_ptr, c"XDefaultRootWindow".as_ptr());
		libc::dlerror();
		let xchangeproperty_ptr = dlsym(lib_ptr, c"XChangeProperty".as_ptr());
		libc::dlerror();
		let xdeleteproperty_ptr = dlsym(lib_ptr, c"XDeleteProperty".as_ptr());

		LoadedXlib {
			lib: lib_ptr as isize,
			xopendisplay: sym!(xopendisplay_ptr, FnXOpenDisplay),
			xclosedisplay: sym!(xclosedisplay_ptr, FnXCloseDisplay),
			xinternatom: sym!(xinternatom_ptr, FnXInternAtom),
			xgetwindowproperty: sym!(xgetwindowproperty_ptr, FnXGetWindowProperty),
			xseterrorhandler: sym!(xseterrorhandler_ptr, FnXSetErrorHandler),
			xfree: sym!(xfree_ptr, FnXFree),
			xsetinputfocus: sym!(xsetinputfocus_ptr, FnXSetInputFocus),
			xflush: sym!(xflush_ptr, FnXFlush),
			xdefaultrootwindow: sym!(xdefaultrootwindow_ptr, FnXDefaultRootWindow),
			xchangeproperty: sym!(xchangeproperty_ptr, FnXChangeProperty),
			xdeleteproperty: sym!(xdeleteproperty_ptr, FnXDeleteProperty),
		}
	});
}

// ---------------------------------------------------------------------------
// Focus control data
// ---------------------------------------------------------------------------

/// Focus control information read from X11 root window properties.
///
/// Set by Steam via `GAMESCOPECTRL_BASELAYER_WINDOW` and
/// `GAMESCOPECTRL_BASELAYER_APPID` root properties.
#[derive(Debug, Clone, Default)]
pub(crate) struct FocusControl {
	/// X11 window ID that Steam wants focused.
	pub window: Option<u32>,
	/// App IDs that Steam wants focused.
	pub app_ids: Vec<AppId>,
}

/// X11 focus control manager.
///
/// Opens a connection to the XWayland display and reads root window
/// properties to determine Steam's focus control preferences. Also
/// provides per-window property access for app_id detection and
/// input focus mode detection.
pub(crate) struct X11Focus {
	dpy: *mut XDisplay,
	/// Root window of the default screen — obtained once via XDefaultRootWindow.
	root: Window,
	/// Pre-interned atom IDs — cached once at construction time to avoid
	/// repeated `XInternAtom` calls on every property read.
	atoms: CachedAtoms,
}

/// Cached X11 atom IDs — interned once at construction time.
struct CachedAtoms {
	net_wm_pid: Atom,
	net_wm_state: Atom,
	net_wm_state_skip_taskbar: Atom,
	net_wm_state_skip_pager: Atom,
	net_wm_window_opacity: Atom,
	steam_input_focus: Atom,
	steam_overlay: Atom,
	steam_streaming_client: Atom,
	steam_streaming_client_video: Atom,
	steam_legacy_big_picture: Atom,
	steam_gamescope_vroverlay_target: Atom,
	gamescope_external_overlay: Atom,
	gamescopectrl_baselayer_window: Atom,
	gamescopectrl_baselayer_appid: Atom,
	wine_hwnd_style: Atom,
	gamescope_focused_app: Atom,
	gamescope_focusable_apps: Atom,
	gamescope_focusable_windows: Atom,
	gamescope_xwayland_server_id: Atom,
}

impl CachedAtoms {
	fn intern_all(dpy: *mut XDisplay) -> Option<Self> {
		with_xlib(|loaded| {
			let intern = loaded.xinternatom?;
			unsafe {
				let intern_one = |name: &[u8]| -> Option<Atom> {
					let cstr = CString::new(name).ok()?;
					Some(intern(dpy, cstr.as_ptr(), 0))
				};
				Some(Self {
					net_wm_pid: intern_one(b"_NET_WM_PID")?,
					net_wm_state: intern_one(b"_NET_WM_STATE")?,
					net_wm_state_skip_taskbar: intern_one(b"_NET_WM_STATE_SKIP_TASKBAR")?,
					net_wm_state_skip_pager: intern_one(b"_NET_WM_STATE_SKIP_PAGER")?,
					net_wm_window_opacity: intern_one(b"_NET_WM_WINDOW_OPACITY")?,
					steam_input_focus: intern_one(b"STEAM_INPUT_FOCUS")?,
					steam_overlay: intern_one(b"STEAM_OVERLAY")?,
					steam_streaming_client: intern_one(b"STEAM_STREAMING_CLIENT")?,
					steam_streaming_client_video: intern_one(b"STEAM_STREAMING_CLIENT_VIDEO")?,
					steam_legacy_big_picture: intern_one(b"STEAM_LEGACY_BIG_PICTURE")?,
					steam_gamescope_vroverlay_target: intern_one(b"STEAM_GAMESCOPE_VROVERLAY_TARGET")?,
					gamescope_external_overlay: intern_one(b"GAMESCOPE_EXTERNAL_OVERLAY")?,
					gamescopectrl_baselayer_window: intern_one(b"GAMESCOPECTRL_BASELAYER_WINDOW")?,
					gamescopectrl_baselayer_appid: intern_one(b"GAMESCOPECTRL_BASELAYER_APPID")?,
					wine_hwnd_style: intern_one(b"WINE_HWND_STYLE")?,
					gamescope_focused_app: intern_one(b"GAMESCOPE_FOCUSED_APP")?,
					gamescope_focusable_apps: intern_one(b"GAMESCOPE_FOCUSABLE_APPS")?,
					gamescope_focusable_windows: intern_one(b"GAMESCOPE_FOCUSABLE_WINDOWS")?,
					gamescope_xwayland_server_id: intern_one(b"GAMESCOPE_XWAYLAND_SERVER_ID")?,
				})
			}
		})
	}
}

impl X11Focus {
	/// Open an X11 connection to the XWayland display `:N`.
	pub fn open(display_number: u32) -> Option<Self> {
		load_xlib();
		let open_fn = with_xlib(|loaded| loaded.xopendisplay)?;
		let display_name = CString::new(format!(":{}", display_number)).ok()?;
		let dpy = unsafe { open_fn(display_name.as_ptr()) };
		if dpy.is_null() {
			tracing::warn!(target: "focus", "XOpenDisplay(:{}) failed", display_number);
			return None;
		}
		let atoms = CachedAtoms::intern_all(dpy)?;

		// Get the root window — needed for root property reads.
		let root = with_xlib(|loaded| loaded.xdefaultrootwindow.map(|f| unsafe { f(dpy) })).unwrap_or(0);
		if root == 0 {
			tracing::warn!(target: "focus", "XDefaultRootWindow returned 0");
			return None;
		}

		// Initialize GAMESCOPE_XWAYLAND_SERVER_ID on the root window so that
		// compatible WSI clients can discover this compositor's XWayland server
		// and succeed the override_window_content handshake.
		if atoms.gamescope_xwayland_server_id != 0 {
			let server_id = display_id_to_server_id(display_number);
			with_xlib(|loaded| {
				let seterr = loaded.xseterrorhandler?;
				let change = loaded.xchangeproperty?;
				unsafe {
					let prev = seterr(Some(silent_x11_error));
					// XChangeProperty with format=32 expects native C `long`
					// elements.  On LP64 (64-bit) `long` is 8 bytes but `u32`
					// is 4 bytes, so we must convert to native `long` to avoid
					// reading garbage past the value.
					let server_id_long = server_id as libc_c_long;
					change(
						dpy,
						root,
						atoms.gamescope_xwayland_server_id,
						XA_CARDINAL,
						32,
						1,
						&server_id_long as *const libc_c_long as *const c_void,
						1,
					);
					seterr(prev);
				}
				Some(())
			});
		}

		tracing::debug!(target: "focus", "Opened X11 connection to :{}", display_number);
		Some(Self { dpy, root, atoms })
	}

	/// Read a single CARDINAL (format-32) window property by pre-interned atom.
	///
	/// Centralizes the ~15-line pattern of: get property → validate format →
	/// read bytes → free → restore error handler. Uses cached atom IDs to
	/// avoid repeated `XInternAtom` calls.
	/// Returns the u32 value if the property is set, or `default_val` otherwise.
	fn read_cardinal_prop_by_atom(&self, window_id: Window, atom: Atom, default_val: u32) -> u32 {
		if self.dpy.is_null() {
			return default_val;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let get_prop = loaded.xgetwindowproperty?;
			let free = loaded.xfree?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				let mut actual_type: Atom = 0;
				let mut actual_format: c_int = 0;
				let mut nitems: libc_c_ulong = 0;
				let mut bytes_after: libc_c_ulong = 0;
				let mut prop: *mut u8 = std::ptr::null_mut();
				let result = get_prop(
					self.dpy,
					window_id,
					atom,
					0,
					1,
					0,
					XA_CARDINAL,
					&mut actual_type,
					&mut actual_format,
					&mut nitems,
					&mut bytes_after,
					&mut prop,
				);
				// format-32 data is an array of native-endian C `long` (8 bytes
				// on LP64). Cast to *const libc_c_long and read the low 32 bits.
				let value = if result == 0 && !prop.is_null() && nitems > 0 && actual_format == 32 {
					*(prop as *const libc_c_long) as u32
				} else {
					default_val
				};
				if !prop.is_null() {
					free(prop as *mut c_void);
				}
				seterr(prev);
				Some(value)
			}
		})
		.unwrap_or(default_val)
	}

	/// Read a variable-length CARDINAL (format-32) array from a window property.
	///
	/// Returns a Vec of u32 values. Empty vec if the property is not set.
	fn read_cardinal_array_prop(&self, window_id: Window, atom: Atom, max_items: libc_c_ulong) -> Vec<u32> {
		if self.dpy.is_null() {
			return Vec::new();
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let get_prop = loaded.xgetwindowproperty?;
			let free = loaded.xfree?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				let mut actual_type: Atom = 0;
				let mut actual_format: c_int = 0;
				let mut nitems: libc_c_ulong = 0;
				let mut bytes_after: libc_c_ulong = 0;
				let mut prop: *mut u8 = std::ptr::null_mut();
				let result = get_prop(
					self.dpy,
					window_id,
					atom,
					0,
					max_items,
					0,
					0 as Atom, // Any type
					&mut actual_type,
					&mut actual_format,
					&mut nitems,
					&mut bytes_after,
					&mut prop,
				);
				let values = if result == 0 && !prop.is_null() && nitems > 0 && actual_format == 32 {
					let count = (nitems as usize).min(max_items as usize);
					let longs = std::slice::from_raw_parts(prop as *const libc_c_long, count);
					longs.iter().map(|&v| v as u32).collect()
				} else {
					Vec::new()
				};
				if !prop.is_null() {
					free(prop as *mut c_void);
				}
				seterr(prev);
				Some(values)
			}
		})
		.unwrap_or_default()
	}

	/// Read all focus control data at once.
	///
	/// Directly call `XSetInputFocus` on the given X11 window.
	///
	/// Equivalent to gamescope's `sync_x11_focus()` — sets input focus
	/// unconditionally, bypassing the WM_TAKE_FOCUS protocol. Necessary for
	/// GloballyActive windows (WmHints { input: false }, e.g. HFW under
	/// Proton) where Smithay only sends WM_TAKE_FOCUS but never calls
	/// XSetInputFocus, so the game never receives FocusIn → WM_ACTIVATE.
	pub fn set_input_focus(&self, window_id: u32) {
		if self.dpy.is_null() {
			return;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let set_focus = loaded.xsetinputfocus?;
			let flush = loaded.xflush?;
			unsafe {
				// Install a silent error handler before XSetInputFocus.
				// A destroyed window will cause BadWindow/BadMatch; without
				// a handler that terminates the process via the default handler.
				let prev = seterr(Some(silent_x11_error));
				// RevertToPointerRoot = 1; CurrentTime = 0
				set_focus(self.dpy, window_id as Window, 1, 0);
				flush(self.dpy);
				seterr(prev);
			}
			tracing::debug!(target: "focus", window_id, "set_input_focus: XSetInputFocus called");
			Some(())
		});
	}

	/// Read all focus control data at once.
	///
	/// Returns `None` if the X11 display connection is broken or invalid.
	/// Uses a silent error handler to suppress BadWindow errors when
	/// the X11 connection is dead (Xwayland crashed).
	pub fn read_focus_control(&self) -> Option<FocusControl> {
		if self.dpy.is_null() {
			return None;
		}
		// Verify X11 library is loaded.
		LOADED_XLIB.get()?;
		let window_val = self.read_cardinal_prop_by_atom(self.root, self.atoms.gamescopectrl_baselayer_window, 0);
		let window = if window_val != 0 { Some(window_val) } else { None };
		let app_ids = self.read_cardinal_array_prop(self.root, self.atoms.gamescopectrl_baselayer_appid, 1024);
		let app_ids: Vec<AppId> = app_ids.into_iter().map(AppId).collect();
		Some(FocusControl { window, app_ids })
	}

	/// Read the PID for the client owning an X11 window.
	/// Returns the PID if available, 0 otherwise.
	///
	/// Primary: read `_NET_WM_PID` window property (ECD spec).
	/// Fallback: use `XResQueryClientIds` to query X server directly
	/// for the PID of the client that owns this window resource.
	/// Gamescope: `get_win_pid()` — uses XResQueryClientIds only.
	pub fn get_window_pid(&self, window_id: u32) -> u32 {
		// Try _NET_WM_PID first — uses cached atom.
		let pid = self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.net_wm_pid, 0);

		if pid != 0 {
			return pid;
		}

		// Fallback: XResQueryClientIds — query X server for client PID.
		// Gamescope uses this as its primary method.
		// Works even when _NET_WM_PID is not set (e.g., Proton/Wine windows).
		self.get_window_pid_xres(window_id as Window)
	}

	/// XRes fallback: query X server for the PID of the client owning a window.
	///
	/// Uses XResQueryClientIds with XRES_CLIENT_ID_PID_MASK to ask the X server
	/// directly which client (by PID) owns the given window resource.
	/// This bypasses the need for the client to set _NET_WM_PID.
	fn get_window_pid_xres(&self, window_id: Window) -> u32 {
		if self.dpy.is_null() {
			return 0;
		}
		load_xres();
		with_xres(|loaded| {
			let query = loaded.xresqueryclientids?;
			let get_pid = loaded.xresgetclientpid?;
			let destroy = loaded.xresclientidsdestroy?;
			unsafe {
				let spec = XResClientIdSpec {
					client: window_id as libc_c_ulong,
					mask: XRES_CLIENT_ID_PID_MASK,
					_padding: 0,
				};
				let mut num_ids: libc_c_long = 0;
				let mut client_ids: *mut XResClientIdValue = std::ptr::null_mut();

				let status = query(self.dpy, 1, &spec, &mut num_ids, &mut client_ids);

				let pid = if status == XLIB_SUCCESS && num_ids > 0 && !client_ids.is_null() {
					let pid = get_pid(&*client_ids) as u32;
					destroy(num_ids, client_ids);
					pid
				} else {
					if !client_ids.is_null() {
						destroy(num_ids, client_ids);
					}
					0
				};

				Some(pid)
			}
		})
		.unwrap_or(0)
	}

	/// Read the STEAM_INPUT_FOCUS window property.
	/// Returns 0 if not set.
	///
	/// Gamescope: reads `steamInputFocusAtom` property.
	/// Mode 0 = normal (keyboard and pointer focus on same window).
	/// Mode 2 = separate keyboard/pointer focus — keyboard stays
	/// on the main window while pointer/input routes to the overlay
	/// (used by Steam overlay when active).
	pub fn get_input_focus_mode(&self, window_id: u32) -> u32 {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.steam_input_focus, 0)
	}

	/// Read the raw STEAM_OVERLAY window property value.
	/// Returns the value (0 = not a Steam window, non-zero = Steam window).
	pub fn get_steam_overlay_value(&self, window_id: u32) -> u32 {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.steam_overlay, 0)
	}

	/// Read the _NET_WM_WINDOW_OPACITY property from an X11 window.
	/// Returns a value 0-255 where 255 is fully opaque.
	///
	/// _NET_WM_WINDOW_OPACITY is a 32-bit value (0 = transparent,
	/// 0xFFFFFFFF = opaque). We scale it to 0-255.
	pub fn get_window_opacity(&self, window_id: u32) -> u32 {
		// Read the raw 32-bit value via the shared helper, then scale.
		let raw = self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.net_wm_window_opacity, 0xFFFFFFFF);
		if raw == 0 {
			0
		} else {
			// Use u64 to avoid overflow: raw * 255 can exceed u32::MAX when
			// raw is close to 0xFFFFFFFF (e.g. 0xFFFFFFFF * 255 = 0xFEFFFFFF01).
			((raw as u64 * 255) / 0xFFFFFFFF) as u32
		}
	}

	/// Read the STEAM_STREAMING_CLIENT window property.
	/// Returns true if the window is a Steam Streaming Client window.
	///
	/// Gamescope: reads `steamStreamingClientAtom` property.
	/// Streaming client windows are skipped from focus candidates.
	pub fn is_steam_streaming_client(&self, window_id: u32) -> bool {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.steam_streaming_client, 0) != 0
	}

	/// Read the STEAM_STREAMING_CLIENT_VIDEO window property.
	/// Returns true if the window is a Steam Streaming Client Video window.
	///
	/// Gamescope: reads `steamStreamingClientVideoAtom` property.
	/// Streaming client video windows are skipped from focus candidates.
	pub fn is_steam_streaming_client_video(&self, window_id: u32) -> bool {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.steam_streaming_client_video, 0) != 0
	}

	/// Read the GAMESCOPE_EXTERNAL_OVERLAY window property.
	/// Returns true if the window is an external overlay (e.g., Discord, mangoapp, OBS).
	///
	/// Gamescope: reads `externalOverlayAtom` property (GAMESCOPE_EXTERNAL_OVERLAY).
	/// External applications set this property to tell gamescope they are overlays.
	/// When set, gamescope also forces appID = 0 so the window is deprioritized.
	pub fn is_gamescope_external_overlay(&self, window_id: u32) -> bool {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.gamescope_external_overlay, 0) != 0
	}

	/// Read the STEAM_GAMESCOPE_VROVERLAY_TARGET window property.
	/// Returns the VR overlay handle (u64) if set, 0 otherwise.
	///
	/// Gamescope: reads `steamGamescopeVROverlayTarget` property.
	/// Windows with a non-zero VR overlay target are skipped from focus candidates.
	pub fn get_vr_overlay_target(&self, window_id: u32) -> u64 {
		if self.dpy.is_null() {
			return 0;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let get_prop = loaded.xgetwindowproperty?;
			let free = loaded.xfree?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				let atom = self.atoms.steam_gamescope_vroverlay_target;
				let mut actual_type: Atom = 0;
				let mut actual_format: c_int = 0;
				let mut nitems: libc_c_ulong = 0;
				let mut bytes_after: libc_c_ulong = 0;
				let mut prop: *mut u8 = std::ptr::null_mut();
				// Read 8 bytes for a 64-bit value (format 32, 2 items)
				let result = get_prop(
					self.dpy,
					window_id as Window,
					atom,
					0,
					2,
					0,
					XA_CARDINAL,
					&mut actual_type,
					&mut actual_format,
					&mut nitems,
					&mut bytes_after,
					&mut prop,
				);
				// format-32 VR overlay target is stored as two consecutive
				// native-endian C `long` values forming a 64-bit handle.
				let vr_target = if result == 0 && !prop.is_null() && nitems >= 2 && actual_format == 32 {
					let longs = std::slice::from_raw_parts(prop as *const libc_c_long, 2);
					(longs[1] as u64) << 32 | (longs[0] as u64)
				} else {
					0
				};
				if !prop.is_null() {
					free(prop as *mut c_void);
				}
				seterr(prev);
				Some(vr_target)
			}
		})
		.unwrap_or(0)
	}

	/// Read the STEAM_LEGACY_BIG_PICTURE window property.
	/// Returns true if the window is a Steam Big Picture Mode window.
	///
	/// Gamescope: reads `steamLegacyBigPictureAtom` property.
	/// Steam Big Picture windows get app_id = 769.
	pub fn is_steam_big_picture(&self, window_id: u32) -> bool {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.steam_legacy_big_picture, 0) != 0
	}

	/// Read the WINE_HWND_STYLE window property.
	/// Returns the window style bits if set, 0 otherwise.
	///
	/// Gamescope: reads `wineHwndStyleAtom` property.
	/// WS_DISABLED = 0x80000000 bit indicates a disabled window.
	pub fn get_window_style(&self, window_id: u32) -> u32 {
		self.read_cardinal_prop_by_atom(window_id as Window, self.atoms.wine_hwnd_style, 0)
	}

	/// Write a single CARDINAL (format-32) value to a window property.
	///
	/// Gamescope: writes GAMESCOPE_FOCUSED_APP, GAMESCOPE_FOCUSABLE_APPS,
	/// and GAMESCOPE_FOCUSABLE_WINDOWS to the root window so Steam knows
	/// which window/app is focused and which are focusable (controller routing).
	fn write_cardinal_prop(&self, window_id: Window, atom: Atom, value: u32) {
		if self.dpy.is_null() {
			return;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let change = loaded.xchangeproperty?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				// Replace = 1; Format = 32; type = CARDINAL; data = value
				// XChangeProperty with format=32 expects native C `long`
				// elements.  On LP64 (64-bit) `long` is 8 bytes but `u32`
				// is 4 bytes, so we must convert to native `long` to avoid
				// reading garbage past the value.
				let value_long = value as libc_c_long;
				change(
					self.dpy,
					window_id,
					atom,
					XA_CARDINAL,
					32,
					1,
					&value_long as *const libc_c_long as *const c_void,
					1,
				);
				seterr(prev);
			}
			Some(())
		});
	}

	/// Write an array of CARDINAL (format-32) values to a window property.
	fn write_cardinal_array(&self, window_id: Window, atom: Atom, values: &[u32]) {
		if self.dpy.is_null() || values.is_empty() {
			return;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let change = loaded.xchangeproperty?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				// Replace = 1; Format = 32; type = CARDINAL
				// XChangeProperty with format=32 expects native C `long`
				// elements.  On LP64 (64-bit) `long` is 8 bytes but `u32`
				// is 4 bytes, so we must convert to native `long` to avoid
				// writing past each 4-byte value and corrupting the payload.
				let longs: Vec<libc_c_long> = values.iter().map(|&v| v as libc_c_long).collect();
				change(
					self.dpy,
					window_id,
					atom,
					XA_CARDINAL,
					32,
					1,
					longs.as_ptr() as *const c_void,
					values.len() as c_int,
				);
				seterr(prev);
			}
			Some(())
		});
	}

	/// Delete a window property.
	fn delete_property(&self, window_id: Window, atom: Atom) {
		if self.dpy.is_null() {
			return;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let delete = loaded.xdeleteproperty?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				delete(self.dpy, window_id, atom);
				seterr(prev);
			}
			Some(())
		});
	}

	/// Write the focused app ID to GAMESCOPE_FOCUSED_APP on the root window.
	///
	/// Gamescope: `set_focused_app()` — tells Steam which app ID is currently
	/// focused so it can route controller input appropriately.
	pub fn set_focused_app(&self, app_id: u32) {
		if self.atoms.gamescope_focused_app == 0 {
			return;
		}
		self.write_cardinal_prop(self.root, self.atoms.gamescope_focused_app, app_id);
	}

	/// Clear GAMESCOPE_FOCUSED_APP from the root window.
	pub fn clear_focused_app(&self) {
		if self.atoms.gamescope_focused_app == 0 {
			return;
		}
		self.delete_property(self.root, self.atoms.gamescope_focused_app);
	}

	/// Write the list of focusable app IDs to GAMESCOPE_FOCUSABLE_APPS on the root window.
	///
	/// Gamescope: `set_focusable_apps()` — tells Steam which app IDs are
	/// focusable, used for controller routing and gamepad navigation.
	pub fn set_focusable_apps(&self, app_ids: &[u32]) {
		if self.atoms.gamescope_focusable_apps == 0 || app_ids.is_empty() {
			return;
		}
		self.write_cardinal_array(self.root, self.atoms.gamescope_focusable_apps, app_ids);
	}

	/// Clear GAMESCOPE_FOCUSABLE_APPS from the root window.
	pub fn clear_focusable_apps(&self) {
		if self.atoms.gamescope_focusable_apps == 0 {
			return;
		}
		self.delete_property(self.root, self.atoms.gamescope_focusable_apps);
	}

	/// Write the list of focusable X11 window triplets to GAMESCOPE_FOCUSABLE_WINDOWS on the root window.
	///
	/// Each triplet is [window_id, app_id, pid] — 3 u32 values per window.
	/// Gamescope: `set_focusable_windows()` — tells Steam which X11 window IDs
	/// are focusable, used for controller routing.
	pub fn set_focusable_windows(&self, triplets: &[[u32; 3]]) {
		if self.atoms.gamescope_focusable_windows == 0 || triplets.is_empty() {
			return;
		}
		// Flatten the triplets into a single array for writing.
		let flat: Vec<u32> = triplets.iter().flat_map(|t| [t[0], t[1], t[2]]).collect();
		self.write_cardinal_array(self.root, self.atoms.gamescope_focusable_windows, &flat);
	}

	/// Clear GAMESCOPE_FOCUSABLE_WINDOWS from the root window.
	pub fn clear_focusable_windows(&self) {
		if self.atoms.gamescope_focusable_windows == 0 {
			return;
		}
		self.delete_property(self.root, self.atoms.gamescope_focusable_windows);
	}

	pub fn get_net_wm_state(&self, window: u32) -> Option<Vec<Atom>> {
		if self.dpy.is_null() {
			return None;
		}
		with_xlib(|loaded| {
			let seterr = loaded.xseterrorhandler?;
			let get_prop = loaded.xgetwindowproperty?;
			let free = loaded.xfree?;
			unsafe {
				let prev = seterr(Some(silent_x11_error));
				let atom_net_wm_state = self.atoms.net_wm_state;
				if atom_net_wm_state == 0 {
					seterr(prev);
					return None;
				}
				let mut type_out: Atom = 0;
				let mut format_out: c_int = 0;
				let mut nitems_out: libc_c_ulong = 0;
				let mut bytes_after_out: libc_c_ulong = 0;
				let mut prop_ptr: *mut u8 = std::ptr::null_mut();
				let result = get_prop(
					self.dpy,
					window as Window,
					atom_net_wm_state,
					0,
					1024,
					0,
					4, // XA_ATOM
					&mut type_out,
					&mut format_out,
					&mut nitems_out,
					&mut bytes_after_out,
					&mut prop_ptr,
				);
				if result != 0 || prop_ptr.is_null() || nitems_out == 0 || format_out != 32 {
					if !prop_ptr.is_null() {
						free(prop_ptr as *mut c_void);
					}
					seterr(prev);
					return None;
				}
				debug_assert!(
					nitems_out <= 1024,
					"XGetWindowProperty returned more atoms than requested"
				);
				// format-32 data is an array of native-endian C `long`.
				// Cast to *const libc_c_long and truncate each element to u32
				// (the low 32 bits contain the Atom value on both ILP32 and LP64).
				let longs = std::slice::from_raw_parts(prop_ptr as *const libc_c_long, nitems_out as usize);
				let atoms: Vec<Atom> = longs.iter().map(|&v| v as Atom).collect();
				free(prop_ptr as *mut c_void);
				seterr(prev);
				Some(atoms)
			}
		})
	}

	/// Read `_NET_WM_STATE` once and check for both SKIP_TASKBAR and SKIP_PAGER
	/// atoms in a single X11 roundtrip. Replaces the old separate
	/// `has_net_wm_state_skip_taskbar()` / `has_net_wm_state_skip_pager()` calls
	/// that each did their own `XGetWindowProperty`.
	pub fn get_net_wm_state_skip_flags(&self, window: u32) -> (bool, bool) {
		let atom_skip_taskbar = self.atoms.net_wm_state_skip_taskbar;
		let atom_skip_pager = self.atoms.net_wm_state_skip_pager;
		if atom_skip_taskbar == 0 && atom_skip_pager == 0 {
			return (false, false);
		}
		self.get_net_wm_state(window)
			.map(|atoms: Vec<Atom>| {
				(
					atom_skip_taskbar != 0 && atoms.contains(&atom_skip_taskbar),
					atom_skip_pager != 0 && atoms.contains(&atom_skip_pager),
				)
			})
			.unwrap_or((false, false))
	}
}

impl Drop for X11Focus {
	fn drop(&mut self) {
		// load_xlib() was already called in open() — OnceLock ensures it stays loaded.
		let close_fn = with_xlib(|loaded| loaded.xclosedisplay);
		if let Some(close_fn) = close_fn {
			unsafe {
				if !self.dpy.is_null() {
					close_fn(self.dpy);
				}
			}
		}
	}
}
