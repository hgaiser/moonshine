use std::path::PathBuf;

use crate::common::{get_nodes, make_device, DeviceDefinition};
use crate::sys::{
    inputtino_mouse_create, inputtino_mouse_destroy, inputtino_mouse_get_nodes,
    inputtino_mouse_move, inputtino_mouse_move_absolute, inputtino_mouse_press_button,
    inputtino_mouse_release_button, inputtino_mouse_scroll_horizontal,
    inputtino_mouse_scroll_vertical,
};

use crate::{InputtinoError, MouseButton};

pub struct Mouse {
    mouse: *mut crate::sys::InputtinoMouse,
}

impl Mouse {
    /// Create a new emulated mouse device with the given device definition.
    ///
    /// # Examples
    ///
    /// ```
    /// let definition = inputtino::DeviceDefinition::new(
    ///     "Inputtino Mouse",
    ///     0x0,
    ///     0x0,
    ///     0x0,
    ///     "00:11:22:33:44",
    ///     "00:11:22:33:44",
    /// );
    /// let device = inputtino::Mouse::new(&definition);
    /// ```
    pub fn new(device: &DeviceDefinition) -> Result<Self, InputtinoError> {
        make_device(inputtino_mouse_create, device).map(|mouse| Mouse { mouse })
    }

    pub fn get_nodes(&self) -> Result<Vec<PathBuf>, InputtinoError> {
        get_nodes(inputtino_mouse_get_nodes, self.mouse)
    }

    /// Move the mouse relative to its current position.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.move_rel(10, -10);
    /// ```
    pub fn move_rel(&self, x: i32, y: i32) {
        unsafe {
            inputtino_mouse_move(self.mouse, x, y);
        }
    }

    /// Move the mouse to absolute coordinates on the screen.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.move_abs(100, 100, 1920, 1080);
    /// ```
    pub fn move_abs(&self, x: i32, y: i32, screen_width: i32, screen_height: i32) {
        unsafe {
            inputtino_mouse_move_absolute(self.mouse, x, y, screen_width, screen_height);
        }
    }

    /// Simulate a button press.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.press_button(inputtino::MouseButton::LEFT);
    /// ```
    pub fn press_button(&self, button: MouseButton) {
        unsafe {
            inputtino_mouse_press_button(self.mouse, button);
        }
    }

    /// Simulate the release of a button.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.release_button(inputtino::MouseButton::LEFT);
    /// ```
    pub fn release_button(&self, button: MouseButton) {
        unsafe {
            inputtino_mouse_release_button(self.mouse, button);
        }
    }

    /// Simulate scrolling vertically for an amount of ticks.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.scroll_vertical(10);
    /// ```
    pub fn scroll_vertical(&self, amount: i32) {
        unsafe {
            inputtino_mouse_scroll_vertical(self.mouse, amount);
        }
    }

    /// Simulate scrolling horizontally for an amount of ticks.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.scroll_horizontal(-10);
    /// ```
    pub fn scroll_horizontal(&self, amount: i32) {
        unsafe {
            inputtino_mouse_scroll_horizontal(self.mouse, amount);
        }
    }
}

impl Drop for Mouse {
    fn drop(&mut self) {
        unsafe {
            inputtino_mouse_destroy(self.mouse);
        }
    }
}

unsafe impl Send for Mouse {}

#[cfg(test)]
mod tests {
    use crate::common::error_handler_fn;
    use std::ffi::CString;

    use super::*;

    #[test]
    fn test_inputtino_c_mouse() {
        let device_name = CString::new("Rusty Mouse").unwrap();
        let device_phys = CString::new("Rusty Mouse Phys").unwrap();
        let device_uniq = CString::new("Rusty Mouse Uniq").unwrap();
        let def = crate::sys::InputtinoDeviceDefinition {
            name: device_name.as_ptr(),
            vendor_id: 0,
            product_id: 0,
            version: 0,
            device_phys: device_phys.as_ptr(),
            device_uniq: device_uniq.as_ptr(),
        };
        // TODO: test this somehow
        let error_str = std::ptr::null_mut();
        let error_handler = crate::sys::InputtinoErrorHandler {
            eh: Some(error_handler_fn),
            user_data: error_str,
        };

        unsafe {
            let mouse = inputtino_mouse_create(&def, &error_handler);
            assert!(!mouse.is_null());

            let mut nodes_count: core::ffi::c_int = 0;
            let nodes = inputtino_mouse_get_nodes(mouse, &mut nodes_count);
            assert_eq!(nodes_count, 2);
            assert!(!nodes.is_null());
            // Check that the nodes start with /dev/input/event
            assert!(CString::from_raw(*nodes.offset(0))
                .to_str()
                .unwrap()
                .starts_with("/dev/input/event"));
            assert!(CString::from_raw(*nodes.offset(1))
                .to_str()
                .unwrap()
                .starts_with("/dev/input/event"));

            inputtino_mouse_destroy(mouse);
        }
    }
}
