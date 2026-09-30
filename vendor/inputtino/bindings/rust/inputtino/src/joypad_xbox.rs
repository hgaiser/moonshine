use std::ffi::{c_int, c_void};
use std::path::PathBuf;

use crate::common::{get_nodes, make_device, DeviceDefinition};
use crate::sys::{
    inputtino_joypad_xone_create, inputtino_joypad_xone_destroy, inputtino_joypad_xone_get_nodes,
    inputtino_joypad_xone_set_on_rumble, inputtino_joypad_xone_set_pressed_buttons,
    inputtino_joypad_xone_set_stick, inputtino_joypad_xone_set_triggers,
};
use crate::{InputtinoError, JoypadStickPosition};

/// Emulated XBox One joypad.
pub struct XboxOneJoypad {
    joypad: *mut crate::sys::InputtinoXOneJoypad,
    on_rumble_fn: *mut c_void,
}

impl XboxOneJoypad {
    /// Create a new emulated XBox One joypad device with the given device definition.
    ///
    /// # Examples
    ///
    /// ```
    /// let definition = inputtino::DeviceDefinition::new(
    ///     "Inputtino XBox One controller",
    ///     0x045E,
    ///     0x02DD,
    ///     0x0100,
    ///     "00:11:22:33:44",
    ///     "00:11:22:33:44",
    /// );
    /// let device = inputtino::SwitchJoypad::new(&definition);
    /// ```
    pub fn new(device: &DeviceDefinition) -> Result<Self, InputtinoError> {
        make_device(inputtino_joypad_xone_create, device).map(|joypad| XboxOneJoypad {
            joypad,
            on_rumble_fn: std::ptr::null_mut(),
        })
    }

    /// Set the state of all buttons.
    ///
    /// Any buttons that are not set are released if they were set before.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_pressed(inputtino::JoypadButton::A | inputtino::JoypadButton::B);
    /// ```
    pub fn set_pressed(&self, buttons: i32) {
        unsafe {
            inputtino_joypad_xone_set_pressed_buttons(self.joypad, buttons);
        }
    }

    /// Set the state of the triggers.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_triggers(0, -i16::MAX);
    /// ```
    pub fn set_triggers(&self, left_trigger: i16, right_trigger: i16) {
        unsafe {
            inputtino_joypad_xone_set_triggers(self.joypad, left_trigger, right_trigger);
        }
    }

    /// Set the state of the joysticks.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_stick(inputtino::JoypadStickPosition::LS, 0, -i16::MAX);
    /// ```
    pub fn set_stick(&self, stick_type: JoypadStickPosition, x: i16, y: i16) {
        unsafe {
            inputtino_joypad_xone_set_stick(self.joypad, stick_type, x, y);
        }
    }

    /// Sets a callback to be called when this device receives a rumble event.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_on_rumble(|low, high| {
    ///     println!("Received rumble event with frequencies low: {low}, high: {high}");
    /// });
    /// ```
    pub fn set_on_rumble(&mut self, on_rumble_fn: impl FnMut(i32, i32) + 'static) {
        let on_rumble_fn = Box::new(RumbleFunction {
            on_rumble_fn: Box::new(on_rumble_fn),
        });
        self.on_rumble_fn = Box::into_raw(on_rumble_fn) as *mut c_void;
        unsafe {
            inputtino_joypad_xone_set_on_rumble(
                self.joypad,
                Some(on_rumble_c_fn),
                self.on_rumble_fn,
            );
        }
    }

    pub fn get_nodes(&self) -> Result<Vec<PathBuf>, InputtinoError> {
        get_nodes(inputtino_joypad_xone_get_nodes, self.joypad)
    }
}

impl Drop for XboxOneJoypad {
    fn drop(&mut self) {
        unsafe {
            inputtino_joypad_xone_destroy(self.joypad);
            if !self.on_rumble_fn.is_null() {
                drop(Box::from_raw(self.on_rumble_fn as *mut RumbleFunction));
            }
        }
    }
}

struct RumbleFunction {
    on_rumble_fn: Box<dyn FnMut(i32, i32)>,
}

unsafe extern "C" fn on_rumble_c_fn(
    left_motor: c_int,
    right_motor: c_int,
    user_data: *mut ::core::ffi::c_void,
) {
    let on_rumble_fn = user_data as *mut RumbleFunction;
    ((*on_rumble_fn).on_rumble_fn)(left_motor, right_motor);
}

unsafe impl Send for XboxOneJoypad {}
