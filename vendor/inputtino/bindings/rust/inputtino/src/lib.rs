mod common;
pub use common::DeviceDefinition;

mod error;
pub use error::InputtinoError;

mod mouse;
pub use mouse::Mouse;

mod keyboard;
pub use keyboard::Keyboard;

mod joypad;
pub use joypad::Joypad;

mod joypad_ps5;
pub use joypad_ps5::PS5Joypad;

mod joypad_nintendo;
pub use joypad_nintendo::SwitchJoypad;

pub use joypad_xbox::XboxOneJoypad;
mod joypad_xbox;

pub use inputtino_sys::{
    INPUTTINO_JOYPAD_BTN as JoypadButton, INPUTTINO_JOYPAD_MOTION_TYPE as JoypadMotionType,
    INPUTTINO_JOYPAD_STICK_POSITION as JoypadStickPosition, INPUTTINO_MOUSE_BUTTON as MouseButton,
    BATTERY_STATE as BatteryState,
};

// Low level automatic c bindings.
pub use inputtino_sys as sys;
