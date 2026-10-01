//! Optional nonblocking GLES timer queries. Results are polled on refresh ticks;
//! unavailable/disjoint queries are never turned into a CPU wait.
use smithay::backend::renderer::gles::GlesRenderer;
use std::ffi::CStr;

#[derive(Default)]
pub(super) struct GpuTimer {
	initialized: bool,
	supported: bool,
	queries: [u32; 4],
	pending: [bool; 4],
	active: Option<usize>,
	pub samples: u64,
	pub nanoseconds: u64,
	pub disjoint: u64,
}
impl GpuTimer {
	pub fn poll(&mut self, renderer: &mut GlesRenderer) {
		let _ = renderer.with_context(|gl| unsafe {
			if !self.initialized {
				self.initialized = true;
				let extensions = gl.GetString(0x1F03); // GL_EXTENSIONS (GLES)
				self.supported = !extensions.is_null()
					&& CStr::from_ptr(extensions.cast())
						.to_bytes()
						.split(|c| *c == b' ')
						.any(|s| s == b"GL_EXT_disjoint_timer_query")
					&& gl.GenQueriesEXT.is_loaded()
					&& gl.BeginQueryEXT.is_loaded()
					&& gl.EndQueryEXT.is_loaded()
					&& gl.GetQueryObjectuivEXT.is_loaded()
					&& gl.GetQueryObjectui64vEXT.is_loaded()
					&& gl.GetQueryivEXT.is_loaded();
				if self.supported {
					gl.GenQueriesEXT(4, self.queries.as_mut_ptr());
				}
			}
			if !self.supported {
				return;
			}
			let mut disjoint = 0;
			gl.GetIntegerv(0x8FBB, &mut disjoint); // GL_GPU_DISJOINT_EXT
			if disjoint != 0 {
				self.disjoint += 1;
			}
			for i in 0..4 {
				if !self.pending[i] {
					continue;
				}
				let mut available = 0;
				gl.GetQueryObjectuivEXT(self.queries[i], 0x8867, &mut available); // QUERY_RESULT_AVAILABLE
				if available == 0 {
					// Discard all pending measurements on a disjoint event, including
					// unfinished ones. Query objects can be reused without reading.
					if disjoint != 0 {
						self.pending[i] = false;
					}
					continue;
				}
				self.pending[i] = false;
				if disjoint == 0 {
					let mut ns = 0;
					gl.GetQueryObjectui64vEXT(self.queries[i], 0x8866, &mut ns); // QUERY_RESULT
					self.nanoseconds = self.nanoseconds.saturating_add(ns);
					self.samples += 1;
				}
			}
		});
	}
	pub fn begin(&mut self, renderer: &mut GlesRenderer) {
		if !self.initialized {
			self.poll(renderer);
		}
		if !self.supported {
			return;
		}
		let Some(index) = self.pending.iter().position(|p| !p) else {
			return;
		};
		let _ = renderer.with_context(|gl| unsafe {
			let mut current = 0;
			gl.GetQueryivEXT(0x88BF, 0x8865, &mut current); // TIME_ELAPSED / CURRENT_QUERY
			if current == 0 {
				gl.BeginQueryEXT(0x88BF, self.queries[index]);
				self.active = Some(index);
			}
		});
	}
	pub fn end(&mut self, renderer: &mut GlesRenderer) {
		let Some(index) = self.active.take() else {
			return;
		};
		let _ = renderer.with_context(|gl| unsafe {
			gl.EndQueryEXT(0x88BF);
			// Smithay flushed its render fence before EndQuery. Flush this trailing
			// query too, including the last frame before a static-screen skip.
			gl.Flush();
			self.pending[index] = true;
		});
	}
	pub fn has_pending(&self) -> bool {
		self.pending.iter().any(|p| *p)
	}
	pub fn supported(&self) -> bool {
		self.supported
	}
	pub fn reset_window(&mut self) {
		self.samples = 0;
		self.nanoseconds = 0;
		self.disjoint = 0;
	}
	pub fn destroy(&mut self, renderer: &mut GlesRenderer) {
		if self.supported {
			let _ = renderer.with_context(|gl| unsafe {
				gl.DeleteQueriesEXT(4, self.queries.as_ptr());
			});
			self.supported = false;
		}
	}
}
