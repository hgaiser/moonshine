use std::path::PathBuf;

use crate::common::{get_nodes, make_device, DeviceDefinition};
use crate::sys::{
    inputtino_keyboard_create, inputtino_keyboard_destroy, inputtino_keyboard_get_nodes,
    inputtino_keyboard_press, inputtino_keyboard_release,
};
use crate::InputtinoError;

/// Emulated keyboard device.
pub struct Keyboard {
    kb: *mut crate::sys::InputtinoKeyboard,
}

impl Keyboard {
    /// Create a new emulated keyboard device with the given device definition.
    ///
    /// # Examples
    ///
    /// ```
    /// let definition = inputtino::DeviceDefinition::new(
    ///     "Inputtino Keyboard",
    ///     0xBEEF,
    ///     0xDEAD,
    ///     0x111,
    ///     "00:11:22:33:44",
    ///     "00:11:22:33:44",
    /// );
    /// let device = inputtino::Keyboard::new(&definition);
    /// ```
    pub fn new(device: &DeviceDefinition) -> Result<Self, InputtinoError> {
        make_device(inputtino_keyboard_create, device).map(|kb| Keyboard { kb })
    }

    pub fn get_nodes(&self) -> Result<Vec<PathBuf>, InputtinoError> {
        get_nodes(inputtino_keyboard_get_nodes, self.kb)
    }

    /// Simulate a keypress on this keyboard device.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.press_key(0x41); // KEY_A
    /// ```
    pub fn press_key(&self, key: i16) {
        // TODO: Export key mapping in Rust.
        unsafe {
            inputtino_keyboard_press(self.kb, key);
        }
    }

    /// Simulate a release of a key from this keyboard device.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.release_key(0x41); // KEY_A
    /// ```
    pub fn release_key(&self, key: i16) {
        // TODO: Export key mapping in Rust.
        unsafe {
            inputtino_keyboard_release(self.kb, key);
        }
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        unsafe {
            inputtino_keyboard_destroy(self.kb);
        }
    }
}

unsafe impl Send for Keyboard {}
