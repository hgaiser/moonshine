use std::sync::Arc;
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use strum_macros::FromRepr;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use self::gamepad::{Gamepad, GamepadConfig, GamepadEmulation, VirtualGamepad, VirtualIdentity};
use self::ownership::FeedbackRoute;
use self::remap::{HoldToHome, HoldTransition};
use self::touch::{Pen, PointerEventKind, Touch};
use crate::session::compositor::input::CompositorInputEvent;
use crate::session::manager::SessionShutdownReason;

use self::{
	gamepad::{GamepadBattery, GamepadInfo, GamepadMotion, GamepadTouch, GamepadUpdate},
	keyboard::Key,
	mouse::{MouseButton, MouseMoveAbsolute, MouseMoveRelative, MouseScrollHorizontal, MouseScrollVertical},
};

use crate::session::stream::control::FeedbackCommand;

pub(crate) mod gamepad;
mod keyboard;
mod mouse;
mod ownership;
mod remap;
mod touch;

#[derive(FromRepr)]
#[repr(u32)]
enum InputEventType {
	KeyDown = 0x00000003,
	KeyUp = 0x00000004,
	MouseMoveAbsolute = 0x00000005,
	MouseMoveRelative = 0x00000007,
	MouseButtonDown = 0x00000008,
	MouseButtonUp = 0x00000009,
	MouseScrollVertical = 0x0000000A,
	MouseScrollHorizontal = 0x55000001,
	Touch = 0x55000002,
	Pen = 0x55000003,
	GamepadInfo = 0x55000004, // Called ControllerArrival in Moonlight.
	GamepadTouch = 0x55000005,
	GamepadMotion = 0x55000006,
	GamepadBattery = 0x55000007,
	GamepadUpdate = 0x0000000C,
	GamepadEnableHaptics = 0x0000000D,
	Utf8Text = 0x00000017,
}

#[derive(Debug)]
#[repr(u32)]
enum InputEvent {
	KeyDown(Key),
	KeyUp(Key),
	MouseMoveAbsolute(MouseMoveAbsolute),
	MouseMoveRelative(MouseMoveRelative),
	MouseButtonDown(MouseButton),
	MouseButtonUp(MouseButton),
	MouseScrollVertical(MouseScrollVertical),
	MouseScrollHorizontal(MouseScrollHorizontal),
	Touch(Touch),
	Pen(Pen),
	GamepadInfo(GamepadInfo),
	GamepadTouch(GamepadTouch),
	GamepadMotion(GamepadMotion),
	GamepadBattery(GamepadBattery),
	GamepadUpdate(GamepadUpdate),
	GamepadEnableHaptics,
	Utf8Text(String),
}

impl InputEvent {
	fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			tracing::warn!(
				"Expected control message to have at least 4 bytes, got {}",
				buffer.len()
			);
			return Err(());
		}

		let event_type = u32::from_le_bytes(buffer[..4].try_into().unwrap());
		match InputEventType::from_repr(event_type) {
			Some(InputEventType::KeyDown) => Ok(InputEvent::KeyDown(Key::from_bytes(&buffer[4..])?)),
			Some(InputEventType::KeyUp) => Ok(InputEvent::KeyUp(Key::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseMoveAbsolute) => Ok(InputEvent::MouseMoveAbsolute(
				MouseMoveAbsolute::from_bytes(&buffer[4..])?,
			)),
			Some(InputEventType::MouseMoveRelative) => Ok(InputEvent::MouseMoveRelative(
				MouseMoveRelative::from_bytes(&buffer[4..])?,
			)),
			Some(InputEventType::MouseButtonDown) => {
				Ok(InputEvent::MouseButtonDown(MouseButton::from_bytes(&buffer[4..])?))
			},
			Some(InputEventType::MouseButtonUp) => {
				Ok(InputEvent::MouseButtonUp(MouseButton::from_bytes(&buffer[4..])?))
			},
			Some(InputEventType::MouseScrollVertical) => Ok(InputEvent::MouseScrollVertical(
				MouseScrollVertical::from_bytes(&buffer[4..])?,
			)),
			Some(InputEventType::MouseScrollHorizontal) => Ok(InputEvent::MouseScrollHorizontal(
				MouseScrollHorizontal::from_bytes(&buffer[4..])?,
			)),
			Some(InputEventType::Touch) => Ok(InputEvent::Touch(Touch::from_bytes(&buffer[4..])?)),
			Some(InputEventType::Pen) => Ok(InputEvent::Pen(Pen::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadInfo) => Ok(InputEvent::GamepadInfo(GamepadInfo::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadTouch) => Ok(InputEvent::GamepadTouch(GamepadTouch::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadMotion) => {
				Ok(InputEvent::GamepadMotion(GamepadMotion::from_bytes(&buffer[4..])?))
			},
			Some(InputEventType::GamepadBattery) => {
				Ok(InputEvent::GamepadBattery(GamepadBattery::from_bytes(&buffer[4..])?))
			},
			Some(InputEventType::GamepadUpdate) => {
				Ok(InputEvent::GamepadUpdate(GamepadUpdate::from_bytes(&buffer[4..])?))
			},
			Some(InputEventType::GamepadEnableHaptics) => Ok(InputEvent::GamepadEnableHaptics),
			Some(InputEventType::Utf8Text) => {
				let text = String::from_utf8(buffer[4..].to_vec())
					.map_err(|e| tracing::warn!("Invalid UTF-8 in text event: {e}"))?;
				if text.is_empty() {
					tracing::warn!("Received empty text event");
					return Err(());
				}
				Ok(InputEvent::Utf8Text(text))
			},
			None => {
				tracing::warn!("Received unknown event type: {event_type}");
				Err(())
			},
		}
	}
}

/// Decode an input event without dispatching it (parser regression tests).
#[cfg(test)]
pub(super) fn parse_input_event(buffer: &[u8]) -> Result<(), ()> {
	InputEvent::from_bytes(buffer).map(|_| ())
}

enum GamepadCommand {
	/// Input from the active peer, with that peer's feedback channel.
	Input(InputEvent, mpsc::Sender<FeedbackCommand>),
	/// The controlling peer lost ownership: neutralize and unbind every
	/// controller, but keep the devices (see `run_gamepad_handler`).
	RevokeOwner(tokio::sync::oneshot::Sender<()>),
}

pub(crate) struct InputHandler {
	keys: std::collections::BTreeSet<u32>,
	buttons: std::collections::BTreeSet<u32>,
	input_tx: calloop::channel::Sender<CompositorInputEvent>,
	gamepad_tx: mpsc::Sender<GamepadCommand>,
}

impl InputHandler {
	pub fn new(
		input_tx: calloop::channel::Sender<CompositorInputEvent>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
		gamepad_config: GamepadConfig,
	) -> Result<Self, ()> {
		Self::with_gamepads(input_tx, stop_session_manager, gamepad_config, Gamepad::new)
	}

	fn with_gamepads<D, F>(
		input_tx: calloop::channel::Sender<CompositorInputEvent>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
		gamepad_config: GamepadConfig,
		create_gamepad: F,
	) -> Result<Self, ()>
	where
		D: VirtualGamepad + 'static,
		F: Fn(&GamepadInfo, FeedbackRoute, GamepadEmulation) -> Result<D, ()> + Send + 'static,
	{
		let (gamepad_tx, gamepad_rx) = mpsc::channel(10);

		// Registered before the thread exists so virtual devices are always
		// destroyed before the session reports completion.
		let worker = crate::session::lifecycle::WorkerGuard::register(
			&stop_session_manager,
			SessionShutdownReason::InputHandlerStopped,
		)?;
		std::thread::Builder::new()
			.name("gamepad-input".to_string())
			.spawn(move || {
				let _worker = worker;
				let rt = tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.build()
					.expect("Failed to create tokio runtime for input handler");

				rt.block_on(async move {
					let local = tokio::task::LocalSet::new();
					local
						.run_until(async move {
							run_gamepad_handler(gamepad_rx, stop_session_manager, gamepad_config, create_gamepad).await;
						})
						.await;
				});
			})
			.map_err(|e| tracing::error!("Failed to spawn gamepad input thread: {e}"))?;

		Ok(Self {
			input_tx,
			gamepad_tx,
			keys: Default::default(),
			buttons: Default::default(),
		})
	}

	async fn handle_input(&mut self, event: InputEvent, feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		match event {
			InputEvent::KeyDown(key) => {
				if let Some(keycode) = key.to_linux_keycode() {
					self.keys.insert(keycode);
					tracing::trace!("Pressing key: {key:?} (keycode: {keycode})");
					let _ = self.input_tx.send(CompositorInputEvent::KeyDown { keycode });
				}
			},
			InputEvent::KeyUp(key) => {
				if let Some(keycode) = key.to_linux_keycode() {
					self.keys.remove(&keycode);
					tracing::trace!("Releasing key: {key:?} (keycode: {keycode})");
					let _ = self.input_tx.send(CompositorInputEvent::KeyUp { keycode });
				}
			},
			InputEvent::MouseMoveAbsolute(event) => {
				tracing::trace!("Absolute mouse movement: {event:?}");
				let _ = self.input_tx.send(CompositorInputEvent::MouseMoveAbsolute {
					x: event.x,
					y: event.y,
					screen_width: event.screen_width,
					screen_height: event.screen_height,
				});
			},
			InputEvent::MouseMoveRelative(event) => {
				tracing::trace!("Moving mouse relative: {event:?}");
				let _ = self.input_tx.send(CompositorInputEvent::MouseMoveRelative {
					dx: event.x,
					dy: event.y,
				});
			},
			InputEvent::MouseButtonDown(button) => {
				tracing::trace!("Pressing mouse button: {button:?}");
				let button_code: u32 = button.into();
				self.buttons.insert(button_code);
				let _ = self
					.input_tx
					.send(CompositorInputEvent::MouseButtonDown { button: button_code });
			},
			InputEvent::MouseButtonUp(button) => {
				tracing::trace!("Releasing mouse button: {button:?}");
				let button_code: u32 = button.into();
				self.buttons.remove(&button_code);
				let _ = self
					.input_tx
					.send(CompositorInputEvent::MouseButtonUp { button: button_code });
			},
			InputEvent::MouseScrollVertical(event) => {
				tracing::trace!("Scrolling vertically: {event:?}");
				let _ = self
					.input_tx
					.send(CompositorInputEvent::ScrollVertical { amount: event.amount });
			},
			InputEvent::MouseScrollHorizontal(event) => {
				tracing::trace!("Scrolling horizontally: {event:?}");
				let _ = self
					.input_tx
					.send(CompositorInputEvent::ScrollHorizontal { amount: event.amount });
			},
			InputEvent::Touch(event) => {
				tracing::trace!("Touch input: {event:?}");
				let compositor_event = match event.event_kind {
					PointerEventKind::Down => Some(CompositorInputEvent::TouchDown {
						slot: event.pointer_id,
						x: event.x,
						y: event.y,
					}),
					PointerEventKind::Move => Some(CompositorInputEvent::TouchMove {
						slot: event.pointer_id,
						x: event.x,
						y: event.y,
					}),
					PointerEventKind::Up | PointerEventKind::Cancel => {
						Some(CompositorInputEvent::TouchUp { slot: event.pointer_id })
					},
					PointerEventKind::CancelAll => Some(CompositorInputEvent::TouchCancelAll),
					PointerEventKind::Hover | PointerEventKind::ButtonOnly | PointerEventKind::HoverLeave => None,
				};
				if let Some(event) = compositor_event {
					let _ = self.input_tx.send(event);
				}
			},
			InputEvent::Pen(event) => {
				tracing::trace!("Pen input: {event:?}");
				let _ = self.input_tx.send(CompositorInputEvent::Pen {
					event_kind: event.event_kind as u8,
					tool_kind: event.tool_kind as u8,
					buttons: event.buttons,
					x: event.x,
					y: event.y,
					pressure_or_distance: event.pressure_or_distance,
					rotation: event.rotation,
					tilt: event.tilt,
				});
			},
			InputEvent::Utf8Text(text) => {
				tracing::debug!("Typing clipboard text.");
				let _ = self.input_tx.send(CompositorInputEvent::TypeText { text });
			},
			// Gamepad events: forward to gamepad handler thread.
			gamepad_event => {
				self.gamepad_tx
					.send(GamepadCommand::Input(gamepad_event, feedback))
					.await
					.map_err(|e| tracing::warn!("Failed to send gamepad event: {e}"))?;
			},
		}
		Ok(())
	}

	/// Called only after ControlPeers revokes the active owner. Both queues are
	/// ordered; completion means no key, button, contact, text, controller
	/// state, Home timer or feedback route of the old owner survives. Virtual
	/// controllers themselves stay plugged in for the next owner.
	pub async fn reset(&mut self) -> Result<(), ()> {
		let (ready, waiting) = tokio::sync::oneshot::channel();
		self.input_tx
			.send(CompositorInputEvent::Reset {
				keys: std::mem::take(&mut self.keys).into_iter().collect(),
				buttons: std::mem::take(&mut self.buttons).into_iter().collect(),
				ready,
			})
			.map_err(|_| ())?;
		let (done, gamepads_done) = tokio::sync::oneshot::channel();
		self.gamepad_tx
			.send(GamepadCommand::RevokeOwner(done))
			.await
			.map_err(|_| ())?;
		gamepads_done.await.map_err(|_| ())?;
		waiting.await.map_err(|_| ())
	}

	pub async fn handle_raw_input(&mut self, event: &[u8], feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		let event = InputEvent::from_bytes(event)?;
		self.handle_input(event, feedback).await
	}
}

/// Per-gamepad slot holding the virtual device, remap state, feedback route
/// and rumble tracking.
///
/// A slot's device lives as long as its identity: it is destroyed only when
/// the client reports the controller gone (active mask), an arrival needs a
/// different virtual identity, or the session ends. Ownership changes only
/// neutralize the device and rebind its feedback route.
struct GamepadSlot<D: VirtualGamepad> {
	/// Arrival metadata of the current owner's controller.
	info: GamepadInfo,
	/// Native identity of `device`; arrivals that keep it reuse the device.
	identity: VirtualIdentity,
	/// The underlying virtual device.
	device: D,

	/// Hold-to-Home button remap state machine.
	remap: HoldToHome,

	/// Current owner's feedback destination, shared with native callbacks.
	route: FeedbackRoute,

	/// Gamepad index assigned by the client (0-15).
	index: u8,

	/// Wakes the timer task when a new hold-to-Home deadline is set.
	timer_wake: Arc<Notify>,

	/// Rumble intensity for the hold-to-Home activation pulse (0.0-1.0).
	home_rumble_intensity: f64,

	/// Duration of the hold-to-Home activation rumble pulse.
	home_rumble_duration: Duration,

	/// Instant at which the hold-to-Home rumble pulse should be turned off,
	/// or `None` if no pulse is active.
	home_rumble_off_at: Option<Instant>,
}

impl<D: VirtualGamepad> GamepadSlot<D> {
	fn new(
		info: &GamepadInfo,
		config: &GamepadConfig,
		timer_wake: Arc<Notify>,
		create_gamepad: &impl Fn(&GamepadInfo, FeedbackRoute, GamepadEmulation) -> Result<D, ()>,
	) -> Result<Self, ()> {
		let identity = info.virtual_identity(config.emulation);
		let route = FeedbackRoute::new(info.index, identity.has_motion());
		let device = create_gamepad(info, route.clone(), config.emulation)?;
		Ok(Self {
			info: *info,
			identity,
			device,
			remap: HoldToHome::new(config),
			route,
			index: info.index,
			timer_wake,
			home_rumble_intensity: config.home_button.rumble_intensity,
			home_rumble_duration: Duration::from_millis(config.home_button.rumble_duration_ms),
			home_rumble_off_at: None,
		})
	}

	/// Bind the device to the peer whose input is being applied. Returns the
	/// order-independent feedback (motion enables) to send to that owner.
	fn claim(&mut self, owner: &mpsc::Sender<FeedbackCommand>) -> Option<Vec<FeedbackCommand>> {
		let enables = self.route.claim(owner)?;
		tracing::debug!(index = self.index, "Gamepad bound to the controlling peer");
		Some(enables)
	}

	/// Ownership was lost: stop the shortcut pulse at the old owner, cancel
	/// any pending Home/Guide transition, return every input to rest and
	/// unbind feedback. The device stays connected.
	fn revoke_owner(&mut self) {
		if self.home_rumble_off_at.take().is_some() {
			self.send_rumble(0, 0);
		}
		self.remap.cancel();
		self.device.neutralize();
		self.route.revoke();
	}

	/// Apply button flags through the remap layer. Returns any transition.
	fn apply_buttons(&mut self, flags: u32, now: Instant) -> HoldTransition {
		let (remapped, transition) = self.remap.apply(flags, now);
		self.device.set_pressed(remapped);
		self.check_rumble(now);
		// Wake the timer task so it can pick up any new deadline.
		self.timer_wake.notify_one();
		self.maybe_fire_rumble(transition, now)
	}

	/// Advance the remap state machine using internally tracked button state.
	fn advance(&mut self, now: Instant) -> HoldTransition {
		if !self.route.is_owned() {
			// Defensive: revocation already cancelled the timers.
			self.remap.cancel();
			self.home_rumble_off_at = None;
			self.device.neutralize();
			return HoldTransition::None;
		}
		self.check_rumble(now);
		let (remapped, transition) = self.remap.advance(now);
		self.device.set_pressed(remapped);
		self.maybe_fire_rumble(transition, now)
	}

	/// The next time at which `advance()` should be called, or `None`.
	/// Includes both remap deadlines and the rumble turn-off deadline.
	fn next_deadline(&self) -> Option<Instant> {
		let remap = self.remap.next_deadline();
		let rumble = self.home_rumble_off_at;
		match (remap, rumble) {
			(Some(a), Some(b)) => Some(a.min(b)),
			(Some(a), None) => Some(a),
			(None, Some(b)) => Some(b),
			(None, None) => None,
		}
	}

	fn check_rumble(&mut self, now: Instant) {
		if let Some(off_at) = self.home_rumble_off_at
			&& now >= off_at
		{
			self.send_rumble(0, 0);
			self.home_rumble_off_at = None;
		}
	}

	fn maybe_fire_rumble(&mut self, transition: HoldTransition, now: Instant) -> HoldTransition {
		if transition == HoldTransition::HomeActivated
			&& self.home_rumble_off_at.is_none()
			&& self.home_rumble_intensity > 0.0
			&& !self.home_rumble_duration.is_zero()
		{
			let intensity = (self.home_rumble_intensity * u16::MAX as f64) as u16;
			self.send_rumble(intensity, intensity);
			self.home_rumble_off_at = Some(now + self.home_rumble_duration);
		}
		transition
	}

	fn send_rumble(&self, low_frequency: u16, high_frequency: u16) {
		self.route.try_deliver(FeedbackCommand::Rumble(
			crate::session::stream::control::feedback::RumbleCommand {
				id: self.index as u16,
				low_frequency,
				high_frequency,
			},
		));
	}
}

impl<D: VirtualGamepad> Drop for GamepadSlot<D> {
	fn drop(&mut self) {
		// Release input and the shortcut pulse before destroying/reusing a slot.
		self.device.neutralize();
		if self.home_rumble_off_at.is_some() {
			self.send_rumble(0, 0);
		}
	}
}

type Slots<D> = Arc<Mutex<[Option<GamepadSlot<D>>; 16]>>;

/// Ensure slot `idx` holds a device for `info`, reusing a compatible one.
///
/// Returns `false` if a required device could not be created.
fn ensure_slot<D: VirtualGamepad>(
	slots: &mut [Option<GamepadSlot<D>>; 16],
	info: &GamepadInfo,
	config: &GamepadConfig,
	timer_wake: &Arc<Notify>,
	create_gamepad: &impl Fn(&GamepadInfo, FeedbackRoute, GamepadEmulation) -> Result<D, ()>,
) -> bool {
	let idx = info.index as usize;
	if let Some(slot) = slots[idx].as_mut()
		&& slot.identity == info.virtual_identity(config.emulation)
	{
		// Same native device: keep it plugged in for the running game.
		slot.info = *info;
		return true;
	}
	// Destroy the old UHID device before reusing its MAC. This also handles
	// late arrivals after compatibility creation and a changed subtype.
	slots[idx] = None;
	match GamepadSlot::new(info, config, timer_wake.clone(), create_gamepad) {
		Ok(slot) => {
			slots[idx] = Some(slot);
			tracing::info!("Gamepad {} connected.", info.index);
			true
		},
		Err(()) => false,
	}
}

/// Claim slot `idx` for `owner` and send it any motion enables.
async fn claim_slot<D: VirtualGamepad>(slots: &Slots<D>, idx: usize, owner: &mpsc::Sender<FeedbackCommand>) {
	let enables = slots.lock().await[idx].as_mut().and_then(|slot| slot.claim(owner));
	for command in enables.into_iter().flatten() {
		let _ = owner.send(command).await;
	}
}

async fn run_gamepad_handler<D, F>(
	mut command_rx: mpsc::Receiver<GamepadCommand>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	gamepad_config: GamepadConfig,
	create_gamepad: F,
) where
	D: VirtualGamepad + 'static,
	F: Fn(&GamepadInfo, FeedbackRoute, GamepadEmulation) -> Result<D, ()>,
{
	let gamepads: Slots<D> = Arc::new(Mutex::new([const { None }; 16]));
	let timer_wake = Arc::new(Notify::new());

	// Spawn a timer task that advances gamepads with pending deadlines.
	let gamepads_timer = gamepads.clone();
	let timer_wake_for_timer = timer_wake.clone();
	let timer_task = tokio::task::spawn_local(run_timer_task(gamepads_timer, timer_wake_for_timer));

	while let Ok(Some(command)) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
		let (command, feedback_tx) = match command {
			GamepadCommand::Input(event, feedback) => (event, feedback),
			GamepadCommand::RevokeOwner(done) => {
				// The timer uses this same mutex, so after acknowledgment it can
				// never advance a previous owner's state, and no native callback
				// can reach the old (or any) peer until a new owner claims a slot.
				// Devices stay connected: a retained-session reconnect must not
				// look like an unplug to the running game.
				for slot in gamepads.lock().await.iter_mut().flatten() {
					slot.revoke_owner();
				}
				timer_wake.notify_one();
				let _ = done.send(());
				continue;
			},
		};
		if feedback_tx.is_closed() {
			continue;
		}
		match command {
			InputEvent::GamepadInfo(gamepad) => {
				tracing::debug!("Gamepad info: {gamepad:?}");
				let idx = gamepad.index as usize;
				if idx >= 16 {
					tracing::warn!(
						"Received info for gamepad {}, but we only have 16 slots.",
						gamepad.index
					);
					continue;
				}

				let created = ensure_slot(
					&mut *gamepads.lock().await,
					&gamepad,
					&gamepad_config,
					&timer_wake,
					&create_gamepad,
				);
				if created {
					claim_slot(&gamepads, idx, &feedback_tx).await;
				}
			},
			InputEvent::GamepadTouch(gamepad_touch) => {
				tracing::trace!("Gamepad touch: {gamepad_touch:?}");
				let idx = gamepad_touch.index as usize;
				if idx >= 16 {
					tracing::warn!(
						"Received touch for gamepad {}, but we only have 16 gamepads.",
						gamepad_touch.index
					);
					continue;
				}
				claim_slot(&gamepads, idx, &feedback_tx).await;
				match gamepads.lock().await[idx].as_mut() {
					Some(slot) => slot.device.touch(&gamepad_touch),
					None => tracing::warn!(
						"Received touch for gamepad {}, but no gamepad is connected.",
						gamepad_touch.index
					),
				}
			},
			InputEvent::GamepadMotion(gamepad_motion) => {
				tracing::trace!("Gamepad motion: {gamepad_motion:?}");
				let idx = gamepad_motion.index as usize;
				if idx >= 16 {
					tracing::warn!(
						"Received motion for gamepad {}, but we only have 16 gamepads.",
						gamepad_motion.index
					);
					continue;
				}
				claim_slot(&gamepads, idx, &feedback_tx).await;
				match gamepads.lock().await[idx].as_mut() {
					Some(slot) => slot.device.set_motion(&gamepad_motion),
					None => tracing::warn!(
						"Received motion for gamepad {}, but no gamepad is connected.",
						gamepad_motion.index
					),
				}
			},
			InputEvent::GamepadBattery(gamepad_battery) => {
				tracing::trace!("Gamepad battery: {gamepad_battery:?}");
				let idx = gamepad_battery.index as usize;
				if idx >= 16 {
					tracing::warn!(
						"Received battery for gamepad {}, but we only have 16 gamepads.",
						gamepad_battery.index
					);
					continue;
				}
				claim_slot(&gamepads, idx, &feedback_tx).await;
				match gamepads.lock().await[idx].as_mut() {
					Some(slot) => slot.device.set_battery(&gamepad_battery),
					None => tracing::warn!(
						"Received battery for gamepad {}, but no gamepad is connected.",
						gamepad_battery.index
					),
				}
			},
			InputEvent::GamepadUpdate(gamepad_update) => {
				tracing::trace!("Gamepad update: {gamepad_update:?}");
				let idx = gamepad_update.index as usize;
				if idx >= 16 {
					tracing::warn!(
						"Received update for gamepad {}, but we only have 16 gamepads.",
						gamepad_update.index
					);
					continue;
				}

				// Some clients (e.g. Moonlight Android OSC) send GamepadUpdate without a
				// preceding GamepadInfo. Auto-create a default gamepad in that case.
				{
					let mut slots = gamepads.lock().await;
					if slots[idx].is_none() && gamepad_update.active_gamepad_mask & (1 << gamepad_update.index) != 0 {
						tracing::debug!(
							"Received update for gamepad {} before arrival, auto-creating default gamepad.",
							gamepad_update.index
						);
						let synthetic_info = GamepadInfo::default_for_index(gamepad_update.index as u8);
						ensure_slot(
							&mut slots,
							&synthetic_info,
							&gamepad_config,
							&timer_wake,
							&create_gamepad,
						);
					}
				}
				claim_slot(&gamepads, idx, &feedback_tx).await;

				let mut gamepads = gamepads.lock().await;
				match gamepads[idx].as_mut() {
					Some(slot) => {
						let now = Instant::now();
						slot.apply_buttons(gamepad_update.button_flags(), now);
						slot.device.apply_update(&gamepad_update);
					},
					None => tracing::warn!(
						"Received update for gamepad {}, but no gamepad is connected.",
						gamepad_update.index
					),
				}

				// The client reports controllers that are no longer present:
				// that is a real removal, so destroy the device. The remap state
				// is owned by the GamepadSlot, so dropping it resets hold-to-Home.
				for (i, slot) in gamepads.iter_mut().enumerate() {
					if slot.is_some() && gamepad_update.active_gamepad_mask & (1 << i) == 0 {
						tracing::debug!("Gamepad {} disconnected.", i);
						*slot = None;
					}
				}
			},
			InputEvent::GamepadEnableHaptics => {
				tracing::debug!("Received request to enable haptics on gamepads.");
				// We don't actually need to do anything.
			},
			_ => {
				tracing::warn!("Gamepad handler received unexpected non-gamepad event.");
			},
		}
	}

	// The timer owns an Arc to every slot; explicitly stop it and drop devices
	// on session shutdown so a later session cannot inherit them.
	timer_task.abort();
	for slot in gamepads.lock().await.iter_mut() {
		*slot = None;
	}
	tracing::debug!("Input handler stopped.");
}

async fn run_timer_task<D: VirtualGamepad>(gamepads: Slots<D>, wake: Arc<Notify>) {
	loop {
		// Find the soonest deadline across all gamepads.
		let next_deadline = {
			let gamepads = gamepads.lock().await;
			gamepads
				.iter()
				.filter_map(|s| s.as_ref().and_then(|s| s.next_deadline()))
				.min()
		};

		let timer = async {
			match next_deadline {
				Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
				None => std::future::pending::<()>().await,
			}
		};

		tokio::select! {
			_ = timer => {},
			_ = wake.notified() => {},
		}

		// Advance any gamepad whose deadline has passed.
		let now = Instant::now();
		let mut gamepads = gamepads.lock().await;
		for slot in gamepads.iter_mut().flatten() {
			if slot.next_deadline().is_some_and(|d| now >= d) {
				slot.advance(now);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::InputEvent;

	#[tokio::test]
	async fn owner_reset_orders_key_modifier_mouse_pointer_text_and_gamepad_cleanup() {
		use super::*;
		let stop = ShutdownManager::new();
		let (tx, rx) = calloop::channel::channel();
		let mut handler = InputHandler::new(tx, stop.clone(), Default::default()).unwrap();
		let (feedback, _feedback_rx) = mpsc::channel(10);
		for event in [
			InputEvent::KeyDown(keyboard::Key::A),
			InputEvent::KeyDown(keyboard::Key::LeftControl),
			InputEvent::MouseButtonDown(mouse::MouseButton::Left),
			InputEvent::Utf8Text("queued text".into()),
			InputEvent::Touch(touch::Touch {
				event_kind: PointerEventKind::Down,
				pointer_id: 7,
				x: 0.2,
				y: 0.4,
			}),
			InputEvent::Pen(touch::Pen {
				event_kind: PointerEventKind::Down,
				tool_kind: touch::PenToolKind::Pen,
				buttons: 3,
				x: 0.2,
				y: 0.4,
				pressure_or_distance: 0.5,
				rotation: 0,
				tilt: 0,
			}),
		] {
			handler.handle_input(event, feedback.clone()).await.unwrap();
		}
		let reset = tokio::spawn(async move {
			handler.reset().await.unwrap();
			handler
		});
		let ready = loop {
			if let Ok(CompositorInputEvent::Reset { keys, buttons, ready }) = rx.try_recv() {
				assert_eq!(keys, vec![29, 30]);
				assert_eq!(buttons, vec![0x110]);
				break ready;
			}
			tokio::task::yield_now().await;
		};
		assert!(!reset.is_finished(), "reset waits for compositor cleanup");
		ready.send(()).unwrap();
		let mut handler = reset.await.unwrap();
		assert!(handler.keys.is_empty() && handler.buttons.is_empty());
		handler
			.handle_input(InputEvent::KeyDown(keyboard::Key::B), feedback)
			.await
			.unwrap();
		assert_eq!(handler.keys.iter().copied().collect::<Vec<_>>(), vec![48]);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		stop.wait_shutdown_complete().await;
	}

	/// Recording stand-in for an Inputtino device, so ownership can be tested
	/// without uinput/uhid access.
	mod fake {
		use super::super::*;
		use std::sync::Mutex as StdMutex;

		#[derive(Clone, Default)]
		pub struct Log {
			pub events: Arc<StdMutex<Vec<String>>>,
			pub routes: Arc<StdMutex<Vec<FeedbackRoute>>>,
		}

		impl Log {
			pub fn push(&self, event: String) {
				self.events.lock().unwrap().push(event);
			}
			pub fn take(&self) -> Vec<String> {
				std::mem::take(&mut self.events.lock().unwrap())
			}
			pub fn snapshot(&self) -> Vec<String> {
				self.events.lock().unwrap().clone()
			}
			/// The feedback route held by the device's native callbacks.
			pub fn route(&self, device: usize) -> FeedbackRoute {
				self.routes.lock().unwrap()[device].clone()
			}
		}

		pub struct Pad {
			id: usize,
			log: Log,
		}

		pub fn factory(log: Log) -> impl Fn(&GamepadInfo, FeedbackRoute, GamepadEmulation) -> Result<Pad, ()> + Send {
			move |info, route, _| {
				let id = {
					let mut routes = log.routes.lock().unwrap();
					routes.push(route);
					routes.len() - 1
				};
				log.push(format!("create {id} index {}", info.index));
				Ok(Pad { id, log: log.clone() })
			}
		}

		impl VirtualGamepad for Pad {
			fn neutralize(&mut self) {
				self.log.push(format!("neutralize {}", self.id));
			}
			fn set_pressed(&self, flags: u32) {
				self.log.push(format!("pressed {} {flags:#x}", self.id));
			}
			fn apply_update(&self, _: &GamepadUpdate) {}
			fn touch(&mut self, _: &GamepadTouch) {}
			fn set_motion(&self, _: &GamepadMotion) {}
			fn set_battery(&self, _: &GamepadBattery) {}
		}

		impl Drop for Pad {
			fn drop(&mut self) {
				self.log.push(format!("destroy {}", self.id));
			}
		}
	}

	mod ownership_tests {
		use super::super::*;
		use super::fake;
		use crate::session::stream::control::feedback::RumbleCommand;
		use crate::session::stream::control::input::remap::{BACK_FLAG, SPECIAL_FLAG};

		const PLAYSTATION: u8 = 2;
		const XBOX: u8 = 1;

		fn arrival(index: u8, kind: u8) -> InputEvent {
			InputEvent::GamepadInfo(GamepadInfo::from_bytes(&[index, kind, 0xff, 0, 0, 0, 0, 0]).unwrap())
		}

		fn update(index: u8, mask: u16, buttons: u16) -> InputEvent {
			let mut packet = [0u8; 26];
			packet[2..4].copy_from_slice(&u16::from(index).to_le_bytes());
			packet[4..6].copy_from_slice(&mask.to_le_bytes());
			packet[8..10].copy_from_slice(&buttons.to_le_bytes());
			InputEvent::GamepadUpdate(GamepadUpdate::from_bytes(&packet).unwrap())
		}

		fn rumble(level: u16) -> FeedbackCommand {
			FeedbackCommand::Rumble(RumbleCommand {
				id: 0,
				low_frequency: level,
				high_frequency: level,
			})
		}

		struct Harness {
			handler: InputHandler,
			compositor: calloop::channel::Channel<CompositorInputEvent>,
			log: fake::Log,
			stop: ShutdownManager<SessionShutdownReason>,
		}

		impl Harness {
			fn new(config: GamepadConfig) -> Self {
				let stop = ShutdownManager::new();
				let (tx, compositor) = calloop::channel::channel();
				let log = fake::Log::default();
				let handler =
					InputHandler::with_gamepads(tx, stop.clone(), config, fake::factory(log.clone())).unwrap();
				Self {
					handler,
					compositor,
					log,
					stop,
				}
			}

			async fn send(&mut self, event: InputEvent, owner: &mpsc::Sender<FeedbackCommand>) {
				self.handler.handle_input(event, owner.clone()).await.unwrap();
			}

			/// Wait until the gamepad thread has logged `count` events.
			async fn settle(&self, count: usize) -> Vec<String> {
				for _ in 0..200 {
					if self.log.snapshot().len() >= count {
						break;
					}
					tokio::time::sleep(Duration::from_millis(5)).await;
				}
				self.log.take()
			}

			/// Revoke the owner as a peer disconnect/replacement does,
			/// acknowledging the compositor part of the reset.
			async fn revoke(&mut self) {
				let compositor = &self.compositor;
				let ack = async {
					loop {
						if let Ok(CompositorInputEvent::Reset { ready, .. }) = compositor.try_recv() {
							ready.send(()).unwrap();
							break;
						}
						tokio::task::yield_now().await;
					}
				};
				let (reset, ()) = tokio::join!(self.handler.reset(), ack);
				reset.unwrap();
			}

			async fn stop(self) -> Vec<String> {
				self.stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
				self.stop.wait_shutdown_complete().await;
				self.log.take()
			}
		}

		fn drain(rx: &mut mpsc::Receiver<FeedbackCommand>) -> Vec<FeedbackCommand> {
			std::iter::from_fn(|| rx.try_recv().ok()).collect()
		}

		/// Inputtino invokes feedback callbacks on its own native threads.
		fn native_feedback(route: &FeedbackRoute, command: FeedbackCommand) {
			let route = route.clone();
			std::thread::spawn(move || route.deliver_blocking(command))
				.join()
				.unwrap();
		}

		#[tokio::test]
		async fn peer_replacement_keeps_the_device_but_revokes_state_and_feedback() {
			let mut harness = Harness::new(GamepadConfig::default());
			let (old, mut old_rx) = mpsc::channel(10);
			harness.send(arrival(0, PLAYSTATION), &old).await;
			harness.send(update(0, 1, 0x1000), &old).await;
			assert_eq!(harness.settle(2).await, vec!["create 0 index 0", "pressed 0 0x1000"]);
			assert_eq!(drain(&mut old_rx).len(), 2, "motion enabled for the owner");
			let route = harness.log.route(0);

			// Ownership loss neutralizes the device without unplugging it.
			harness.revoke().await;
			assert_eq!(harness.log.take(), vec!["neutralize 0"]);
			native_feedback(&route, rumble(1));
			assert!(drain(&mut old_rx).is_empty(), "no feedback after revocation");

			// The replacement peer reconnects the same controller: same device,
			// fresh motion enable, and native feedback now reaches only it.
			let (new, mut new_rx) = mpsc::channel(10);
			harness.send(arrival(0, PLAYSTATION), &new).await;
			harness.send(update(0, 1, 0x2000), &new).await;
			assert_eq!(harness.settle(1).await, vec!["pressed 0 0x2000"]);
			assert_eq!(drain(&mut new_rx).len(), 2, "motion re-enabled for the new owner");
			native_feedback(&route, rumble(2));
			assert_eq!(drain(&mut new_rx), vec![rumble(2)]);
			assert!(drain(&mut old_rx).is_empty());

			// Session teardown still destroys the device.
			assert_eq!(harness.stop().await, vec!["neutralize 0", "destroy 0"]);
		}

		#[tokio::test]
		async fn input_without_arrival_rebinds_a_retained_device() {
			let mut harness = Harness::new(GamepadConfig::default());
			let (old, _old_rx) = mpsc::channel(10);
			harness.send(update(0, 1, 0x1000), &old).await;
			assert_eq!(harness.settle(2).await, vec!["create 0 index 0", "pressed 0 0x1000"]);
			harness.revoke().await;
			harness.log.take();

			// Some clients send state without an arrival; the retained device
			// is claimed rather than recreated.
			let (new, mut new_rx) = mpsc::channel(10);
			harness.send(update(0, 1, 0x10), &new).await;
			assert_eq!(harness.settle(1).await, vec!["pressed 0 0x10"]);
			native_feedback(&harness.log.route(0), rumble(3));
			assert_eq!(drain(&mut new_rx), vec![rumble(3)]);
			harness.stop().await;
		}

		#[tokio::test]
		async fn reported_removal_and_identity_change_still_destroy_devices() {
			let mut harness = Harness::new(GamepadConfig::default());
			let (owner, _rx) = mpsc::channel(10);
			harness.send(arrival(0, XBOX), &owner).await;
			harness.send(arrival(1, XBOX), &owner).await;
			assert_eq!(harness.settle(2).await, vec!["create 0 index 0", "create 1 index 1"]);

			// A different virtual identity at the same index replaces the device.
			harness.send(arrival(0, PLAYSTATION), &owner).await;
			assert_eq!(
				harness.settle(3).await,
				vec!["neutralize 0", "destroy 0", "create 2 index 0"]
			);

			// After a reconnect the new client reports only controller 0: the
			// missing controller is a real removal.
			harness.revoke().await;
			harness.log.take();
			let (new, _new_rx) = mpsc::channel(10);
			harness.send(update(0, 0b01, 0), &new).await;
			let events = harness.settle(3).await;
			assert!(events.contains(&"destroy 1".to_string()), "{events:?}");
			assert!(!events.iter().any(|event| event.starts_with("create")), "{events:?}");
			assert!(!events.contains(&"destroy 2".to_string()), "{events:?}");
			harness.stop().await;
		}

		#[tokio::test]
		async fn pending_home_hold_never_fires_across_owners() {
			let mut config = GamepadConfig::default();
			config.home_button.hold_ms = 40;
			let mut harness = Harness::new(config);
			let (old, mut old_rx) = mpsc::channel(10);
			harness.send(update(0, 1, BACK_FLAG as u16), &old).await;
			harness.settle(2).await;

			// Ownership is lost while Back is held and the Home deadline pending.
			harness.revoke().await;
			let (new, mut new_rx) = mpsc::channel(10);
			tokio::time::sleep(Duration::from_millis(120)).await;
			let events = harness.log.take();
			assert!(
				!events.iter().any(|event| event.contains(&format!("{SPECIAL_FLAG:#x}"))),
				"old Home shortcut fired: {events:?}"
			);
			assert!(drain(&mut old_rx).is_empty(), "no activation pulse");

			// The new owner starts from rest and its own hold still works.
			harness.send(update(0, 1, 0), &new).await;
			harness.settle(1).await;
			harness.send(update(0, 1, BACK_FLAG as u16), &new).await;
			tokio::time::sleep(Duration::from_millis(120)).await;
			let events = harness.log.take();
			assert!(
				events.iter().any(|event| event.contains(&format!("{SPECIAL_FLAG:#x}"))),
				"{events:?}"
			);
			assert!(
				drain(&mut new_rx)
					.iter()
					.any(|command| matches!(command, FeedbackCommand::Rumble(_)))
			);
			harness.stop().await;
		}
	}

	#[test]
	fn parses_utf8_text_event() {
		let payload = [0x17, 0x00, 0x00, 0x00, b'h', b'e', b'y'];
		assert!(matches!(
			InputEvent::from_bytes(&payload),
			Ok(InputEvent::Utf8Text(text)) if text == "hey"
		));
	}

	#[test]
	fn rejects_empty_utf8_text_event() {
		let payload = [0x17, 0x00, 0x00, 0x00];
		assert!(InputEvent::from_bytes(&payload).is_err());
	}

	const EVENT_TYPES: [u32; 17] = [
		0x03,
		0x04,
		0x05,
		0x07,
		0x08,
		0x09,
		0x0A,
		0x5500_0001,
		0x5500_0002,
		0x5500_0003,
		0x5500_0004,
		0x5500_0005,
		0x5500_0006,
		0x5500_0007,
		0x0C,
		0x0D,
		0x17,
	];

	/// Every subtype decoder bounds-checks before reading: all truncations of
	/// zero-, one- and pattern-filled payloads yield a value or an error.
	#[test]
	fn every_subtype_truncation_is_bounded() {
		for event_type in EVENT_TYPES {
			for fill in [0x00, 0x01, 0xff, 0x7f] {
				for len in 0..64 {
					let mut payload = event_type.to_le_bytes().to_vec();
					payload.extend(std::iter::repeat_n(fill, len));
					let _ = InputEvent::from_bytes(&payload);
				}
			}
		}
		for len in 0..4 {
			assert!(InputEvent::from_bytes(&[0x03; 4][..len]).is_err());
		}
	}

	/// Non-finite motion and touch values are rejected before they reach the
	/// native device or the touchpad conversion.
	#[test]
	fn rejects_non_finite_gamepad_values() {
		let finite = 0.5_f32.to_le_bytes();
		for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
			for field in 0..3 {
				let mut motion = 0x5500_0006_u32.to_le_bytes().to_vec();
				motion.extend([0, 1, 0, 0]); // index, acceleration, reserved
				for i in 0..3 {
					motion.extend(if i == field { bad.to_le_bytes() } else { finite });
				}
				assert!(InputEvent::from_bytes(&motion).is_err(), "motion {field} {bad}");

				let mut touch = 0x5500_0005_u32.to_le_bytes().to_vec();
				touch.extend([0, 1, 0, 0, 1, 0, 0, 0]); // index, event, reserved, pointer
				for i in 0..3 {
					touch.extend(if i == field { bad.to_le_bytes() } else { finite });
				}
				assert!(InputEvent::from_bytes(&touch).is_err(), "touch {field} {bad}");
			}
		}
		let mut motion = 0x5500_0006_u32.to_le_bytes().to_vec();
		motion.extend([0, 2, 0, 0]);
		motion.extend(finite.repeat(3));
		assert!(matches!(
			InputEvent::from_bytes(&motion),
			Ok(InputEvent::GamepadMotion(_))
		));
	}
}
