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
pub fn load_default_cursor() -> MemoryRenderBuffer {
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
pub struct PointerElement {
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
					vec![PointerRenderElement::<R>::from(
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
					.into()]
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
