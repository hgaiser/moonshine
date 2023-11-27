use evdev::{uinput::{VirtualDeviceBuilder, VirtualDevice}, RelativeAxisType, AttributeSet, Key};
use tokio::sync::mpsc;

#[repr(u32)]
enum InputEventType {
	KeyDown = 0x00000003,
	KeyUp = 0x00000004,
	MouseMoveAbsolute = 0x00000005,
	// MouseMoveRelative = 0x00000006 (pre gen5)
	MouseMoveRelative = 0x00000007,
	MouseButtonDown = 0x00000008,
	MouseButtonUp = 0x00000009,
}

impl TryFrom<u32> for InputEventType {
	type Error = ();

	fn try_from(v: u32) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::KeyDown as u32 => Ok(Self::KeyDown),
			x if x == Self::KeyUp as u32 => Ok(Self::KeyUp),
			x if x == Self::MouseMoveAbsolute as u32 => Ok(Self::MouseMoveAbsolute),
			x if x == Self::MouseMoveRelative as u32 => Ok(Self::MouseMoveRelative),
			x if x == Self::MouseButtonDown as u32 => Ok(Self::MouseButtonDown),
			x if x == Self::MouseButtonUp as u32 => Ok(Self::MouseButtonUp),
			_ => {
				log::warn!("Unknown event type: {v}");
				Err(())
			},
		}
	}
}

#[derive(Debug)]
pub enum InputEvent {
	KeyDown(KeyEvent),
	KeyUp(KeyEvent),
	MouseMoveAbsolute(MouseMoveAbsolute),
	MouseMoveRelative(MouseMoveRelative),
	MouseButtonDown(MouseButton),
	MouseButtonUp(MouseButton),
}

#[derive(Debug)]
pub struct MouseMoveAbsolute {
	pub x: i16,
	pub y: i16,
	pub padding: u16,
	pub width: i16,
	pub height: i16,
}

#[derive(Debug)]
pub struct MouseMoveRelative {
	pub x: i16,
	pub y: i16,
}

// #[repr(u8)]
// pub enum KeyModifier {
// 	Shift = 0x01,
// 	Ctrl = 0x02,
// 	Alt = 0x04,
// 	Meta = 0x08,
// }

#[derive(Debug)]
pub struct KeyEvent {
	pub flags: u8,
	pub key: u16,
	pub modifiers: u8,
	pub padding: u16,
}

// impl Into<enigo::Key> for KeyEvent {
// 	fn into(self) -> enigo::Key {
// 		enigo::Key::
// 	}
// }

#[derive(Debug)]
#[repr(u8)]
pub enum MouseButton {
	Left = 0x01,
	Middle = 0x02,
	Right = 0x03,
	Side = 0x04,
	Extra = 0x05,
}

impl From<MouseButton> for Key {
	fn from(val: MouseButton) -> Self {
		match val {
			MouseButton::Left => Key::BTN_LEFT,
			MouseButton::Middle => Key::BTN_MIDDLE,
			MouseButton::Right => Key::BTN_RIGHT,
			MouseButton::Side => Key::BTN_SIDE,
			MouseButton::Extra => Key::BTN_EXTRA,
		}
	}
}

impl TryFrom<u8> for MouseButton {
	type Error = ();

	fn try_from(v: u8) -> Result<Self, Self::Error> {
		match v {
			x if x == Self::Left as u8 => Ok(Self::Left),
			x if x == Self::Middle as u8 => Ok(Self::Middle),
			x if x == Self::Right as u8 => Ok(Self::Right),
			x if x == Self::Side as u8 => Ok(Self::Side),
			x if x == Self::Extra as u8 => Ok(Self::Extra),
			_ => Err(()),
		}
	}
}

impl InputEvent {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < 4 {
			log::warn!("Expected control message to have at least 4 bytes, got {}", buffer.len());
			return Err(());
		}

		match u32::from_le_bytes(buffer[..4].try_into().unwrap()).try_into()? {
			InputEventType::KeyDown => {
				if buffer.len() != std::mem::size_of::<KeyEvent>() + 4 {
					log::warn!(
						"Expected KeyDown message to have exactly {} bytes, got {}",
						std::mem::size_of::<KeyEvent>() + 4,
						buffer.len()
					);
					return Err(());
				}

				Ok(InputEvent::KeyDown(KeyEvent {
					flags: buffer[4],
					key: u16::from_le_bytes(buffer[5..7].try_into().unwrap()) & 0x00FF,
					modifiers: buffer[7],
					padding: 0,
				}))
			},
			InputEventType::KeyUp => {
				if buffer.len() != std::mem::size_of::<KeyEvent>() + 4 {
					log::warn!(
						"Expected KeyUp message to have exactly {} bytes, got {}",
						std::mem::size_of::<KeyEvent>() + 4,
						buffer.len()
					);						return Err(());
				}

				Ok(InputEvent::KeyUp(KeyEvent {
					flags: buffer[4],
					key: u16::from_le_bytes(buffer[5..7].try_into().unwrap()) & 0x00FF,
					modifiers: buffer[7],
					padding: 0,
				}))
			},
			InputEventType::MouseMoveAbsolute => {
				if buffer.len() != std::mem::size_of::<MouseMoveAbsolute>() + 4 {
					log::warn!(
						"Expected absolute mouse movement message to have exactly {} bytes, got {}",
						std::mem::size_of::<MouseMoveAbsolute>() + 4,
						buffer.len()
					);
					return Err(());
				}

				// Moonlight seems to send { x, y, unused, width, height }.
				// We don't seem to need the width and height?
				Ok(InputEvent::MouseMoveAbsolute(MouseMoveAbsolute {
					x: i16::from_be_bytes(buffer[4..6].try_into().unwrap()),
					y: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
					padding: 0,
					width: i16::from_be_bytes(buffer[10..12].try_into().unwrap()),
					height: i16::from_be_bytes(buffer[12..14].try_into().unwrap()),
				}))
			},
			InputEventType::MouseMoveRelative => {
				// Expect 2 i16's.
				if buffer.len() != std::mem::size_of::<MouseMoveRelative>() + 4 {
					log::warn!("Expected relative mouse movement message to have exactly 8 bytes, got {}", buffer.len());
					return Err(());
				}

				Ok(InputEvent::MouseMoveRelative(MouseMoveRelative {
					x: i16::from_be_bytes(buffer[4..6].try_into().unwrap()),
					y: i16::from_be_bytes(buffer[6..8].try_into().unwrap()),
				}))
			},
			InputEventType::MouseButtonDown => {
				// Expect 1 u8.
				if buffer.len() != 1 + 4 {
					log::warn!("Expected mouse button down message to have exactly 5 bytes, got {}", buffer.len());
					return Err(());
				}

				Ok(InputEvent::MouseButtonDown(buffer[4].try_into()?))
			},
			InputEventType::MouseButtonUp => {
				// Expect 1 u8.
				if buffer.len() != 1 + 4 {
					log::warn!("Expected mouse button up message to have exactly 5 bytes, got {}", buffer.len());
					return Err(());
				}

				Ok(InputEvent::MouseButtonUp(buffer[4].try_into()?))
			},
		}
	}
}

pub struct InputHandler {
	command_tx: mpsc::Sender<InputEvent>,
}

impl InputHandler {
	pub fn new() -> Result<Self, ()> {
		let mouse = VirtualDeviceBuilder::new()
			.map_err(|e| log::error!("Failed to initiate virtual mouse: {e}"))?
			.name("moonshine-mouse")
			.with_relative_axes(&AttributeSet::from_iter([
				RelativeAxisType::REL_X,
				RelativeAxisType::REL_Y,
				RelativeAxisType::REL_WHEEL,
				RelativeAxisType::REL_HWHEEL,
			]))
			.map_err(|e| log::error!("Failed to enable relative axes for virtual mouse: {e}"))?
			// .with_absolute_axis(UinputAbsSetup::)
			// .map_err(|e| log::error!("Failed to enable absolute axes for virtual mouse: {e}"))?
			.with_keys(&AttributeSet::from_iter([
				Key::BTN_LEFT,
				Key::BTN_MIDDLE,
				Key::BTN_RIGHT,
				Key::BTN_FORWARD,
				Key::BTN_BACK,
			]))
			.map_err(|e| log::error!("Failed to add keys to virtual mouse: {e}"))?
			.build()
			.map_err(|e| log::error!("Failed to create virtual mouse: {e}"))?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = InputHandlerInner { mouse };
		tokio::spawn(inner.run(command_rx));

		Ok(Self { command_tx })
	}

	pub async fn handle_input(&self, event: InputEvent) -> Result<(), ()> {
		self.command_tx.send(event).await
			.map_err(|e| log::error!("Failed to send input event: {e}"))
	}
}

struct InputHandlerInner {
	mouse: VirtualDevice,
}

impl InputHandlerInner {
	pub async fn run(mut self, mut command_rx: mpsc::Receiver<InputEvent>) {
		while let Some(command) = command_rx.recv().await {
			match command {
				InputEvent::KeyDown(_event) => {},
				InputEvent::KeyUp(_event) => {},
				InputEvent::MouseMoveAbsolute(_event) => {},
				InputEvent::MouseMoveRelative(event) => {
					log::trace!("Moving mouse relative: {event:?}");
					let event_x = evdev::InputEvent::new_now(
						evdev::EventType::RELATIVE,
						RelativeAxisType::REL_X.0,
						event.x as i32,
					);
					let event_y = evdev::InputEvent::new_now(
						evdev::EventType::RELATIVE,
						RelativeAxisType::REL_Y.0,
						event.y as i32,
					);
					let _ = self.mouse.emit(&[event_x, event_y])
						.map_err(|e| log::error!("Failed to make relative mouse movement: {e}"));
				},
				InputEvent::MouseButtonDown(event) => {
					log::trace!("Pressing mouse button: {event:?}");
					let button_event = evdev::InputEvent::new_now(
						evdev::EventType::KEY,
						Into::<Key>::into(event).code(),
						1
					);
					let _ = self.mouse.emit(&[button_event])
						.map_err(|e| log::error!("Failed to press mouse button: {e}"));
				},
				InputEvent::MouseButtonUp(event) => {
					log::trace!("Releasing mouse button: {event:?}");
					let button_event = evdev::InputEvent::new_now(
						evdev::EventType::KEY,
						Into::<Key>::into(event).code(),
						0
					);
					let _ = self.mouse.emit(&[button_event])
						.map_err(|e| log::error!("Failed to release mouse button: {e}"));
				},
			}
		}

		log::debug!("Input handler closing.");
	}
}
