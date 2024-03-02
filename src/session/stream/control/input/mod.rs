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
	gamepad::{GamepadInfo, GamepadUpdate}
};

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
	GamepadUpdate(GamepadUpdate),
}

impl InputEvent {
	fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			log::warn!("Expected control message to have at least 4 bytes, got {}", buffer.len());
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
			Some(InputEventType::GamepadUpdate) => Ok(InputEvent::GamepadUpdate(GamepadUpdate::from_bytes(&buffer[4..])?)),
			None => {
				log::warn!("Received unknown event type: {event_type}");
				Err(())
			}
		}
	}
}

pub struct InputHandler {
	command_tx: mpsc::Sender<InputEvent>,
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

	async fn handle_input(&self, event: InputEvent) -> Result<(), ()> {
		self.command_tx.send(event).await
			.map_err(|e| log::error!("Failed to send input event: {e}"))
	}

	pub async fn handle_raw_input<'a>(&self, event: &'a [u8]) -> Result<(), ()> {
		let event = InputEvent::from_bytes(event)?;
		self.handle_input(event).await
	}
}

struct InputHandlerInner {
	mouse: Mouse,
	keyboard: Keyboard,
}

impl InputHandlerInner {
	pub async fn run(mut self, mut command_rx: mpsc::Receiver<InputEvent>) {
		let mut gamepads = Vec::new();

		while let Some(command) = command_rx.recv().await {
			match command {
				InputEvent::KeyDown(key) => {
					log::trace!("Pressing key: {key:?}");
					let _ = self.keyboard.key_down(key);
				},
				InputEvent::KeyUp(key) => {
					log::trace!("Releasing key: {key:?}");
					let _ = self.keyboard.key_up(key);
				},
				InputEvent::MouseMoveAbsolute(event) => {
					log::trace!("Absolute mouse movement: {event:?}");
					let _ = self.mouse.move_absolute(event.x as i32, event.y as i32);
				},
				InputEvent::MouseMoveRelative(event) => {
					log::trace!("Moving mouse relative: {event:?}");
					let _ = self.mouse.move_relative(event.x as i32, event.y as i32);
				},
				InputEvent::MouseButtonDown(button) => {
					log::trace!("Pressing mouse button: {button:?}");
					let _ = self.mouse.button_down(button);
				},
				InputEvent::MouseButtonUp(button) => {
					log::trace!("Releasing mouse button: {button:?}");
					let _ = self.mouse.button_up(button);
				},
				InputEvent::MouseScrollVertical(event) => {
					log::trace!("Scrolling vertically: {event:?}");
					let _ = self.mouse.scroll_vertical(event.amount);
				},
				InputEvent::MouseScrollHorizontal(event) => {
					log::trace!("Scrolling horizontally: {event:?}");
					let _ = self.mouse.scroll_horizontal(event.amount);
				},
				InputEvent::GamepadInfo(gamepad) => {
					log::debug!("Gamepad info: {gamepad:?}");
					if let Ok(gamepad) = Gamepad::new(gamepad) {
						gamepads.push(gamepad);
					}
				},
				InputEvent::GamepadUpdate(gamepad_update) => {
					log::trace!("Gamepad update: {gamepad_update:?}");
					if gamepad_update.index as usize >= gamepads.len() {
						log::warn!("Received update for gamepad {}, but we only have {} gamepads.", gamepad_update.index, gamepads.len());
						continue;
					}

					let _ = gamepads[gamepad_update.index as usize].update(gamepad_update);
				},
			}
		}

		log::debug!("Input handler closing.");
	}
}
