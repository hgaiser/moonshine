use std::sync::Arc;
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use strum_macros::FromRepr;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use self::gamepad::Gamepad;
use self::gamepad::GamepadConfig;
use self::remap::{HoldToHome, HoldTransition};
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
mod remap;

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
	GamepadInfo = 0x55000004, // Called ControllerArrival in Moonlight.
	GamepadTouch = 0x55000005,
	GamepadMotion = 0x55000006,
	GamepadBattery = 0x55000007,
	GamepadUpdate = 0x0000000C,
	GamepadEnableHaptics = 0x0000000D,
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
	GamepadInfo(GamepadInfo),
	GamepadTouch(GamepadTouch),
	GamepadMotion(GamepadMotion),
	GamepadBattery(GamepadBattery),
	GamepadUpdate(GamepadUpdate),
	GamepadEnableHaptics,
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
			None => {
				tracing::warn!("Received unknown event type: {event_type}");
				Err(())
			},
		}
	}
}

pub(crate) struct InputHandler {
	input_tx: calloop::channel::Sender<CompositorInputEvent>,
	gamepad_tx: mpsc::Sender<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
}

impl InputHandler {
	pub fn new(
		input_tx: calloop::channel::Sender<CompositorInputEvent>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
		gamepad_config: GamepadConfig,
	) -> Result<Self, ()> {
		let (gamepad_tx, gamepad_rx) = mpsc::channel(10);

		std::thread::spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.expect("Failed to create tokio runtime for input handler");

			rt.block_on(async move {
				let local = tokio::task::LocalSet::new();
				local
					.run_until(async move {
						run_gamepad_handler(gamepad_rx, stop_session_manager, gamepad_config).await;
					})
					.await;
			});
		});

		Ok(Self { input_tx, gamepad_tx })
	}

	async fn handle_input(&self, event: InputEvent, feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		match event {
			InputEvent::KeyDown(key) => {
				if let Some(keycode) = key.to_linux_keycode() {
					tracing::trace!("Pressing key: {key:?} (keycode: {keycode})");
					let _ = self.input_tx.send(CompositorInputEvent::KeyDown { keycode });
				}
			},
			InputEvent::KeyUp(key) => {
				if let Some(keycode) = key.to_linux_keycode() {
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
				let _ = self
					.input_tx
					.send(CompositorInputEvent::MouseButtonDown { button: button_code });
			},
			InputEvent::MouseButtonUp(button) => {
				tracing::trace!("Releasing mouse button: {button:?}");
				let button_code: u32 = button.into();
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
			// Gamepad events: forward to gamepad handler thread.
			gamepad_event => {
				self.gamepad_tx
					.send((gamepad_event, feedback))
					.await
					.map_err(|e| tracing::warn!("Failed to send gamepad event: {e}"))?;
			},
		}
		Ok(())
	}

	pub async fn handle_raw_input(&self, event: &[u8], feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		let event = InputEvent::from_bytes(event)?;
		self.handle_input(event, feedback).await
	}
}

/// Per-gamepad slot holding the inputtino wrapper, remap state, and rumble tracking.
struct GamepadSlot {
	/// The underlying inputtino joypad.
	gamepad: Gamepad,

	/// Hold-to-Home button remap state machine.
	remap: HoldToHome,

	/// Channel to send feedback commands (rumble, LED, trigger effects, motion)
	/// back to the client.
	feedback_tx: mpsc::Sender<FeedbackCommand>,

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

impl GamepadSlot {
	async fn new(
		info: &GamepadInfo,
		feedback_tx: mpsc::Sender<FeedbackCommand>,
		config: &GamepadConfig,
		timer_wake: Arc<Notify>,
	) -> Result<Self, ()> {
		let gamepad = Gamepad::new(info, feedback_tx.clone()).await?;
		Ok(Self {
			gamepad,
			remap: HoldToHome::new(config),
			feedback_tx,
			index: info.index,
			timer_wake,
			home_rumble_intensity: config.home_button.rumble_intensity,
			home_rumble_duration: Duration::from_millis(config.home_button.rumble_duration_ms),
			home_rumble_off_at: None,
		})
	}

	/// Apply button flags through the remap layer. Returns any transition.
	fn apply_buttons(&mut self, flags: u32, now: Instant) -> HoldTransition {
		let (remapped, transition) = self.remap.apply(flags, now);
		self.gamepad.set_pressed(remapped);
		self.check_rumble(now);
		// Wake the timer task so it can pick up any new deadline.
		self.timer_wake.notify_one();
		self.maybe_fire_rumble(transition, now)
	}

	/// Advance the remap state machine using internally tracked button state.
	fn advance(&mut self, now: Instant) -> HoldTransition {
		self.check_rumble(now);
		let (remapped, transition) = self.remap.advance(now);
		self.gamepad.set_pressed(remapped);
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
		if let Some(off_at) = self.home_rumble_off_at {
			if now >= off_at {
				self.send_rumble(0, 0);
				self.home_rumble_off_at = None;
			}
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
		let _ = self.feedback_tx.try_send(FeedbackCommand::Rumble(
			crate::session::stream::control::feedback::RumbleCommand {
				id: self.index as u16,
				low_frequency,
				high_frequency,
			},
		));
	}
}

async fn run_gamepad_handler(
	mut command_rx: mpsc::Receiver<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	gamepad_config: GamepadConfig,
) {
	let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::InputHandlerStopped);
	let _delay_stop = stop_session_manager.delay_shutdown_token();

	let gamepads = Arc::new(Mutex::new([const { None }; 16]));
	let timer_wake = Arc::new(Notify::new());

	// Spawn a timer task that advances gamepads with pending deadlines.
	let gamepads_timer = gamepads.clone();
	let timer_wake_for_timer = timer_wake.clone();
	tokio::task::spawn_local(run_timer_task(gamepads_timer, timer_wake_for_timer));

	while let Ok(Some((command, feedback_tx))) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
		match command {
			InputEvent::GamepadInfo(gamepad) => {
				tracing::debug!("Gamepad info: {gamepad:?}");
				let mut gamepads = gamepads.lock().await;
				if gamepad.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received info for gamepad {}, but we only have {} slots.",
						gamepad.index,
						gamepads.len()
					);
					continue;
				}

				if gamepads[gamepad.index as usize].is_none() {
					if let Ok(new_slot) =
						GamepadSlot::new(&gamepad, feedback_tx, &gamepad_config, timer_wake.clone()).await
					{
						gamepads[gamepad.index as usize] = Some(new_slot);
						tracing::info!("Gamepad {} connected.", gamepad.index);
					}
				}
			},
			InputEvent::GamepadTouch(gamepad_touch) => {
				tracing::trace!("Gamepad touch: {gamepad_touch:?}");
				let mut gamepads = gamepads.lock().await;
				if gamepad_touch.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received touch for gamepad {}, but we only have {} gamepads.",
						gamepad_touch.index,
						gamepads.len()
					);
					continue;
				}

				match gamepads[gamepad_touch.index as usize].as_mut() {
					Some(slot) => slot.gamepad.touch(&gamepad_touch),
					None => tracing::warn!(
						"Received touch for gamepad {}, but no gamepad is connected.",
						gamepad_touch.index
					),
				}
			},
			InputEvent::GamepadMotion(gamepad_motion) => {
				tracing::trace!("Gamepad motion: {gamepad_motion:?}");
				let mut gamepads = gamepads.lock().await;
				if gamepad_motion.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received motion for gamepad {}, but we only have {} gamepads.",
						gamepad_motion.index,
						gamepads.len()
					);
					continue;
				}

				match gamepads[gamepad_motion.index as usize].as_mut() {
					Some(slot) => slot.gamepad.set_motion(&gamepad_motion),
					None => tracing::warn!(
						"Received motion for gamepad {}, but no gamepad is connected.",
						gamepad_motion.index
					),
				}
			},
			InputEvent::GamepadBattery(gamepad_battery) => {
				tracing::trace!("Gamepad battery: {gamepad_battery:?}");
				let mut gamepads = gamepads.lock().await;
				if gamepad_battery.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received battery for gamepad {}, but we only have {} gamepads.",
						gamepad_battery.index,
						gamepads.len()
					);
					continue;
				}

				match gamepads[gamepad_battery.index as usize].as_mut() {
					Some(slot) => slot.gamepad.set_battery(&gamepad_battery),
					None => tracing::warn!(
						"Received battery for gamepad {}, but no gamepad is connected.",
						gamepad_battery.index
					),
				}
			},
			InputEvent::GamepadUpdate(gamepad_update) => {
				tracing::trace!("Gamepad update: {gamepad_update:?}");
				let mut gamepads = gamepads.lock().await;
				if gamepad_update.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received update for gamepad {}, but we only have {} gamepads.",
						gamepad_update.index,
						gamepads.len()
					);
					continue;
				}

				let idx = gamepad_update.index as usize;

				// Some clients (e.g. Moonlight Android OSC) send GamepadUpdate without a
				// preceding GamepadInfo. Auto-create a default gamepad in that case.
				if gamepads[idx].is_none() && gamepad_update.active_gamepad_mask & (1 << gamepad_update.index) != 0 {
					tracing::debug!(
						"Received update for gamepad {} before arrival, auto-creating default gamepad.",
						gamepad_update.index
					);
					let synthetic_info = GamepadInfo::default_for_index(gamepad_update.index as u8);
					if let Ok(new_slot) = GamepadSlot::new(
						&synthetic_info,
						feedback_tx.clone(),
						&gamepad_config,
						timer_wake.clone(),
					)
					.await
					{
						gamepads[idx] = Some(new_slot);
					}
				}

				match gamepads[idx].as_mut() {
					Some(slot) => {
						let now = Instant::now();
						slot.apply_buttons(gamepad_update.button_flags(), now);
						slot.gamepad.apply_update(&gamepad_update);
					},
					None => tracing::warn!(
						"Received update for gamepad {}, but no gamepad is connected.",
						gamepad_update.index
					),
				}

				// Disconnect gamepads that are no longer active. The remap state is
				// owned by the GamepadSlot, so dropping it resets the hold-to-Home state.
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

	tracing::debug!("Input handler stopped.");
}

async fn run_timer_task(gamepads: Arc<Mutex<[Option<GamepadSlot>; 16]>>, wake: Arc<Notify>) {
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
