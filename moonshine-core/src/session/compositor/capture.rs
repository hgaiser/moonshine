//! Capture lifecycle and complete-scene eligibility, independent of input focus.
use super::CaptureMode;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(super) struct CaptureTick {
	pub render: bool,
	pub released: usize,
}

/// Release completed scanout holds and flush client events before the static
/// screen gate. A client waiting for a reusable image cannot commit damage
/// until its buffer release arrives, even when no new capture is needed.
/// Only the encoder's acquire/release consumption flag permits dropping a hold.
pub(super) fn prepare_capture<K: Eq + Hash, B>(
	held: &mut Vec<(Arc<AtomicBool>, K, B)>,
	indices: &mut HashMap<K, usize>,
	screen_dirty: bool,
	capture_age: Duration,
	flush_releases: impl FnOnce(),
) -> CaptureTick {
	let previous = held.len();
	held.retain(|(consumed, buffer_id, _)| {
		if consumed.load(Ordering::Acquire) {
			indices.remove(buffer_id);
			false
		} else {
			true
		}
	});
	let released = previous - held.len();
	if released != 0 {
		flush_releases();
	}
	CaptureTick {
		render: screen_dirty || capture_age >= Duration::from_secs(1),
		released,
	}
}

#[derive(Default)]
pub(super) struct SceneExtras {
	pub cursor: bool,
	pub steam_overlay: bool,
	pub steam_notification: bool,
	pub external_overlay: bool,
	pub dropdown: bool,
	pub decoration: bool,
	pub scaling: bool,
	pub fractional_scale: bool,
}

/// First blocking reason in deterministic scene order. One counter increment
/// per attempted capture; no strings, allocations, or atomics in classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(super) enum DirectReject {
	ForcedComposition,
	Cursor,
	Overlay,
	Notification,
	ExternalOverlay,
	Dropdown,
	Decoration,
	Scaling,
	FractionalScale,
	OutputOrigin,
	NotOpaque,
	SurfaceTree,
	Transform,
	Crop,
	Size,
	NoSurface,
	NotDmabuf,
	Other,
}
impl DirectReject {
	pub const COUNT: usize = 18;
}

impl SceneExtras {
	pub fn rejection(&self, mode: CaptureMode) -> Option<DirectReject> {
		use DirectReject::*;
		[
			(mode == CaptureMode::Composited, ForcedComposition),
			(self.cursor, Cursor),
			(self.steam_overlay, Overlay),
			(self.steam_notification, Notification),
			(self.external_overlay, ExternalOverlay),
			(self.dropdown, Dropdown),
			(self.decoration, Decoration),
			(self.scaling, Scaling),
			(self.fractional_scale, FractionalScale),
		]
		.into_iter()
		.find_map(|(yes, reason)| yes.then_some(reason))
	}
	#[cfg(test)]
	pub fn requires_composition(&self, mode: CaptureMode) -> bool {
		self.rejection(mode).is_some()
	}
}

pub(super) fn surface_view_rejection(
	view: smithay::backend::renderer::utils::SurfaceView,
	buffer_scale: i32,
	transform: smithay::utils::Transform,
	output: smithay::utils::Size<i32, smithay::utils::Logical>,
) -> Option<DirectReject> {
	use DirectReject::*;
	if transform != smithay::utils::Transform::Normal {
		Some(Transform)
	} else if buffer_scale != 1 {
		Some(Scaling)
	} else if view.offset != (0, 0).into() {
		Some(OutputOrigin)
	} else if view.src.loc != (0.0, 0.0).into() {
		Some(Crop)
	} else if view.dst != output {
		Some(Size)
	} else if view.src.size != output.to_f64() {
		Some(Scaling)
	} else {
		None
	}
}
#[cfg(test)]
fn surface_view_is_direct(
	view: smithay::backend::renderer::utils::SurfaceView,
	buffer_scale: i32,
	transform: smithay::utils::Transform,
	output: smithay::utils::Size<i32, smithay::utils::Logical>,
) -> bool {
	surface_view_rejection(view, buffer_scale, transform, output).is_none()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::compositor::cursor::CursorState;
	use smithay::input::pointer::CursorImageStatus;
	use std::cell::Cell;
	use std::rc::Rc;

	struct BufferHold(Rc<Cell<usize>>);
	impl Drop for BufferHold {
		fn drop(&mut self) {
			self.0.set(self.0.get() + 1);
		}
	}

	#[test]
	fn static_ticks_release_and_flush_completed_three_image_swapchain() {
		let released = Rc::new(Cell::new(0));
		let flags: Vec<_> = (0..3).map(|_| Arc::new(AtomicBool::new(false))).collect();
		let mut held: Vec<_> = flags
			.iter()
			.enumerate()
			.map(|(id, flag)| (flag.clone(), id, BufferHold(released.clone())))
			.collect();
		let mut indices: HashMap<_, _> = (0..3).map(|id| (id, id)).collect();
		// The game has no free image and cannot dirty the screen. Completion
		// occurs asynchronously after capture; no wall-clock wait is needed.
		for (id, flag) in flags.iter().enumerate() {
			flag.store(true, Ordering::Release);
			let tick = prepare_capture(&mut held, &mut indices, false, Duration::from_millis(7), || {
				// Flush must happen AFTER dropping the hold queues wl_buffer.release.
				assert_eq!(released.get(), id + 1);
			});
			assert!(!tick.render);
			assert_eq!(tick.released, 1);
			assert_eq!(held.len(), 2 - id);
			assert!(!indices.contains_key(&id));
			assert_eq!(indices.len(), held.len());
		}
		assert_eq!(released.get(), 3);
	}

	#[test]
	fn static_ticks_preserve_buffers_still_read_by_encoder() {
		let released = Rc::new(Cell::new(0));
		let flag = Arc::new(AtomicBool::new(false));
		let mut held = vec![(flag.clone(), 0, BufferHold(released.clone()))];
		let mut indices = HashMap::from([(0, 0)]);
		for _ in 0..10_000 {
			let tick = prepare_capture(&mut held, &mut indices, false, Duration::from_millis(7), || {
				panic!("unconsumed buffers must not be released or flushed");
			});
			assert!(!tick.render);
			assert_eq!(tick.released, 0);
		}
		assert_eq!(held.len(), 1);
		assert_eq!(indices.len(), 1);
		assert_eq!(released.get(), 0);
	}

	#[test]
	fn dirty_and_keepalive_ticks_still_render_and_release() {
		for (dirty, age) in [(true, Duration::ZERO), (false, Duration::from_secs(1))] {
			let released = Rc::new(Cell::new(0));
			let mut held = vec![(Arc::new(AtomicBool::new(true)), 0, BufferHold(released.clone()))];
			let mut indices = HashMap::from([(0, 0)]);
			let flushed = Cell::new(false);
			let tick = prepare_capture(&mut held, &mut indices, dirty, age, || flushed.set(true));
			assert!(tick.render);
			assert_eq!(tick.released, 1);
			assert_eq!(released.get(), 1);
			assert!(flushed.get());
			assert!(held.is_empty() && indices.is_empty());
		}
	}

	#[test]
	fn cropping_scaling_offsets_and_rotation_require_composition() {
		use smithay::backend::renderer::utils::SurfaceView;
		use smithay::utils::{Rectangle, Transform};
		let output = (1920, 1080).into();
		let view = SurfaceView {
			src: Rectangle::from_size((1920.0, 1080.0).into()),
			dst: output,
			offset: (0, 0).into(),
		};
		assert!(surface_view_is_direct(view, 1, Transform::Normal, output));
		assert!(!surface_view_is_direct(view, 2, Transform::Normal, output));
		assert!(!surface_view_is_direct(view, 1, Transform::_90, output));
		assert!(!surface_view_is_direct(
			SurfaceView {
				offset: (1, 0).into(),
				..view
			},
			1,
			Transform::Normal,
			output
		));
		assert!(!surface_view_is_direct(
			SurfaceView {
				dst: (1280, 720).into(),
				..view
			},
			1,
			Transform::Normal,
			output
		));
		assert!(!surface_view_is_direct(
			SurfaceView {
				src: Rectangle::from_size((3840.0, 2160.0).into()),
				..view
			},
			1,
			Transform::Normal,
			output
		));
	}

	#[test]
	fn configuration_defaults_and_only_complete_scene_modes() {
		let config: super::super::CompositorConfig = toml::from_str("").unwrap();
		assert_eq!(config.capture_mode, CaptureMode::Auto);
		let config: super::super::CompositorConfig = toml::from_str("capture_mode = \"composited\"").unwrap();
		assert_eq!(config.capture_mode, CaptureMode::Composited);
		assert!(toml::from_str::<super::super::CompositorConfig>("capture_mode = \"direct\"").is_err());
	}

	#[test]
	fn fullscreen_scene_can_resume_direct_after_each_extra_disappears() {
		let mut scene = SceneExtras::default();
		assert!(!scene.requires_composition(CaptureMode::Auto));
		for field in [0, 1, 2, 3, 4, 5, 6] {
			match field {
				0 => scene.cursor = true,
				1 => scene.steam_overlay = true,
				2 => scene.steam_notification = true,
				3 => scene.external_overlay = true,
				4 => scene.dropdown = true,
				5 => scene.decoration = true,
				_ => scene.scaling = true,
			}
			assert!(scene.requires_composition(CaptureMode::Auto));
			scene = SceneExtras::default();
			assert!(!scene.requires_composition(CaptureMode::Auto));
		}
		assert!(scene.requires_composition(CaptureMode::Composited));
	}
	#[test]
	fn cursor_intent_drives_scanout_without_an_idle_timeout() {
		let mut cursor = CursorState::default();
		assert!(
			!SceneExtras {
				cursor: cursor.visible(),
				..Default::default()
			}
			.requires_composition(CaptureMode::Auto)
		);
		cursor.activate_pointer();
		assert!(
			SceneExtras {
				cursor: cursor.visible(),
				..Default::default()
			}
			.requires_composition(CaptureMode::Auto)
		);
		cursor.set_image(CursorImageStatus::Hidden);
		assert!(
			!SceneExtras {
				cursor: cursor.visible(),
				..Default::default()
			}
			.requires_composition(CaptureMode::Auto)
		);
	}
	#[test]
	fn direct_rejections_classify_scene_and_surface_requirements() {
		let scene = SceneExtras {
			cursor: true,
			steam_overlay: true,
			..Default::default()
		};
		assert_eq!(scene.rejection(CaptureMode::Auto), Some(DirectReject::Cursor));
		assert_eq!(
			scene.rejection(CaptureMode::Composited),
			Some(DirectReject::ForcedComposition)
		);
		assert_eq!(
			SceneExtras {
				fractional_scale: true,
				..Default::default()
			}
			.rejection(CaptureMode::Auto),
			Some(DirectReject::FractionalScale)
		);
		use smithay::backend::renderer::utils::SurfaceView;
		use smithay::utils::{Rectangle, Transform};
		let output = (1920, 1080).into();
		let view = SurfaceView {
			src: Rectangle::from_size((1920.0, 1080.0).into()),
			dst: output,
			offset: (0, 0).into(),
		};
		assert_eq!(
			surface_view_rejection(view, 1, Transform::_90, output),
			Some(DirectReject::Transform)
		);
		assert_eq!(
			surface_view_rejection(view, 2, Transform::Normal, output),
			Some(DirectReject::Scaling)
		);
		assert_eq!(
			surface_view_rejection(
				SurfaceView {
					offset: (1, 0).into(),
					..view
				},
				1,
				Transform::Normal,
				output
			),
			Some(DirectReject::OutputOrigin)
		);
		assert_eq!(
			surface_view_rejection(
				SurfaceView {
					src: Rectangle::new((1.0, 0.0).into(), (1919.0, 1080.0).into()),
					..view
				},
				1,
				Transform::Normal,
				output
			),
			Some(DirectReject::Crop)
		);
	}
}

/// Preserve the refresh phase even after missed slots, without catch-up bursts.
/// Rebasing to `now + interval` creates a second clock: a late wakeup would
/// move every later frame callback and capture opportunity off the grid.
pub(super) fn next_refresh_deadline(
	previous: std::time::Instant,
	now: std::time::Instant,
	interval: std::time::Duration,
) -> std::time::Instant {
	let ideal = previous + interval;
	if ideal > now {
		return ideal;
	}
	// Only jump whole slots. The remainder is less than the refresh interval
	// (at most one second), so conversion to u64 nanoseconds is bounded.
	let remainder = now.duration_since(previous).as_nanos() % interval.as_nanos();
	now + interval - std::time::Duration::from_nanos(remainder as u64)
}

/// A refresh slot whose tick wanted a capture while the consumer had no credit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DeferredSlot {
	pub deadline: std::time::Instant,
	commit_generation: u64,
}

/// What consumer demand may do outside a refresh tick.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DemandOpportunity {
	/// No slot is waiting; the next refresh tick samples demand itself.
	None,
	/// A client committed after the slot's frame callbacks: capturing now
	/// would sample the next slot's content early, so the slot is abandoned.
	Superseded,
	/// Capture the scene as it was at this refresh deadline.
	Capture(DeferredSlot),
}

/// The refresh tick is the only capture clock.
///
/// Each tick owns at most one capture opportunity, taken before its frame
/// callbacks are sent. If the consumer cannot accept a frame at the tick, the
/// slot is deferred: a later demand wakeup may complete it, but only while no
/// client has committed since that tick. Demand therefore never opens a slot,
/// never captures twice per refresh, and never samples content rendered in
/// response to the slot's own frame callbacks. A late completion moves only
/// transmit time, not the sampled content; the next tick discards any slot
/// still waiting, so no second clock or catch-up burst can form.
#[derive(Debug, Default)]
pub(super) struct CaptureSchedule {
	deferred: Option<DeferredSlot>,
}

impl CaptureSchedule {
	/// A new refresh deadline supersedes the previous slot. Returns whether
	/// that slot was still waiting for the consumer (and is now skipped).
	pub fn begin_tick(&mut self) -> bool {
		self.deferred.take().is_some()
	}

	/// The tick wanted a capture but admission had no credit.
	pub fn defer(&mut self, deadline: std::time::Instant, commit_generation: u64) {
		self.deferred = Some(DeferredSlot {
			deadline,
			commit_generation,
		});
	}

	/// Consumer demand outside a tick. A slot is completed at most once:
	/// `Capture` must be followed by [`Self::captured`] or the slot remains
	/// open for a later demand within the same refresh interval.
	pub fn demand(&mut self, commit_generation: u64) -> DemandOpportunity {
		match self.deferred {
			None => DemandOpportunity::None,
			Some(slot) if slot.commit_generation != commit_generation => {
				self.deferred = None;
				DemandOpportunity::Superseded
			},
			Some(slot) => DemandOpportunity::Capture(slot),
		}
	}

	/// A frame was published for the current slot.
	pub fn captured(&mut self) {
		self.deferred = None;
	}
}

/// Which scheduling path published a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureTrigger {
	/// The refresh tick itself, before its frame callbacks.
	Refresh,
	/// Late completion of a deferred refresh slot by consumer demand.
	DeferredSlot,
}

/// Bounded per-window sample storage; 5 s at 360 Hz fits without growth.
const CADENCE_SAMPLES: usize = 2048;

/// Capture-cadence diagnostics for `log_stats` summaries.
///
/// Average FPS cannot reveal uneven sampling: 120 captures per second can
/// still repeat or skip application frames. This records, per accepted
/// capture, its spacing, its lateness against the refresh deadline it
/// samples, the scheduling path, and how many client surface commits it
/// covered (0 = repeated content, 1 = one-to-one, 2+ = commits never captured;
/// cursor or overlay surfaces also commit, so this bounds rather than equals
/// skipped game frames). Storage is preallocated; recording never allocates.
pub(super) struct CaptureCadence {
	enabled: bool,
	last_capture: Option<std::time::Instant>,
	last_deadline: Option<std::time::Instant>,
	last_commit_generation: u64,
	intervals_us: Vec<u32>,
	lateness_us: Vec<u32>,
	refresh: u64,
	deferred: u64,
	/// Two captures sampled the same refresh deadline. Must stay zero.
	same_slot: u64,
	commits: [u64; 4],
	/// Ticks that wanted a capture without consumer credit.
	pub deferred_slots: u64,
	/// Deferred slots abandoned because a client committed newer content.
	pub superseded_slots: u64,
	/// Deferred slots still waiting when the next refresh deadline arrived.
	pub expired_slots: u64,
	/// Same coverage for commits of the WSI-presented game surface only,
	/// excluding cursor, overlay and other client surfaces.
	last_source_generation: u64,
	source_commits: [u64; 4],
	/// When game-surface commits land, measured from the last refresh
	/// deadline. A game paced by frame callbacks commits shortly after each
	/// tick; a game on its own clock spreads across the whole slot.
	source_commit_phase_us: Vec<u32>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CadenceSummary {
	pub captures: usize,
	pub interval_us: [u32; 5],
	pub lateness_us: [u32; 3],
	pub refresh: u64,
	pub deferred: u64,
	pub same_slot: u64,
	pub commits: [u64; 4],
	pub deferred_slots: u64,
	pub superseded_slots: u64,
	pub expired_slots: u64,
	pub source_commits: [u64; 4],
	pub source_commit_count: usize,
	pub source_commit_phase_us: [u32; 3],
}

impl CaptureCadence {
	pub fn new(enabled: bool) -> Self {
		let capacity = if enabled { CADENCE_SAMPLES } else { 0 };
		Self {
			enabled,
			last_capture: None,
			last_deadline: None,
			last_commit_generation: 0,
			intervals_us: Vec::with_capacity(capacity),
			lateness_us: Vec::with_capacity(capacity),
			refresh: 0,
			deferred: 0,
			same_slot: 0,
			commits: [0; 4],
			deferred_slots: 0,
			superseded_slots: 0,
			expired_slots: 0,
			last_source_generation: 0,
			source_commits: [0; 4],
			source_commit_phase_us: Vec::with_capacity(capacity),
		}
	}

	/// A commit of the captured game surface, `phase` after the last deadline.
	pub fn record_source_commit(&mut self, phase: Duration) {
		if self.enabled && self.source_commit_phase_us.len() < CADENCE_SAMPLES {
			self.source_commit_phase_us
				.push(phase.as_micros().min(u128::from(u32::MAX)) as u32);
		}
	}

	pub fn record(
		&mut self,
		now: std::time::Instant,
		deadline: std::time::Instant,
		trigger: CaptureTrigger,
		commit_generation: u64,
		source_generation: u64,
	) {
		if !self.enabled {
			return;
		}
		let micros = |duration: Duration| duration.as_micros().min(u128::from(u32::MAX)) as u32;
		if let Some(last) = self.last_capture
			&& self.intervals_us.len() < CADENCE_SAMPLES
		{
			self.intervals_us.push(micros(now.saturating_duration_since(last)));
		}
		if self.lateness_us.len() < CADENCE_SAMPLES {
			self.lateness_us.push(micros(now.saturating_duration_since(deadline)));
		}
		if self.last_deadline == Some(deadline) {
			self.same_slot += 1;
		}
		match trigger {
			CaptureTrigger::Refresh => self.refresh += 1,
			CaptureTrigger::DeferredSlot => self.deferred += 1,
		}
		if self.last_capture.is_some() {
			let commits = commit_generation.wrapping_sub(self.last_commit_generation);
			self.commits[commits.min(3) as usize] += 1;
			let source = source_generation.wrapping_sub(self.last_source_generation);
			self.source_commits[source.min(3) as usize] += 1;
		}
		self.last_source_generation = source_generation;
		self.last_capture = Some(now);
		self.last_deadline = Some(deadline);
		self.last_commit_generation = commit_generation;
	}

	/// Summarize and clear the window, keeping the allocation and the last
	/// capture so the next window's first interval is still measured.
	pub fn take_summary(&mut self) -> CadenceSummary {
		fn percentile(sorted: &[u32], fraction: f64) -> u32 {
			if sorted.is_empty() {
				return 0;
			}
			sorted[((sorted.len() - 1) as f64 * fraction).round() as usize]
		}
		self.intervals_us.sort_unstable();
		self.lateness_us.sort_unstable();
		self.source_commit_phase_us.sort_unstable();
		let i = &self.intervals_us;
		let l = &self.lateness_us;
		let p = &self.source_commit_phase_us;
		let summary = CadenceSummary {
			captures: l.len(),
			interval_us: [
				i.first().copied().unwrap_or(0),
				percentile(i, 0.50),
				percentile(i, 0.95),
				percentile(i, 0.99),
				i.last().copied().unwrap_or(0),
			],
			lateness_us: [percentile(l, 0.50), percentile(l, 0.99), l.last().copied().unwrap_or(0)],
			refresh: self.refresh,
			deferred: self.deferred,
			same_slot: self.same_slot,
			commits: self.commits,
			deferred_slots: self.deferred_slots,
			superseded_slots: self.superseded_slots,
			expired_slots: self.expired_slots,
			source_commits: self.source_commits,
			source_commit_count: p.len(),
			source_commit_phase_us: [percentile(p, 0.05), percentile(p, 0.50), percentile(p, 0.95)],
		};
		self.source_commit_phase_us.clear();
		self.source_commits = [0; 4];
		self.intervals_us.clear();
		self.lateness_us.clear();
		self.refresh = 0;
		self.deferred = 0;
		self.same_slot = 0;
		self.commits = [0; 4];
		self.deferred_slots = 0;
		self.superseded_slots = 0;
		self.expired_slots = 0;
		summary
	}
}

#[cfg(test)]
mod cadence_tests {
	use super::*;
	use std::time::Instant;

	#[test]
	fn late_refresh_wakeup_keeps_the_refresh_phase() {
		let start = Instant::now();
		let interval = Duration::from_nanos(8_333_333);
		let late = start + interval + interval / 2;
		// A stalled tick skips a slot without moving callbacks half a refresh
		// for the rest of the retained game session.
		let mut next = next_refresh_deadline(start, late, interval);
		assert_eq!(next, start + interval * 2);
		for index in 2..1000 {
			let due = start + interval * index;
			next = next_refresh_deadline(next, due + Duration::from_micros(50), interval);
			assert_eq!(next, start + interval * (index + 1));
		}
	}

	#[test]
	fn long_pause_skips_slots_without_rebasing_or_catchup() {
		let start = Instant::now();
		let interval = Duration::from_millis(8);
		let now = start + Duration::from_secs(37) + Duration::from_millis(3);
		let next = next_refresh_deadline(start, now, interval);
		assert!(next > now);
		assert!(next <= now + interval);
		assert_eq!(next.duration_since(start).as_nanos() % interval.as_nanos(), 0);
	}

	#[test]
	fn jitter_preserves_cadence_but_missed_slots_do_not_burst() {
		let start = Instant::now();
		let interval = Duration::from_millis(8);
		assert_eq!(
			next_refresh_deadline(start, start + Duration::from_millis(1), interval),
			start + interval
		);
		assert_eq!(
			next_refresh_deadline(start, start + interval * 5, interval),
			start + interval * 6
		);
	}

	#[test]
	fn demand_cannot_open_a_slot() {
		let mut schedule = CaptureSchedule::default();
		for generation in 0..100 {
			assert_eq!(schedule.demand(generation), DemandOpportunity::None);
		}
	}

	#[test]
	fn deferred_slot_is_completed_at_most_once_and_coalesces_demand() {
		let deadline = Instant::now();
		let mut schedule = CaptureSchedule::default();
		assert!(!schedule.begin_tick());
		schedule.defer(deadline, 7);
		// Demand without an available credit leaves the slot for a later wakeup.
		assert!(matches!(schedule.demand(7), DemandOpportunity::Capture(s) if s.deadline == deadline));
		assert!(matches!(schedule.demand(7), DemandOpportunity::Capture(_)));
		schedule.captured();
		for _ in 0..100 {
			assert_eq!(schedule.demand(7), DemandOpportunity::None);
		}
		assert!(!schedule.begin_tick(), "a completed slot is not skipped");
	}

	#[test]
	fn commit_after_the_tick_supersedes_the_deferred_slot() {
		let mut schedule = CaptureSchedule::default();
		schedule.begin_tick();
		schedule.defer(Instant::now(), 41);
		// The application answered this tick's frame callback before the
		// consumer became ready; that frame belongs to the next refresh.
		assert_eq!(schedule.demand(42), DemandOpportunity::Superseded);
		assert_eq!(schedule.demand(42), DemandOpportunity::None);
		assert!(!schedule.begin_tick());
	}

	#[test]
	fn next_tick_expires_a_waiting_slot() {
		let mut schedule = CaptureSchedule::default();
		schedule.begin_tick();
		schedule.defer(Instant::now(), 3);
		assert!(schedule.begin_tick());
		assert_eq!(schedule.demand(3), DemandOpportunity::None);
	}

	#[test]
	fn cadence_reports_spacing_lateness_paths_and_commit_coverage() {
		let start = Instant::now();
		let interval = Duration::from_micros(8_333);
		let mut cadence = CaptureCadence::new(true);
		// Commits 1, 2, 2 (repeat), 5 (two skipped).
		let captures = [
			(0, 1, CaptureTrigger::Refresh, 0),
			(1, 2, CaptureTrigger::Refresh, 0),
			(2, 2, CaptureTrigger::DeferredSlot, 600),
			(3, 5, CaptureTrigger::Refresh, 0),
		];
		for (slot, generation, trigger, late) in captures {
			let deadline = start + interval * slot;
			cadence.record(
				deadline + Duration::from_micros(late),
				deadline,
				trigger,
				generation,
				generation,
			);
		}
		let summary = cadence.take_summary();
		assert_eq!(summary.captures, 4);
		assert_eq!(summary.refresh, 3);
		assert_eq!(summary.deferred, 1);
		assert_eq!(summary.same_slot, 0);
		assert_eq!(summary.commits, [1, 1, 0, 1]);
		assert_eq!(summary.interval_us[0], 7_733);
		assert_eq!(summary.interval_us[4], 8_933);
		assert_eq!(summary.lateness_us[2], 600);
		// The next window starts empty but still measures its first spacing.
		cadence.record(
			start + interval * 4,
			start + interval * 4,
			CaptureTrigger::Refresh,
			6,
			6,
		);
		let next = cadence.take_summary();
		assert_eq!(next.captures, 1);
		assert_eq!(next.interval_us[0], 8_333);
		assert_eq!(next.commits, [0, 1, 0, 0]);
	}

	#[test]
	fn cadence_flags_two_captures_of_one_refresh_deadline() {
		let deadline = Instant::now();
		let mut cadence = CaptureCadence::new(true);
		cadence.record(deadline, deadline, CaptureTrigger::Refresh, 1, 1);
		cadence.record(deadline, deadline, CaptureTrigger::DeferredSlot, 1, 1);
		assert_eq!(cadence.take_summary().same_slot, 1);
	}

	#[test]
	fn disabled_cadence_does_not_allocate_or_record() {
		let mut cadence = CaptureCadence::new(false);
		let now = Instant::now();
		for generation in 0..10_000 {
			cadence.record(now, now, CaptureTrigger::Refresh, generation, generation);
		}
		assert_eq!(cadence.intervals_us.capacity(), 0);
		assert_eq!(cadence.take_summary(), CadenceSummary::default());
	}
}

/// Refresh/demand scheduling against the real admission channel, following
/// `MoonshineCompositor::render_and_export`'s decision order: a tick begins a
/// slot, captures when admission has a credit (before its frame callbacks) or
/// defers; demand may only complete the deferred slot.
#[cfg(test)]
mod scheduling_tests {
	use super::*;
	use crate::session::compositor::admission::{CaptureReceiver, CaptureSender, capture_channel};
	use crate::session::compositor::frame::ExportedFrame;
	use std::time::Instant;

	const INTERVAL: Duration = Duration::from_nanos(8_333_333);

	/// One capture: the refresh slot it sampled, the content it carried and
	/// whether a frame callback for that slot had already been sent.
	#[derive(Debug, PartialEq, Eq)]
	struct Capture {
		slot: u32,
		commit: u64,
		trigger: CaptureTrigger,
	}

	struct Harness {
		tx: CaptureSender,
		rx: CaptureReceiver,
		schedule: CaptureSchedule,
		start: Instant,
		slot: u32,
		commit_generation: u64,
		dirty: bool,
		captures: Vec<Capture>,
		/// Frames the consumer holds (each carries its admission credit).
		held: Vec<ExportedFrame>,
	}

	impl Harness {
		fn new() -> Self {
			let (tx, rx) = capture_channel();
			Self {
				tx,
				rx,
				schedule: CaptureSchedule::default(),
				start: Instant::now(),
				slot: 0,
				commit_generation: 0,
				dirty: false,
				captures: Vec::new(),
				held: Vec::new(),
			}
		}
		fn deadline(&self) -> Instant {
			self.start + INTERVAL * self.slot
		}
		/// The application commits a new frame.
		fn commit(&mut self) {
			self.commit_generation += 1;
			self.dirty = true;
		}
		fn tick(&mut self) {
			self.slot += 1;
			self.schedule.begin_tick();
			if !self.dirty {
				return;
			}
			match self.tx.try_acquire() {
				Some(credit) => self.publish(credit, CaptureTrigger::Refresh),
				None => self.schedule.defer(self.deadline(), self.commit_generation),
			}
		}
		fn demand_wakeup(&mut self) {
			if let DemandOpportunity::Capture(slot) = self.schedule.demand(self.commit_generation) {
				assert_eq!(slot.deadline, self.deadline(), "only the current slot is completed");
				if let Some(credit) = self.tx.try_acquire() {
					self.publish(credit, CaptureTrigger::DeferredSlot);
				}
			}
		}
		fn publish(&mut self, credit: crate::session::compositor::admission::CaptureCredit, trigger: CaptureTrigger) {
			if self.tx.try_send(ExportedFrame::for_test(), credit).is_ok() {
				self.dirty = false;
				self.schedule.captured();
				self.captures.push(Capture {
					slot: self.slot,
					commit: self.commit_generation,
					trigger,
				});
			}
		}
		/// The consumer asks for one frame (a receiver blocked in `recv_timeout`).
		fn request(&mut self) {
			if !self.tx.occupied() && !self.tx.requested() {
				self.rx.request_for_test();
			}
			self.demand_wakeup();
		}
		fn receive(&mut self) {
			if let Ok(frame) = self.rx.recv_timeout(Duration::ZERO) {
				self.held.push(frame);
			}
		}
		/// Encode/send completion releases the consumer's credit.
		fn complete(&mut self) {
			self.held.clear();
		}
	}

	fn assert_one_capture_per_slot(captures: &[Capture]) {
		for pair in captures.windows(2) {
			assert!(pair[0].slot < pair[1].slot, "two captures in one refresh: {pair:?}");
			assert!(pair[0].commit < pair[1].commit, "repeated content: {pair:?}");
		}
	}

	#[test]
	fn ready_consumer_captures_every_commit_on_its_refresh_tick() {
		let mut h = Harness::new();
		h.request();
		for _ in 0..240 {
			h.tick();
			// Callbacks follow the tick; the application answers them.
			h.commit();
			h.receive();
			h.complete();
			h.request();
		}
		assert_eq!(h.captures.len(), 239);
		assert!(h.captures.iter().all(|c| c.trigger == CaptureTrigger::Refresh));
		assert!(h.captures.iter().all(|c| c.commit == u64::from(c.slot) - 1));
		assert_one_capture_per_slot(&h.captures);
	}

	#[test]
	fn late_consumer_completes_the_slot_without_sampling_newer_content() {
		let mut h = Harness::new();
		h.request();
		for slot in 0..240 {
			// Every third frame the consumer finishes after the tick, but
			// before the application answers the tick's frame callback.
			let late = slot % 3 == 0;
			if !late {
				h.complete();
				h.request();
			}
			h.tick();
			if late {
				h.complete();
				h.request();
			}
			h.commit();
			h.receive();
		}
		assert_one_capture_per_slot(&h.captures);
		// The deferred completion carries the tick's content, so no commit
		// is skipped even though a third of the slots completed late.
		for pair in h.captures.windows(2) {
			assert_eq!(pair[1].commit, pair[0].commit + 1, "{pair:?}");
		}
		assert!(h.captures.iter().any(|c| c.trigger == CaptureTrigger::DeferredSlot));
	}

	#[test]
	fn demand_after_the_application_answered_the_callback_waits_for_the_next_tick() {
		let mut h = Harness::new();
		h.request();
		h.tick();
		h.commit();
		h.tick(); // Captures commit 1, then sends callbacks.
		// Consumer still busy at the next tick: the slot is deferred.
		h.commit();
		h.tick();
		assert_eq!(h.captures.len(), 1);
		// The application answers this tick's callback before the consumer
		// is ready. Capturing now would sample the next slot's content and
		// leave the following tick nothing to capture (the off-grid regime).
		h.commit();
		h.receive();
		h.complete();
		for _ in 0..10 {
			h.request();
		}
		assert_eq!(
			h.captures.len(),
			1,
			"demand must not capture superseded content off the grid"
		);
		h.tick();
		assert_eq!(
			h.captures.last(),
			Some(&Capture {
				slot: 4,
				commit: 3,
				trigger: CaptureTrigger::Refresh
			})
		);
	}

	#[test]
	fn reconnect_reset_cannot_add_a_slot_or_let_old_credit_capture() {
		let mut h = Harness::new();
		h.request();
		h.commit();
		h.tick();
		assert_eq!(h.captures.len(), 1);
		h.receive();
		// The old epoch's frame is still being sent when the client resumes.
		let old = std::mem::take(&mut h.held);
		h.commit();
		h.tick(); // Deferred: the old frame holds the only credit.
		h.rx.reset();
		// Releasing the old epoch's credit must not grant new-epoch capture.
		drop(old);
		h.demand_wakeup();
		assert_eq!(h.captures.len(), 1);
		// The new epoch's first demand completes the current slot exactly once.
		for _ in 0..10 {
			h.request();
		}
		assert_eq!(h.captures.len(), 2);
		assert_eq!(h.captures[1].slot, h.slot);
		assert_eq!(h.captures[1].trigger, CaptureTrigger::DeferredSlot);
		h.receive();
		assert_eq!(h.held.len(), 1, "the new epoch receives its capture");
		// Cadence continues on the original grid: the next capture is a tick.
		h.complete();
		h.request();
		h.commit();
		h.tick();
		assert_eq!(
			h.captures.last().map(|c| (c.slot, c.trigger)),
			Some((h.slot, CaptureTrigger::Refresh))
		);
		assert_one_capture_per_slot(&h.captures);
	}

	#[test]
	fn demand_wakeups_never_capture_between_ticks_without_a_deferred_slot() {
		let mut h = Harness::new();
		for _ in 0..100 {
			h.commit();
			h.request();
		}
		assert!(h.captures.is_empty(), "only a refresh tick opens a capture opportunity");
		h.tick();
		assert_eq!(h.captures.len(), 1);
	}
}
