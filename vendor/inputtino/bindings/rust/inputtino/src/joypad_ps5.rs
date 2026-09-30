use inputtino_sys::{inputtino_joypad_ps5_place_finger, inputtino_joypad_ps5_release_finger};
use std::ffi::{c_int, c_void};
use std::path::PathBuf;

use crate::common::{get_nodes, make_device, DeviceDefinition};
use crate::sys::{
    inputtino_joypad_ps5_create, inputtino_joypad_ps5_destroy, inputtino_joypad_ps5_get_nodes,
    inputtino_joypad_ps5_set_on_led, inputtino_joypad_ps5_set_on_rumble,
    inputtino_joypad_ps5_set_pressed_buttons, inputtino_joypad_ps5_set_stick,
    inputtino_joypad_ps5_set_triggers, inputtino_joypad_ps5_set_motion,
    inputtino_joypad_ps5_set_battery, inputtino_joypad_ps5_set_on_trigger_effect,
};
use crate::{BatteryState, InputtinoError, JoypadMotionType, JoypadStickPosition};

/// Emulated PlayStation 5's DualSense joypad.
pub struct PS5Joypad {
    joypad: *mut crate::sys::InputtinoPS5Joypad,
    on_rumble_fn: *mut c_void,
    on_led_fn: *mut c_void,
    on_trigger_effect_fn: *mut c_void,
}

impl PS5Joypad {
    // TODO: Expose in C API and use the binding.
    pub const TOUCHPAD_WIDTH: i32 = 1920;
    pub const TOUCHPAD_HEIGHT: i32 = 1080;

    /// Create a new emulated PS5 DualSense device with the given device definition.
    ///
    /// # Examples
    ///
    /// ```
    /// let definition = inputtino::DeviceDefinition::new(
    ///     "Inputtino PS5 controller",
    ///     0x054C,
    ///     0x0CE6,
    ///     0x8111,
    ///     "00:11:22:33:44",
    ///     "00:11:22:33:44",
    /// );
    /// let device = inputtino::SwitchJoypad::new(&definition);
    /// ```
    pub fn new(device: &DeviceDefinition) -> Result<Self, InputtinoError> {
        make_device(inputtino_joypad_ps5_create, device).map(|joypad| PS5Joypad {
            joypad,
            on_rumble_fn: std::ptr::null_mut(),
            on_led_fn: std::ptr::null_mut(),
            on_trigger_effect_fn: std::ptr::null_mut(),
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
            inputtino_joypad_ps5_set_pressed_buttons(self.joypad, buttons);
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
            inputtino_joypad_ps5_set_triggers(self.joypad, left_trigger, right_trigger);
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
            inputtino_joypad_ps5_set_stick(self.joypad, stick_type, x, y);
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
            inputtino_joypad_ps5_set_on_rumble(
                self.joypad,
                Some(on_rumble_c_fn),
                self.on_rumble_fn,
            );
        }
    }

    /// Sets a callback to be called when this device receives a LED change event.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_on_led(|r, g, b| {
    ///     println!("Received LED event with colors R: {r}, G: {g}, B: {b}");
    /// });
    /// ```
    pub fn set_on_led(&mut self, on_led_fn: impl FnMut(i32, i32, i32) + 'static) {
        let on_led_fn = Box::new(LedFunction {
            on_led_fn: Box::new(on_led_fn),
        });
        self.on_led_fn = Box::into_raw(on_led_fn) as *mut c_void;
        unsafe {
            inputtino_joypad_ps5_set_on_led(self.joypad, Some(on_led_c_fn), self.on_led_fn);
        }
    }

    /// Sets a callback to be called when this device receives a trigger effect event.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_on_trigger_effect(|trigger_event_flags, type_left, type_right, left_effect, right_effect| {
    ///     println!(
    ///         "Received trigger effect event: trigger_event_flags: {trigger_event_flags}, type_left: {type_left}, type_right: {type_right}, \
    ///         left_effect: {:?}, right_effect: {:?}",
    ///         left_effect, right_effect
    ///     );
    /// });
    /// ```
    pub fn set_on_trigger_effect(&mut self, on_trigger_effect_fn: impl FnMut(u8, u8, u8, &[u8], &[u8]) + 'static) {
        let on_trigger_effect_fn = Box::new(TriggerEffectFunction {
            on_trigger_effect_fn: Box::new(on_trigger_effect_fn),
        });
        self.on_trigger_effect_fn = Box::into_raw(on_trigger_effect_fn) as *mut c_void;
        unsafe {
            inputtino_joypad_ps5_set_on_trigger_effect(self.joypad, Some(on_trigger_effect_c_fn), self.on_trigger_effect_fn);
        }
    }

    pub fn get_nodes(&self) -> Result<Vec<PathBuf>, InputtinoError> {
        get_nodes(inputtino_joypad_ps5_get_nodes, self.joypad)
    }

    /// Simulates placement of a finger on the touchpad.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.place_finger(0, 100, inputtino::PS5Joypad::TOUCHPAD_HEIGHT / 2);
    /// ```
    pub fn place_finger(&self, finger_id: u32, x: u16, y: u16) {
        unsafe {
            inputtino_joypad_ps5_place_finger(self.joypad, finger_id as i32, x, y);
        }
    }

    /// Simulates releasing of a finger from the touchpad.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.release_finger(0);
    /// ```
    pub fn release_finger(&self, finger_id: u32) {
        unsafe {
            inputtino_joypad_ps5_release_finger(self.joypad, finger_id as i32);
        }
    }

    /// Sets the state of the gyro or acceleration sensors.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_motion(JoypadMotionType::ACCELERATION, 0.0, 0.0, 1.0);
    /// ```
    pub fn set_motion(&self, motion_type: JoypadMotionType, x: f32, y: f32, z: f32) {
        unsafe {
            inputtino_joypad_ps5_set_motion(self.joypad, motion_type, x, y, z);
        }
    }

    /// Sets the state of the battery.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// device.set_battery(BatteryState::BATTERY_DISCHARGING, 55);
    /// ```
    pub fn set_battery(&self, battery_state: BatteryState, level: u8) {
        unsafe {
            inputtino_joypad_ps5_set_battery(self.joypad, battery_state, level as u16);
        }
    }
}

impl Drop for PS5Joypad {
    fn drop(&mut self) {
        unsafe {
            inputtino_joypad_ps5_destroy(self.joypad);
            if !self.on_rumble_fn.is_null() {
                drop(Box::from_raw(self.on_rumble_fn as *mut RumbleFunction));
            }
            if !self.on_led_fn.is_null() {
                drop(Box::from_raw(self.on_led_fn as *mut LedFunction));
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

struct LedFunction {
    on_led_fn: Box<dyn FnMut(i32, i32, i32)>,
}

unsafe extern "C" fn on_led_c_fn(
    r: c_int,
    g: c_int,
    b: c_int,
    user_data: *mut ::core::ffi::c_void,
) {
    let on_led_fn = user_data as *mut LedFunction;
    ((*on_led_fn).on_led_fn)(r, g, b);
}

struct TriggerEffectFunction {
    on_trigger_effect_fn: Box<dyn FnMut(u8, u8, u8, &[u8], &[u8])>,
}

unsafe extern "C" fn on_trigger_effect_c_fn(
    trigger_event_flags: u8,
    type_left: u8,
    type_right: u8,
    left: *const u8,
    right: *const u8,
    user_data: *mut ::core::ffi::c_void,
) {
    let on_trigger_effect_fn = user_data as *mut TriggerEffectFunction;
    let left_effect = std::slice::from_raw_parts(left, 10);
    let right_effect = std::slice::from_raw_parts(right, 10);
    ((*on_trigger_effect_fn).on_trigger_effect_fn)(
        trigger_event_flags,
        type_left,
        type_right,
        left_effect,
        right_effect,
    );
}

unsafe impl Send for PS5Joypad {}
