use gamepad::GamepadMotion;
use strum_macros::FromRepr;
use tokio::sync::mpsc;

use crate::session::stream::control::input::gamepad::Gamepad;

use self::{
	mouse::{
		Mouse,
		MouseButton,
		MouseMoveAbsolute,
		MouseMoveRelative,
		MouseScrollVertical,
		MouseScrollHorizontal,
	},
	keyboard::{Keyboard, Key},
	gamepad::{GamepadInfo, GamepadTouch, GamepadUpdate}
};

use super::FeedbackCommand;

mod keyboard;
mod mouse;
mod gamepad;

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
	GamepadUpdate = 0x0000000C,
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
	GamepadUpdate(GamepadUpdate),
	GamepadMotion(GamepadMotion),
}

impl InputEvent {
	fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			tracing::warn!("Expected control message to have at least 4 bytes, got {}", buffer.len());
			return Err(());
		}

		let event_type = u32::from_le_bytes(buffer[..4].try_into().unwrap());
		match InputEventType::from_repr(event_type) {
			Some(InputEventType::KeyDown) => Ok(InputEvent::KeyDown(Key::from_bytes(&buffer[4..])?)),
			Some(InputEventType::KeyUp) => Ok(InputEvent::KeyUp(Key::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseMoveAbsolute) => Ok(InputEvent::MouseMoveAbsolute(MouseMoveAbsolute::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseMoveRelative) => Ok(InputEvent::MouseMoveRelative(MouseMoveRelative::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseButtonDown) => Ok(InputEvent::MouseButtonDown(MouseButton::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseButtonUp) => Ok(InputEvent::MouseButtonUp(MouseButton::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseScrollVertical) => Ok(InputEvent::MouseScrollVertical(MouseScrollVertical::from_bytes(&buffer[4..])?)),
			Some(InputEventType::MouseScrollHorizontal) => Ok(InputEvent::MouseScrollHorizontal(MouseScrollHorizontal::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadInfo) => Ok(InputEvent::GamepadInfo(GamepadInfo::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadTouch) => Ok(InputEvent::GamepadTouch(GamepadTouch::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadMotion) => Ok(InputEvent::GamepadMotion(GamepadMotion::from_bytes(&buffer[4..])?)),
			Some(InputEventType::GamepadUpdate) => Ok(InputEvent::GamepadUpdate(GamepadUpdate::from_bytes(&buffer[4..])?)),
			None => {
				tracing::warn!("Received unknown event type: {event_type}");
				Err(())
			}
		}
	}
}

pub struct InputHandler {
	command_tx: mpsc::Sender<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
}

impl InputHandler {
	pub fn new() -> Result<Self, ()> {
		let mouse = Mouse::new()?;
		let keyboard = Keyboard::new()?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = InputHandlerInner { mouse, keyboard };
		tokio::spawn(inner.run(command_rx));

		Ok(Self { command_tx })
	}

	async fn handle_input(&self, event: InputEvent, feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		self.command_tx.send((event, feedback)).await
			.map_err(|e| tracing::error!("Failed to send input event: {e}"))
	}

	pub async fn handle_raw_input(&self, event: &[u8], feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		let event = InputEvent::from_bytes(event)?;
		self.handle_input(event, feedback).await
	}
}

struct InputHandlerInner {
	mouse: Mouse,
	keyboard: Keyboard,
}

impl InputHandlerInner {
	pub async fn run(mut self, mut command_rx: mpsc::Receiver<(InputEvent, mpsc::Sender<FeedbackCommand>)>) {
		let mut gamepads = Vec::new();

		while let Some((command, feedback_tx)) = command_rx.recv().await {
			match command {
				InputEvent::KeyDown(key) => {
					tracing::trace!("Pressing key: {key:?}");
					self.keyboard.key_down(key);
				},
				InputEvent::KeyUp(key) => {
					tracing::trace!("Releasing key: {key:?}");
					self.keyboard.key_up(key);
				},
				InputEvent::MouseMoveAbsolute(event) => {
					tracing::trace!("Absolute mouse movement: {event:?}");
					self.mouse.move_absolute(
						event.x as i32,
						event.y as i32,
						event.screen_width as i32,
						event.screen_height as i32,
					);
				},
				InputEvent::MouseMoveRelative(event) => {
					tracing::trace!("Moving mouse relative: {event:?}");
					self.mouse.move_relative(event.x as i32, event.y as i32);
				},
				InputEvent::MouseButtonDown(button) => {
					tracing::trace!("Pressing mouse button: {button:?}");
					self.mouse.button_down(button);
				},
				InputEvent::MouseButtonUp(button) => {
					tracing::trace!("Releasing mouse button: {button:?}");
					self.mouse.button_up(button);
				},
				InputEvent::MouseScrollVertical(event) => {
					tracing::trace!("Scrolling vertically: {event:?}");
					self.mouse.scroll_vertical(event.amount);
				},
				InputEvent::MouseScrollHorizontal(event) => {
					tracing::trace!("Scrolling horizontally: {event:?}");
					self.mouse.scroll_horizontal(event.amount);
				},
				InputEvent::GamepadInfo(gamepad) => {
					tracing::debug!("Gamepad info: {gamepad:?}");
					if let Ok(gamepad) = Gamepad::new(gamepad, feedback_tx).await {
						gamepads.push(gamepad);
					}
				},
				InputEvent::GamepadTouch(gamepad_touch) => {
					tracing::trace!("Gamepad touch: {gamepad_touch:?}");
					if gamepad_touch.index as usize >= gamepads.len() {
						tracing::warn!("Received touch for gamepad {}, but we only have {} gamepads.", gamepad_touch.index, gamepads.len());
						continue;
					}

					gamepads[gamepad_touch.index as usize].touch(gamepad_touch);
				},
				InputEvent::GamepadMotion(gamepad_motion) => {
					tracing::trace!("Gamepad motion: {gamepad_motion:?}");
					if gamepad_motion.index as usize >= gamepads.len() {
						tracing::warn!("Received motion for gamepad {}, but we only have {} gamepads.", gamepad_motion.index, gamepads.len());
						continue;
					}

					gamepads[gamepad_motion.index as usize].set_motion(gamepad_motion);
				},
				InputEvent::GamepadUpdate(gamepad_update) => {
					tracing::trace!("Gamepad update: {gamepad_update:?}");
					if gamepad_update.index as usize >= gamepads.len() {
						tracing::warn!("Received update for gamepad {}, but we only have {} gamepads.", gamepad_update.index, gamepads.len());
						continue;
					}

					gamepads[gamepad_update.index as usize].update(gamepad_update);
				},
			}
		}

		tracing::debug!("Input handler closing.");
	}
}
