use async_shutdown::ShutdownManager;
use strum_macros::FromRepr;
use tokio::sync::mpsc;

use crate::session::{manager::SessionShutdownReason, stream::control::input::gamepad::Gamepad};

use self::{
	gamepad::{GamepadBattery, GamepadInfo, GamepadMotion, GamepadTouch, GamepadUpdate},
	keyboard::{Key, Keyboard},
	mouse::{Mouse, MouseButton, MouseMoveAbsolute, MouseMoveRelative, MouseScrollHorizontal, MouseScrollVertical},
	reis::ReisClient,
};
use ::reis::event::{DeviceCapability, EiEvent};

use super::FeedbackCommand;

mod gamepad;
mod keyboard;
mod mouse;
mod reis;

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
	command_tx: mpsc::Sender<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
}

impl InputHandler {
	pub fn new(stop_session_manager: ShutdownManager<SessionShutdownReason>) -> Result<Self, ()> {
		let (command_tx, command_rx) = mpsc::channel(10);

		std::thread::spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.expect("Failed to create tokio runtime for input handler");

			rt.block_on(async move {
				let local = tokio::task::LocalSet::new();
				local
					.run_until(async move {
						let mouse = Mouse::new().expect("Failed to create mouse");
						let keyboard = Keyboard::new().expect("Failed to create keyboard");
						let inner = InputHandlerInner { mouse, keyboard };
						inner.run(command_rx, stop_session_manager).await;
					})
					.await;
			});
		});

		Ok(Self { command_tx })
	}

	async fn handle_input(&self, event: InputEvent, feedback: mpsc::Sender<FeedbackCommand>) -> Result<(), ()> {
		self.command_tx
			.send((event, feedback))
			.await
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
	pub async fn run(
		mut self,
		mut command_rx: mpsc::Receiver<(InputEvent, mpsc::Sender<FeedbackCommand>)>,
		stop_session_manager: ShutdownManager<SessionShutdownReason>,
	) {
		// Trigger session shutdown when the input handler stops.
		let _session_stop_token =
			stop_session_manager.trigger_shutdown_token(SessionShutdownReason::InputHandlerStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		let mut gamepads: [Option<Gamepad>; 16] = Default::default();

		let mut reis_client = match ReisClient::new().await {
			Ok(client) => Some(client),
			Err(e) => {
				tracing::debug!("Could not yet connect to libei: {e}");
				None
			},
		};

		enum ReisWorkResult {
			Event(Option<Result<EiEvent, ::reis::Error>>),
			Connected(Box<ReisClient>),
			ConnectError(std::io::Error),
		}

		loop {
			tokio::select! {
				res = async {
					if let Some(client) = &mut reis_client {
						let event = client.next_event().await;
						ReisWorkResult::Event(event)
					} else {
						tokio::time::sleep(std::time::Duration::from_secs(1)).await;
						match ReisClient::new().await {
							Ok(client) => ReisWorkResult::Connected(Box::new(client)),
							Err(e) => ReisWorkResult::ConnectError(e),
						}
					}
				} => {
					match res {
						ReisWorkResult::Event(Some(Ok(event))) => {
							self.handle_reis_event(event);
							if let Some(client) = &reis_client {
								if let Err(e) = client.flush() {
									tracing::error!("Failed to flush reis client: {e}");
								}
							}
						}
						ReisWorkResult::Event(Some(Err(e))) => {
							tracing::error!("Reis error: {e}");
							reis_client = None;
						}
						ReisWorkResult::Event(None) => {
							tracing::debug!("Reis stream ended.");
							reis_client = None;
						}
						ReisWorkResult::Connected(client) => {
							tracing::debug!("Connected to libeis.");
							reis_client = Some(*client);
						}
						ReisWorkResult::ConnectError(e) => {
							tracing::debug!("Could not yet connect to libeis: {e}");
						}
					}
				}
				cmd = stop_session_manager.wrap_cancel(command_rx.recv()) => {
					match cmd {
						Ok(Some((command, feedback_tx))) => {
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
									if gamepad.index as usize >= gamepads.len() {
										tracing::warn!("Received info for gamepad {}, but we only have {} slots.", gamepad.index, gamepads.len());
										continue;
									}

									if gamepads[gamepad.index as usize].is_none() {
										if let Ok(new_gamepad) = Gamepad::new(&gamepad, feedback_tx).await {
											gamepads[gamepad.index as usize] = Some(new_gamepad);
											tracing::info!("Gamepad {} connected.", gamepad.index);
										}
									}
								},
								InputEvent::GamepadTouch(gamepad_touch) => {
									tracing::trace!("Gamepad touch: {gamepad_touch:?}");
									if gamepad_touch.index as usize >= gamepads.len() {
										tracing::warn!("Received touch for gamepad {}, but we only have {} gamepads.", gamepad_touch.index, gamepads.len());
										continue;
									}

									match gamepads[gamepad_touch.index as usize].as_mut() {
										Some(gamepad) => gamepad.touch(&gamepad_touch),
										None => tracing::warn!("Received touch for gamepad {}, but no gamepad is connected.", gamepad_touch.index),
									}
								},
								InputEvent::GamepadMotion(gamepad_motion) => {
									tracing::trace!("Gamepad motion: {gamepad_motion:?}");
									if gamepad_motion.index as usize >= gamepads.len() {
										tracing::warn!("Received motion for gamepad {}, but we only have {} gamepads.", gamepad_motion.index, gamepads.len());
										continue;
									}

									match gamepads[gamepad_motion.index as usize].as_mut() {
										Some(gamepad) => gamepad.set_motion(&gamepad_motion),
										None => tracing::warn!("Received motion for gamepad {}, but no gamepad is connected.", gamepad_motion.index),
									}
								},
								InputEvent::GamepadBattery(gamepad_battery) => {
									tracing::trace!("Gamepad battery: {gamepad_battery:?}");
									if gamepad_battery.index as usize >= gamepads.len() {
										tracing::warn!("Received battery for gamepad {}, but we only have {} gamepads.", gamepad_battery.index, gamepads.len());
										continue;
									}

									match gamepads[gamepad_battery.index as usize].as_mut() {
										Some(gamepad) => gamepad.set_battery(&gamepad_battery),
										None => tracing::warn!("Received battery for gamepad {}, but no gamepad is connected.", gamepad_battery.index),
									}
								},
								InputEvent::GamepadUpdate(gamepad_update) => {
									tracing::trace!("Gamepad update: {gamepad_update:?}");
									if gamepad_update.index as usize >= gamepads.len() {
										tracing::warn!("Received update for gamepad {}, but we only have {} gamepads.", gamepad_update.index, gamepads.len());
										continue;
									}

									match gamepads[gamepad_update.index as usize].as_mut() {
										Some(gamepad) => gamepad.update(&gamepad_update),
										None => tracing::warn!("Received update for gamepad {}, but no gamepad is connected.", gamepad_update.index),
									}

									// Disconnect gamepads that are no longer active.
									for (i, gamepad) in gamepads.iter_mut().enumerate() {
										if gamepad.is_some() && gamepad_update.active_gamepad_mask & (1 << i) == 0 {
											tracing::info!("Gamepad {} disconnected.", i);
											*gamepad = None;
										}
									}
								},
								InputEvent::GamepadEnableHaptics => {
									tracing::debug!("Received request to enable haptics on gamepads.");
									// We don't actually need to do anything.
								},
							}

							if let Some(client) = &reis_client {
								if let Err(e) = client.flush() {
									tracing::error!("Failed to flush reis client: {e}");
								}
							}
						}
						_ => break,
					}
				}
			}
		}

		tracing::debug!("Input handler stopped.");
	}

	fn handle_reis_event(&mut self, event: EiEvent) {
		match event {
			EiEvent::SeatAdded(e) => {
				e.seat.bind_capabilities(&[
					DeviceCapability::Pointer,
					DeviceCapability::Keyboard,
					DeviceCapability::Button,
					DeviceCapability::Scroll,
					DeviceCapability::PointerAbsolute,
				]);
			},
			EiEvent::DeviceAdded(e) => {
				let device = e.device;
				if device.has_capability(DeviceCapability::Pointer) {
					if let Some(pointer) = device.interface::<::reis::ei::Pointer>() {
						self.mouse.set_pointer(pointer);
					}
					if let Some(pointer_absolute) = device.interface::<::reis::ei::PointerAbsolute>() {
						self.mouse.set_pointer_absolute(pointer_absolute);
					}
					if let Some(button) = device.interface::<::reis::ei::Button>() {
						self.mouse.set_button(button);
					}
					if let Some(scroll) = device.interface::<::reis::ei::Scroll>() {
						self.mouse.set_scroll(scroll);
					}
					self.mouse.set_device(device.device().clone());
				}
				if device.has_capability(DeviceCapability::Keyboard) {
					if let Some(keyboard) = device.interface::<::reis::ei::Keyboard>() {
						self.keyboard.set_keyboard(keyboard);
					}
					self.keyboard.set_device(device.device().clone());
				}
			},
			EiEvent::DeviceResumed(e) => {
				let device = e.device;
				device.device().start_emulating(e.serial, 1);
			},
			EiEvent::DevicePaused(e) => {
				let device = e.device;
				device.device().stop_emulating(e.serial);
			},
			_ => {},
		}
	}
}
