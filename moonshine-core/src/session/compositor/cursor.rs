//! Cursor rendering support.
//!
//! Loads an XCursor theme at startup and provides a `PointerElement` that
//! can render either a named (fallback) cursor image or a client-provided
//! cursor surface into the composited frame.

use std::io::Read;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer, Texture};
use smithay::input::pointer::CursorImageStatus;
use smithay::utils::{Physical, Point, Scale, Transform};

// ── XCursor loading ──────────────────────────────────────────────────

/// Load the default cursor from the XCursor theme and return a
/// `MemoryRenderBuffer` suitable for compositing.
pub(crate) fn load_default_cursor() -> MemoryRenderBuffer {
	let name = std::env::var("XCURSOR_THEME").ok().unwrap_or_else(|| "default".into());
	let size = std::env::var("XCURSOR_SIZE")
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(24);

	let theme = xcursor::CursorTheme::load(&name);
	let image = load_icon(&theme, size).unwrap_or_else(|e| {
		tracing::warn!("Failed to load xcursor theme: {e}, using fallback cursor");
		fallback_cursor()
	});

	MemoryRenderBuffer::from_slice(
		&image.pixels_rgba,
		Fourcc::Abgr8888,
		(image.width as i32, image.height as i32),
		1,
		Transform::Normal,
		None,
	)
}

fn load_icon(theme: &xcursor::CursorTheme, size: u32) -> Result<xcursor::parser::Image, CursorLoadError> {
	let icon_path = theme.load_icon("default").ok_or(CursorLoadError::NoDefaultCursor)?;
	let mut cursor_file = std::fs::File::open(icon_path)?;
	let mut cursor_data = Vec::new();
	cursor_file.read_to_end(&mut cursor_data)?;
	let images = xcursor::parser::parse_xcursor(&cursor_data).ok_or(CursorLoadError::Parse)?;

	// Pick the image closest to the requested size.
	let image = images
		.iter()
		.min_by_key(|img| (size as i32 - img.size as i32).abs())
		.cloned()
		.ok_or(CursorLoadError::Parse)?;

	Ok(image)
}

/// A simple 1×1 white pixel as a last-resort cursor.
fn fallback_cursor() -> xcursor::parser::Image {
	xcursor::parser::Image {
		size: 1,
		width: 1,
		height: 1,
		xhot: 0,
		yhot: 0,
		delay: 0,
		pixels_rgba: vec![0xFF, 0xFF, 0xFF, 0xFF],
		pixels_argb: vec![],
	}
}

#[derive(Debug)]
enum CursorLoadError {
	NoDefaultCursor,
	Io(std::io::Error),
	Parse,
}

impl std::fmt::Display for CursorLoadError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::NoDefaultCursor => write!(f, "theme has no default cursor"),
			Self::Io(e) => write!(f, "{e}"),
			Self::Parse => write!(f, "failed to parse xcursor file"),
		}
	}
}

impl From<std::io::Error> for CursorLoadError {
	fn from(e: std::io::Error) -> Self {
		Self::Io(e)
	}
}

// ── PointerElement ───────────────────────────────────────────────────

/// Tracks cursor image status and renders the appropriate cursor.
pub(crate) struct PointerElement {
	buffer: Option<MemoryRenderBuffer>,
	status: CursorImageStatus,
}

impl Default for PointerElement {
	fn default() -> Self {
		Self {
			buffer: None,
			status: CursorImageStatus::default_named(),
		}
	}
}

impl PointerElement {
	pub fn set_status(&mut self, status: CursorImageStatus) {
		self.status = status;
	}

	pub fn set_buffer(&mut self, buffer: MemoryRenderBuffer) {
		self.buffer = Some(buffer);
	}
}

// ── Render element enum ──────────────────────────────────────────────

smithay::backend::renderer::element::render_elements! {
	pub PointerRenderElement<R> where R: ImportAll + ImportMem;
	Surface=WaylandSurfaceRenderElement<R>,
	Memory=MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer> std::fmt::Debug for PointerRenderElement<R> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Surface(e) => f.debug_tuple("Surface").field(e).finish(),
			Self::Memory(e) => f.debug_tuple("Memory").field(e).finish(),
			Self::_GenericCatcher(e) => f.debug_tuple("_GenericCatcher").field(e).finish(),
		}
	}
}

impl<T: Texture + Clone + Send + 'static, R> AsRenderElements<R> for PointerElement
where
	R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
	type RenderElement = PointerRenderElement<R>;

	fn render_elements<E>(
		&self,
		renderer: &mut R,
		location: Point<i32, Physical>,
		scale: Scale<f64>,
		alpha: f32,
	) -> Vec<E>
	where
		E: From<PointerRenderElement<R>>,
	{
		match &self.status {
			CursorImageStatus::Hidden => vec![],
			CursorImageStatus::Named(_) => {
				if let Some(buffer) = self.buffer.as_ref() {
					vec![
						PointerRenderElement::<R>::from(
							MemoryRenderBufferRenderElement::from_buffer(
								renderer,
								location.to_f64(),
								buffer,
								None,
								None,
								None,
								Kind::Cursor,
							)
							.expect("Lost system pointer buffer"),
						)
						.into(),
					]
				} else {
					vec![]
				}
			},
			CursorImageStatus::Surface(surface) => {
				let elements: Vec<PointerRenderElement<R>> =
					smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
						renderer,
						surface,
						location,
						scale,
						alpha,
						Kind::Cursor,
					);
				elements.into_iter().map(E::from).collect()
			},
		}
	}
}

/// Cursor visibility has no clock and receives no controller events.
/// Smithay's default Named callbacks are fallback resets on focus leave or
/// replacement. This compositor does not advertise wp_cursor_shape_manager;
/// client wl_pointer.set_cursor requests arrive as Surface or Hidden.
pub(crate) struct CursorState {
	pub image: CursorImageStatus,
	active: bool,
}

impl Default for CursorState {
	fn default() -> Self {
		Self {
			image: CursorImageStatus::default_named(),
			active: false,
		}
	}
}

impl CursorState {
	pub fn activate_pointer(&mut self) {
		let was_visible = self.visible();
		self.active = true;
		if !was_visible && self.visible() {
			tracing::debug!("Cursor hidden -> visible: pointer activated fallback");
		}
	}
	pub fn set_image(&mut self, image: CursorImageStatus) {
		if !matches!(&image, CursorImageStatus::Named(icon) if *icon == smithay::input::pointer::CursorIcon::Default) {
			self.active = true;
		}
		self.image = image;
	}
	pub fn surface_destroyed(
		&mut self,
		surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
	) -> bool {
		if matches!(&self.image, CursorImageStatus::Surface(current) if current == surface) {
			self.set_image(CursorImageStatus::default_named());
			return true;
		}
		false
	}
	pub fn reset_dead_surface(&mut self) -> bool {
		use smithay::utils::IsAlive;
		if matches!(&self.image, CursorImageStatus::Surface(surface) if !surface.alive()) {
			self.set_image(CursorImageStatus::default_named());
			return true;
		}
		false
	}
	pub fn visible(&self) -> bool {
		self.active && !matches!(self.image, CursorImageStatus::Hidden)
	}
}

#[cfg(test)]
mod visibility_tests {
	use super::*;
	use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
	use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, Resource};
	use std::os::unix::net::UnixStream;
	struct TestState;
	impl Dispatch<WlSurface, ()> for TestState {
		fn request(
			_: &mut Self,
			_: &Client,
			_: &WlSurface,
			_: <WlSurface as Resource>::Request,
			_: &(),
			_: &DisplayHandle,
			_: &mut DataInit<'_, Self>,
		) {
		}
	}
	#[test]
	fn custom_cursor_surface_death_and_replacement_respect_last_request() {
		let display = Display::<TestState>::new().unwrap();
		let mut handle = display.handle();
		let (server, _peer) = UnixStream::pair().unwrap();
		let client = handle.insert_client(server, std::sync::Arc::new(())).unwrap();
		let old = client
			.create_resource::<WlSurface, (), TestState>(&handle, 6, ())
			.unwrap();
		let new = client
			.create_resource::<WlSurface, (), TestState>(&handle, 6, ())
			.unwrap();
		let mut cursor = CursorState::default();
		cursor.set_image(CursorImageStatus::Surface(old.clone()));
		assert!(cursor.visible());
		cursor.set_image(CursorImageStatus::Surface(new.clone()));
		handle.backend_handle().destroy_object::<TestState>(&old.id()).unwrap();
		assert!(
			!cursor.surface_destroyed(&old),
			"old surface death cannot replace the current cursor"
		);
		assert_eq!(cursor.image, CursorImageStatus::Surface(new.clone()));
		handle.backend_handle().destroy_object::<TestState>(&new.id()).unwrap();
		assert!(!new.is_alive());
		assert!(cursor.surface_destroyed(&new));
		assert!(cursor.visible());
		assert_eq!(cursor.image, CursorImageStatus::default_named());
		cursor.set_image(CursorImageStatus::Hidden);
		assert!(!cursor.reset_dead_surface());
		assert!(!cursor.visible());
	}

	#[test]
	fn startup_and_framework_reset_do_not_expose_fallback() {
		let mut cursor = CursorState::default();
		assert!(!cursor.visible());
		cursor.set_image(CursorImageStatus::default_named());
		assert!(!cursor.visible());
	}
	#[test]
	fn mouse_idle_and_controller_updates_cannot_expire_cursor() {
		let mut cursor = CursorState::default();
		cursor.activate_pointer();
		let last_mouse_event = std::time::Instant::now() - std::time::Duration::from_secs(10);
		assert!(last_mouse_event.elapsed() > std::time::Duration::from_secs(3));
		// No cursor API is called by controller updates or advancing time.
		for _controller_update in 0..100 {
			assert!(cursor.visible());
		}
		cursor.set_image(CursorImageStatus::Hidden);
		assert!(!cursor.visible());
		cursor.activate_pointer();
		assert!(!cursor.visible(), "mouse motion must not undo an app hide");
	}
	#[test]
	fn image_replacement_and_active_surface_fallback_preserve_visibility() {
		let mut cursor = CursorState::default();
		cursor.set_image(CursorImageStatus::Named(smithay::input::pointer::CursorIcon::Crosshair));
		assert!(cursor.visible());
		cursor.set_image(CursorImageStatus::default_named());
		assert!(cursor.visible());
		cursor.set_image(CursorImageStatus::Hidden);
		assert!(!cursor.visible());
	}
}
