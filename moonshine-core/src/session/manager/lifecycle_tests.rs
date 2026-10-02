//! Deterministic lifecycle tests for the session manager (STAB-001/002/003).
//!
//! A fake [`SessionBackend`] stands in for the compositor, systemd and stream
//! subsystems. Each operation can be held at a barrier or failed, every fake
//! resource is counted, fake workers register with the session exactly like
//! production workers, and the fake application unit records whether it was
//! created and stopped. No sleeps are used for ordering.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::session::lifecycle::WorkerGuard;
use crate::session::stream::audio::{AudioChannels, AudioConfig};
use crate::session::stream::video::{BitDepth, ChromaFormat, ColorRange, NegotiatedVideoFormat, VideoCodec};
use crate::session::{RemoteInputKey, RemoteInputKeyId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Op {
	Initialize,
	Launch,
	Start,
	Pause,
	Resume,
	StopApplication,
}

const ALL_OPS: [Op; 6] = [
	Op::Initialize,
	Op::Launch,
	Op::Start,
	Op::Pause,
	Op::Resume,
	Op::StopApplication,
];

/// One operation's barrier: entry count, release flag and fault switches.
struct Gate {
	entered: watch::Sender<usize>,
	released: watch::Sender<bool>,
	hold: AtomicBool,
	fail: AtomicBool,
}

impl Gate {
	fn new() -> Self {
		Self {
			entered: watch::channel(0).0,
			released: watch::channel(false).0,
			hold: AtomicBool::new(false),
			fail: AtomicBool::new(false),
		}
	}
}

/// Counts live fake resources; each `Resource` is one socket/thread/GPU owner.
#[derive(Default)]
struct Counters {
	resources: AtomicUsize,
	workers_started: AtomicUsize,
	unit_created: AtomicBool,
	unit_stops: AtomicUsize,
	/// Set if a new session was initialized while an old one still owned resources.
	overlapped: AtomicBool,
	order: std::sync::Mutex<Vec<Op>>,
}

struct Resource(Arc<Counters>);

impl Resource {
	fn new(counters: &Arc<Counters>) -> Self {
		counters.resources.fetch_add(1, Ordering::SeqCst);
		Self(counters.clone())
	}
}

impl Drop for Resource {
	fn drop(&mut self) {
		self.0.resources.fetch_sub(1, Ordering::SeqCst);
	}
}

struct FakeBackend {
	gates: HashMap<Op, Gate>,
	counters: Arc<Counters>,
	/// Workers released only when this opens (models slow worker exit).
	worker_exit: watch::Sender<bool>,
	/// Stop the session right before a successful launch returns.
	stop_on_launch_return: AtomicBool,
	/// The most recent session's stop, so tests can play a failing worker.
	session_stop: std::sync::Mutex<Option<ShutdownManager<SessionShutdownReason>>>,
}

struct FakeSession {
	_resources: Vec<Resource>,
	stop: ShutdownManager<SessionShutdownReason>,
}

impl FakeBackend {
	fn new() -> Self {
		Self {
			gates: ALL_OPS.iter().map(|op| (*op, Gate::new())).collect(),
			counters: Arc::new(Counters::default()),
			worker_exit: watch::channel(true).0,
			stop_on_launch_return: AtomicBool::new(false),
			session_stop: std::sync::Mutex::new(None),
		}
	}

	async fn gate(&self, op: Op) -> Result<(), ()> {
		self.counters.order.lock().unwrap().push(op);
		let gate = &self.gates[&op];
		gate.entered.send_modify(|count| *count += 1);
		if gate.hold.load(Ordering::SeqCst) {
			let mut released = gate.released.subscribe();
			let _ = released.wait_for(|released| *released).await;
		}
		if gate.fail.load(Ordering::SeqCst) {
			Err(())
		} else {
			Ok(())
		}
	}

	/// Spawn a worker registered with the session like production workers.
	fn spawn_worker(&self, stop: &ShutdownManager<SessionShutdownReason>, start: Option<StartLatch>) -> Result<(), ()> {
		let guard = WorkerGuard::register(stop, SessionShutdownReason::VideoPacketHandlerStopped)?;
		let resource = Resource::new(&self.counters);
		let counters = self.counters.clone();
		let stop = stop.clone();
		let mut exit = self.worker_exit.subscribe();
		let waiter = start.map(|latch| latch.waiter());
		tokio::spawn(async move {
			let _guard = guard;
			let _resource = resource;
			if let Some(waiter) = waiter
				&& waiter.wait(&stop).await.is_ok()
			{
				counters.workers_started.fetch_add(1, Ordering::SeqCst);
			}
			stop.wait_shutdown_triggered().await;
			let _ = exit.wait_for(|exit| *exit).await;
		});
		Ok(())
	}

	fn session(&self, stop: &ShutdownManager<SessionShutdownReason>, resources: usize) -> FakeSession {
		FakeSession {
			_resources: (0..resources).map(|_| Resource::new(&self.counters)).collect(),
			stop: stop.clone(),
		}
	}
}

impl SessionBackend for FakeBackend {
	type Initialized = FakeSession;
	type Launched = FakeSession;
	type Active = FakeSession;

	async fn initialize(
		&self,
		_context: SessionContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<FakeSession, ()> {
		if self.counters.resources.load(Ordering::SeqCst) != 0 || self.counters.unit_created.load(Ordering::SeqCst) {
			self.counters.overlapped.store(true, Ordering::SeqCst);
		}
		*self.session_stop.lock().unwrap() = Some(stop.clone());
		// Like the gamepad thread: a worker that exists from initialization.
		self.spawn_worker(&stop, None)?;
		let session = self.session(&stop, 1);
		self.gate(Op::Initialize).await?;
		Ok(session)
	}

	async fn launch(&self, session: FakeSession) -> Result<FakeSession, ()> {
		// systemd accepted the transient unit; the start job is still running.
		self.counters.unit_created.store(true, Ordering::SeqCst);
		self.gate(Op::Launch).await?;
		if self.stop_on_launch_return.load(Ordering::SeqCst) {
			let _ = session.stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
		}
		let launched = self.session(&session.stop, 2);
		drop(session);
		Ok(launched)
	}

	async fn start(&self, session: FakeSession, request: StartRequest) -> Result<(FakeSession, Vec<StartLatch>), ()> {
		let latch = StartLatch::new();
		// Workers exist (and own resources) before the fallible step below.
		self.spawn_worker(&request.stop, Some(latch.clone()))?;
		self.spawn_worker(&request.stop, Some(latch.clone()))?;
		self.gate(Op::Start).await?;
		let active = self.session(&session.stop, 3);
		drop(session);
		Ok((active, vec![latch]))
	}

	fn pause(
		&self,
		_session: &FakeSession,
		_video: bool,
		_audio: bool,
	) -> impl Future<Output = Result<(), ()>> + Send + 'static {
		let gate = (
			self.gates[&Op::Pause].entered.clone(),
			self.gates[&Op::Pause].released.subscribe(),
			self.gates[&Op::Pause].hold.load(Ordering::SeqCst),
			self.gates[&Op::Pause].fail.load(Ordering::SeqCst),
		);
		let counters = self.counters.clone();
		async move {
			let (entered, mut released, hold, fail) = gate;
			counters.order.lock().unwrap().push(Op::Pause);
			entered.send_modify(|count| *count += 1);
			if hold {
				let _ = released.wait_for(|released| *released).await;
			}
			if fail { Err(()) } else { Ok(()) }
		}
	}

	async fn resume(&self, _session: &mut FakeSession, _plan: ResumePlan) -> Result<(), ()> {
		self.gate(Op::Resume).await
	}

	async fn stop_application(&self, _unit_name: &str) -> Result<(), ()> {
		self.gate(Op::StopApplication).await?;
		self.counters.unit_created.store(false, Ordering::SeqCst);
		self.counters.unit_stops.fetch_add(1, Ordering::SeqCst);
		Ok(())
	}
}

struct Harness {
	core: SessionCore<FakeBackend>,
	shutdown: ShutdownManager<ShutdownReason>,
	client: IpAddr,
}

impl Harness {
	fn new() -> Self {
		let shutdown = ShutdownManager::new();
		Self {
			core: SessionCore::new(FakeBackend::new(), shutdown.clone()).unwrap(),
			shutdown,
			client: "127.0.0.1".parse().unwrap(),
		}
	}

	fn backend(&self) -> &FakeBackend {
		&self.core.backend
	}

	fn counters(&self) -> &Counters {
		&self.backend().counters
	}

	fn hold(&self, op: Op) {
		self.backend().gates[&op].hold.store(true, Ordering::SeqCst);
	}

	fn fail(&self, op: Op) {
		self.backend().gates[&op].fail.store(true, Ordering::SeqCst);
	}

	fn release(&self, op: Op) {
		self.backend().gates[&op].released.send_replace(true);
	}

	fn calls(&self, op: Op) -> usize {
		*self.backend().gates[&op].entered.borrow()
	}

	/// Wait until `op` has been entered `count` times.
	async fn entered(&self, op: Op, count: usize) {
		let mut entered = self.backend().gates[&op].entered.subscribe();
		tokio::time::timeout(Duration::from_secs(5), entered.wait_for(|n| *n >= count))
			.await
			.unwrap_or_else(|_| panic!("{op:?} was not entered"))
			.unwrap();
	}

	async fn phase(&self) -> &'static str {
		match &self.core.inner.lock().await.lifecycle {
			Lifecycle::Idle => "idle",
			Lifecycle::Stopping(_) => "stopping",
			Lifecycle::Live(live) => match (&live.state, live.transition) {
				(_, Some(transition)) => match transition.kind {
					TransitionKind::Initialize => "initializing",
					TransitionKind::Launch => "launching",
					TransitionKind::Start => "starting",
					TransitionKind::Announce => "announcing",
					TransitionKind::Resume => "resuming",
				},
				(Some(state), None) => state.name(),
				(None, None) => panic!("state absent without a transition"),
			},
		}
	}

	async fn wait_idle(&self) {
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let waiter = match &self.core.inner.lock().await.lifecycle {
					Lifecycle::Idle => return,
					Lifecycle::Stopping(waiter) => Some(waiter.clone()),
					Lifecycle::Live(_) => None,
				};
				match waiter {
					Some(waiter) => {
						let _ = waiter.wait().await;
					},
					None => tokio::task::yield_now().await,
				}
			}
		})
		.await
		.expect("session did not become idle");
	}

	/// Idle and nothing owned: no resources, no workers, no application unit.
	async fn assert_released(&self) {
		assert_eq!(self.phase().await, "idle");
		assert_eq!(self.counters().resources.load(Ordering::SeqCst), 0, "resources leaked");
		assert!(
			!self.counters().unit_created.load(Ordering::SeqCst),
			"application unit leaked"
		);
		assert!(self.core.inner.lock().await.orphans.is_empty());
		assert!(self.core.get_session_context().await.is_none());
		assert!(self.core.authorize_stream(self.client).await.is_none());
	}

	async fn initialize(&self) -> Result<(), ()> {
		self.core.initialize_session(context()).await
	}

	async fn grant(&self) -> StreamAuthorization {
		self.core.authorize_stream(self.client).await.unwrap()
	}

	async fn announce(&self, grant: &StreamAuthorization) -> Result<(), ()> {
		self.core.set_stream_context(grant, video(), audio(), false).await
	}

	async fn launched(&self) {
		self.initialize().await.unwrap();
		self.core.launch_session().await.unwrap();
		assert_eq!(self.phase().await, "launched");
	}

	async fn active(&self) -> StreamAuthorization {
		self.launched().await;
		let grant = self.grant().await;
		self.announce(&grant).await.unwrap();
		self.core.start_session(&grant).await.unwrap();
		assert_eq!(self.phase().await, "active");
		grant
	}

	/// An active session after a client `/resume`, with ANNOUNCE done.
	async fn resumed(&self) -> StreamAuthorization {
		self.active().await;
		self.core
			.resume_session(keys(), ResumeRequest::default(), self.client)
			.await
			.unwrap();
		let grant = self.grant().await;
		self.announce(&grant).await.unwrap();
		grant
	}
}

fn keys() -> SessionKeyData {
	SessionKeyData::new(RemoteInputKey::from_bytes([7; 16]), RemoteInputKeyId::new(1))
}

fn context() -> SessionContext {
	SessionContext {
		application: Default::default(),
		application_id: 1,
		resolution: (1920, 1080),
		refresh_rate: 60,
		keys: SessionKeys::Keys(keys()),
		audio_channels: AudioChannels::Stereo,
		audio_channel_mask: 0x3,
		hdr: false,
		client_ip: "127.0.0.1".parse().unwrap(),
	}
}

fn video() -> VideoStreamContext {
	VideoStreamContext {
		pyrowave_dialect: None,
		width: 1920,
		height: 1080,
		fps: 60,
		packet_size: 1392,
		bitrate: 20_000_000,
		minimum_fec_packets: 2,
		qos: true,
		format: NegotiatedVideoFormat::sdr(
			VideoCodec::H264,
			ChromaFormat::Yuv420,
			BitDepth::Eight,
			ColorRange::Limited,
		),
		max_reference_frames: 1,
		encrypt_video: false,
	}
}

fn audio() -> AudioStreamContext {
	AudioStreamContext {
		packet_duration_ms: 5,
		qos: true,
		audio_config: AudioConfig::from_channels(AudioChannels::Stereo, 0x3, true),
		encrypt_audio: false,
	}
}

/// Drive a session to the point where `op` is in flight, holding it there.
async fn hold_at(h: &Harness, op: Op) -> tokio::task::JoinHandle<Result<(), ()>> {
	h.hold(op);
	let core = h.core.clone();
	let task = match op {
		Op::Initialize => tokio::spawn(async move { core.initialize_session(context()).await }),
		Op::Launch => {
			h.initialize().await.unwrap();
			tokio::spawn(async move { core.launch_session().await })
		},
		Op::Start => {
			h.launched().await;
			let grant = h.grant().await;
			h.announce(&grant).await.unwrap();
			tokio::spawn(async move { core.start_session(&grant).await })
		},
		Op::Pause => {
			h.active().await;
			let grant = h.grant().await;
			tokio::spawn(async move { core.set_stream_context(&grant, video(), audio(), false).await })
		},
		Op::Resume => {
			let grant = h.resumed().await;
			tokio::spawn(async move { core.start_session(&grant).await })
		},
		Op::StopApplication => unreachable!("not a transition"),
	};
	h.entered(op, 1).await;
	task
}

const TRANSITIONS: [Op; 5] = [Op::Initialize, Op::Launch, Op::Start, Op::Pause, Op::Resume];

/// Full happy path, then a stop that releases everything, twice in a row.
#[tokio::test]
async fn full_lifecycle_releases_everything_and_relaunches_immediately() {
	let h = Harness::new();
	for round in 1..=2 {
		h.active().await;
		h.core.trigger_streams_start().await;
		h.core.stop_session().await.unwrap();
		h.assert_released().await;
		assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), round);
	}
	assert!(!h.counters().overlapped.load(Ordering::SeqCst));
}

/// STAB-002: stop cancels a transition at every await; the in-flight request
/// fails, teardown completes and a new session can follow immediately.
#[tokio::test]
async fn stop_cancels_every_transition_await() {
	for op in TRANSITIONS {
		let h = Harness::new();
		let task = hold_at(&h, op).await;
		assert_ne!(h.phase().await, "idle", "{op:?}: a transition must not look idle");
		h.core.stop_session().await.unwrap();
		assert!(task.await.unwrap().is_err(), "{op:?}: cancelled request must fail");
		h.assert_released().await;
		// Immediate relaunch with the barrier lifted.
		h.release(op);
		h.backend().gates[&op].hold.store(false, Ordering::SeqCst);
		h.active().await;
		h.core.stop_session().await.unwrap();
		h.assert_released().await;
		assert!(!h.counters().overlapped.load(Ordering::SeqCst), "{op:?}");
	}
}

/// STAB-002: a failure at each constructor, after earlier resources exist,
/// tears the whole session down instead of leaving partial state.
#[tokio::test]
async fn failure_of_each_transition_tears_down_deterministically() {
	for op in [Op::Initialize, Op::Launch, Op::Start, Op::Resume] {
		let h = Harness::new();
		h.fail(op);
		let result = match op {
			Op::Initialize => h.initialize().await,
			Op::Launch => {
				h.initialize().await.unwrap();
				h.core.launch_session().await
			},
			Op::Start => {
				h.launched().await;
				let grant = h.grant().await;
				h.announce(&grant).await.unwrap();
				h.core.start_session(&grant).await
			},
			Op::Resume => {
				let grant = h.resumed().await;
				h.core.start_session(&grant).await
			},
			_ => unreachable!(),
		};
		assert!(result.is_err(), "{op:?}");
		h.wait_idle().await;
		h.assert_released().await;
		let unit_recorded = op != Op::Initialize;
		assert_eq!(
			h.counters().unit_stops.load(Ordering::SeqCst) > 0,
			unit_recorded,
			"{op:?}"
		);
	}
}

/// STAB-002: the HTTP launch timeout drops the caller's future after systemd
/// created the unit. The manager still owns the launch; the follow-up stop
/// cancels it and stops the unit.
#[tokio::test]
async fn timeout_after_transient_unit_creation_stops_the_unit() {
	let h = Harness::new();
	h.initialize().await.unwrap();
	h.hold(Op::Launch);
	assert!(
		tokio::time::timeout(Duration::from_millis(10), h.core.launch_session())
			.await
			.is_err()
	);
	assert!(h.counters().unit_created.load(Ordering::SeqCst));
	assert_eq!(h.phase().await, "launching", "launch survives its caller");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
}

/// STAB-002: an abandoned caller does not abandon the launch; it commits.
#[tokio::test]
async fn abandoned_launch_caller_still_commits() {
	let h = Harness::new();
	h.initialize().await.unwrap();
	h.hold(Op::Launch);
	let _ = tokio::time::timeout(Duration::from_millis(10), h.core.launch_session()).await;
	h.release(Op::Launch);
	tokio::time::timeout(Duration::from_secs(5), async {
		while h.phase().await != "launched" {
			tokio::task::yield_now().await;
		}
	})
	.await
	.unwrap();
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-002: duplicate PLAY and PLAY before ANNOUNCE are rejected without
/// touching the retained application or the live streams.
#[tokio::test]
async fn duplicate_and_premature_play_keep_the_application() {
	let h = Harness::new();
	h.launched().await;
	let grant = h.grant().await;
	assert!(h.core.start_session(&grant).await.is_err(), "PLAY before ANNOUNCE");
	assert_eq!(h.phase().await, "launched");
	h.announce(&grant).await.unwrap();
	h.core.start_session(&grant).await.unwrap();
	let resources = h.counters().resources.load(Ordering::SeqCst);
	assert!(h.core.start_session(&grant).await.is_err(), "duplicate PLAY");
	assert_eq!(h.phase().await, "active");
	assert_eq!(h.counters().resources.load(Ordering::SeqCst), resources);
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 0);
	assert!(h.counters().unit_created.load(Ordering::SeqCst));
	// A duplicate launch is equally harmless.
	assert!(h.core.launch_session().await.is_err());
	assert_eq!(h.phase().await, "active");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-002: PLAY racing an ANNOUNCE whose pause is still in flight cannot
/// commit; the contexts are published only after the pause barrier, so the
/// resumed epoch is never paused after it started.
#[tokio::test]
async fn concurrent_announce_and_play_are_ordered() {
	let h = Harness::new();
	h.active().await;
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	let grant = h.grant().await;
	h.hold(Op::Pause);
	let announce = {
		let core = h.core.clone();
		let grant = grant.clone();
		tokio::spawn(async move { core.set_stream_context(&grant, video(), audio(), false).await })
	};
	h.entered(Op::Pause, 1).await;
	assert!(
		h.core.start_session(&grant).await.is_err(),
		"PLAY during ANNOUNCE pause"
	);
	assert_eq!(h.calls(Op::Resume), 0);
	h.release(Op::Pause);
	announce.await.unwrap().unwrap();
	h.core.start_session(&grant).await.unwrap();
	let order = h.counters().order.lock().unwrap().clone();
	let pause = order.iter().rposition(|op| *op == Op::Pause).unwrap();
	let resume = order.iter().rposition(|op| *op == Op::Resume).unwrap();
	assert!(pause < resume);
	assert_eq!(h.phase().await, "active");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Stale generations: contexts announced before a newer `/resume` cannot be
/// committed, and the live session is unaffected.
#[tokio::test]
async fn stale_generation_cannot_commit() {
	let h = Harness::new();
	h.active().await;
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	let first = h.grant().await;
	h.announce(&first).await.unwrap();
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	assert!(h.core.start_session(&first).await.is_err());
	assert!(h.announce(&first).await.is_err());
	assert_eq!(h.calls(Op::Resume), 0);
	assert_eq!(h.phase().await, "active");
	let second = h.grant().await;
	h.announce(&second).await.unwrap();
	h.core.start_session(&second).await.unwrap();
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Stale completion: a launch that finishes as its session stops is handed to
/// that session's teardown instead of being committed.
#[tokio::test]
async fn completion_after_stop_is_released_by_teardown() {
	let h = Harness::new();
	h.initialize().await.unwrap();
	h.backend().stop_on_launch_return.store(true, Ordering::SeqCst);
	assert!(h.core.launch_session().await.is_err());
	h.wait_idle().await;
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
}

/// STAB-003: stop completes only after delayed workers exit; meanwhile the
/// session is not idle and a replacement waits instead of overlapping.
#[tokio::test]
async fn teardown_waits_for_delayed_workers_before_replacement() {
	let h = Harness::new();
	h.active().await;
	h.backend().worker_exit.send_replace(false);
	let stop = {
		let core = h.core.clone();
		tokio::spawn(async move { core.stop_session().await })
	};
	h.entered(Op::StopApplication, 1).await;
	tokio::task::yield_now().await;
	assert_eq!(h.phase().await, "stopping");
	assert!(h.core.get_session_context().await.is_none());
	assert!(h.core.authorize_stream(h.client).await.is_none());
	assert!(
		h.counters().resources.load(Ordering::SeqCst) > 0,
		"workers still own resources"
	);
	let initialized = h.calls(Op::Initialize);
	let replacement = {
		let core = h.core.clone();
		tokio::spawn(async move { core.initialize_session(context()).await })
	};
	for _ in 0..10 {
		tokio::task::yield_now().await;
	}
	assert!(!stop.is_finished() && !replacement.is_finished());
	assert_eq!(
		h.calls(Op::Initialize),
		initialized,
		"replacement must not start before teardown"
	);
	h.backend().worker_exit.send_replace(true);
	stop.await.unwrap().unwrap();
	replacement.await.unwrap().unwrap();
	assert!(!h.counters().overlapped.load(Ordering::SeqCst));
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-003: a worker failure stops the session through the watchdog, which
/// hands over to teardown without cancelling it.
#[tokio::test]
async fn watchdog_triggered_cleanup_completes() {
	let h = Harness::new();
	h.active().await;
	let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
	stop.trigger_shutdown(SessionShutdownReason::VideoEncoderStopped)
		.unwrap();
	h.wait_idle().await;
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
	// The next session starts normally.
	h.active().await;
	h.core.stop_session().await.unwrap();
}

/// STAB-003: the application exiting while the user cancels produces one
/// teardown, one unit stop and a successful cancel.
#[tokio::test]
async fn application_exit_during_cancel_has_one_owner() {
	let h = Harness::new();
	h.active().await;
	h.hold(Op::StopApplication);
	let cancel = {
		let core = h.core.clone();
		tokio::spawn(async move { core.stop_session().await })
	};
	h.entered(Op::StopApplication, 1).await;
	let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
	let _ = stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
	let second_cancel = {
		let core = h.core.clone();
		tokio::spawn(async move { core.stop_session().await })
	};
	h.release(Op::StopApplication);
	cancel.await.unwrap().unwrap();
	second_cancel.await.unwrap().unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
}

/// STAB-001 at the manager: stop before StartB releases workers; StartB
/// before workers poll and duplicate StartB start each worker once.
#[tokio::test]
async fn start_signal_contract_through_the_manager() {
	let h = Harness::new();
	h.active().await;
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().workers_started.load(Ordering::SeqCst), 0);

	h.active().await;
	h.core.trigger_streams_start().await;
	h.core.trigger_streams_start().await;
	tokio::time::timeout(Duration::from_secs(5), async {
		while h.counters().workers_started.load(Ordering::SeqCst) < 2 {
			tokio::task::yield_now().await;
		}
	})
	.await
	.unwrap();
	for _ in 0..10 {
		tokio::task::yield_now().await;
	}
	assert_eq!(h.counters().workers_started.load(Ordering::SeqCst), 2);
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-003: global shutdown completes only after the session's application
/// and workers have been released.
#[tokio::test]
async fn global_shutdown_includes_session_teardown() {
	let h = Harness::new();
	h.active().await;
	h.backend().worker_exit.send_replace(false);
	h.shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
	h.entered(Op::StopApplication, 1).await;
	assert!(
		tokio::time::timeout(Duration::from_millis(20), h.shutdown.wait_shutdown_complete())
			.await
			.is_err()
	);
	h.backend().worker_exit.send_replace(true);
	tokio::time::timeout(Duration::from_secs(5), h.shutdown.wait_shutdown_complete())
		.await
		.unwrap();
	h.assert_released().await;
	assert!(h.initialize().await.is_err(), "no new sessions during service shutdown");
}

/// STAB-003: a worker that never exits exceeds the documented deadline. The
/// session is never reported idle, replacement is refused and the service is
/// asked to stop.
#[tokio::test(start_paused = true)]
async fn teardown_deadline_is_a_terminal_failure() {
	let h = Harness::new();
	h.active().await;
	h.backend().worker_exit.send_replace(false);
	assert!(h.core.stop_session().await.is_err());
	assert_eq!(h.phase().await, "stopping");
	assert!(h.shutdown.is_shutdown_triggered());
	assert!(h.initialize().await.is_err());
	h.backend().worker_exit.send_replace(true);
}

/// STAB-003: the application stop is bounded and does not wedge teardown.
#[tokio::test(start_paused = true)]
async fn hung_application_stop_is_bounded() {
	let h = Harness::new();
	h.active().await;
	h.hold(Op::StopApplication);
	let started = tokio::time::Instant::now();
	h.core.stop_session().await.unwrap();
	assert!(started.elapsed() <= SESSION_TEARDOWN_DEADLINE);
	assert_eq!(h.phase().await, "idle");
	assert_eq!(h.counters().resources.load(Ordering::SeqCst), 0);
}
