use async_shutdown::ShutdownManager;
use strum_macros::FromRepr;
use tokio::sync::mpsc;

use crate::session::{
	compositor::input::CompositorInputEvent, manager::SessionShutdownReason, stream::control::input::gamepad::Gamepad,
};

use self::{
	gamepad::{GamepadBattery, GamepadInfo, GamepadMotion, GamepadTouch, GamepadUpdate},
	keyboard::Key,
	mouse::{MouseButton, MouseMoveAbsolute, MouseMoveRelative, MouseScrollHorizontal, MouseScrollVertical},
};

use super::FeedbackCommand;

mod gamepad;
mod keyboard;
mod mouse;

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

pub struct InputHandler {
	input_tx: calloop::channel::Sender<CompositorInputEvent>,
	gamepad_tx: mpsc::Sender<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
}

impl InputHandler {
	pub fn new(
		input_tx: calloop::channel::Sender<CompositorInputEvent>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
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
						run_gamepad_handler(gamepad_rx, stop_session_manager).await;
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

async fn run_gamepad_handler(
	mut command_rx: mpsc::Receiver<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
) {
	// Trigger session shutdown when the input handler stops.
	let _session_stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::InputHandlerStopped);
	let _delay_stop = stop_session_manager.delay_shutdown_token();

	let mut gamepads: [Option<Gamepad>; 16] = Default::default();

	while let Ok(Some((command, feedback_tx))) = stop_session_manager.wrap_cancel(command_rx.recv()).await {
		match command {
			InputEvent::GamepadInfo(gamepad) => {
				tracing::debug!("Gamepad info: {gamepad:?}");
				if gamepad.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received info for gamepad {}, but we only have {} slots.",
						gamepad.index,
						gamepads.len()
					);
					continue;
				}

				if gamepads[gamepad.index as usize].is_none() {
					if let Ok(new_gamepad) = Gamepad::new(&gamepad, feedback_tx).await {
						gamepads[gamepad.index as usize] = Some(new_gamepad);
						tracing::info!("Gamepad {} connected.", gamepad.index);
						tracing::info!("Gamepad info {:#?}", gamepad);
					}
				}
			},
			InputEvent::GamepadTouch(gamepad_touch) => {
				tracing::trace!("Gamepad touch: {gamepad_touch:?}");
				if gamepad_touch.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received touch for gamepad {}, but we only have {} gamepads.",
						gamepad_touch.index,
						gamepads.len()
					);
					continue;
				}

				match gamepads[gamepad_touch.index as usize].as_mut() {
					Some(gamepad) => gamepad.touch(&gamepad_touch),
					None => tracing::warn!(
						"Received touch for gamepad {}, but no gamepad is connected.",
						gamepad_touch.index
					),
				}
			},
			InputEvent::GamepadMotion(gamepad_motion) => {
				tracing::trace!("Gamepad motion: {gamepad_motion:?}");
				if gamepad_motion.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received motion for gamepad {}, but we only have {} gamepads.",
						gamepad_motion.index,
						gamepads.len()
					);
					continue;
				}

				match gamepads[gamepad_motion.index as usize].as_mut() {
					Some(gamepad) => gamepad.set_motion(&gamepad_motion),
					None => tracing::warn!(
						"Received motion for gamepad {}, but no gamepad is connected.",
						gamepad_motion.index
					),
				}
			},
			InputEvent::GamepadBattery(gamepad_battery) => {
				tracing::trace!("Gamepad battery: {gamepad_battery:?}");
				if gamepad_battery.index as usize >= gamepads.len() {
					tracing::warn!(
						"Received battery for gamepad {}, but we only have {} gamepads.",
						gamepad_battery.index,
						gamepads.len()
					);
					continue;
				}

				match gamepads[gamepad_battery.index as usize].as_mut() {
					Some(gamepad) => gamepad.set_battery(&gamepad_battery),
					None => tracing::warn!(
						"Received battery for gamepad {}, but no gamepad is connected.",
						gamepad_battery.index
					),
				}
			},
			InputEvent::GamepadUpdate(gamepad_update) => {
				tracing::trace!("Gamepad update: {gamepad_update:?}");
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
					if let Ok(new_gamepad) = Gamepad::new(&synthetic_info, feedback_tx.clone()).await {
						gamepads[idx] = Some(new_gamepad);
					}
				}

				match gamepads[idx].as_mut() {
					Some(gamepad) => gamepad.update(&gamepad_update),
					None => tracing::warn!(
						"Received update for gamepad {}, but no gamepad is connected.",
						gamepad_update.index
					),
				}

				// Disconnect gamepads that are no longer active.
				for (i, gamepad) in gamepads.iter_mut().enumerate() {
					if gamepad.is_some() && gamepad_update.active_gamepad_mask & (1 << i) == 0 {
						tracing::debug!("Gamepad {} disconnected.", i);
						*gamepad = None;
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
