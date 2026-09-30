use crate::{JoypadStickPosition, PS5Joypad, SwitchJoypad, XboxOneJoypad};

/// A generic Joypad which exposes some common functionality of the underlying joypad.
pub enum Joypad {
    Switch(SwitchJoypad),
    PS5(PS5Joypad),
    XboxOne(XboxOneJoypad),
}

impl Joypad {
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
        match self {
            Joypad::Switch(joypad) => joypad.set_pressed(buttons),
            Joypad::PS5(joypad) => joypad.set_pressed(buttons),
            Joypad::XboxOne(joypad) => joypad.set_pressed(buttons),
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
        match self {
            Joypad::Switch(joypad) => joypad.set_triggers(left_trigger, right_trigger),
            Joypad::PS5(joypad) => joypad.set_triggers(left_trigger, right_trigger),
            Joypad::XboxOne(joypad) => joypad.set_triggers(left_trigger, right_trigger),
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
        match self {
            Joypad::Switch(joypad) => joypad.set_stick(stick_type, x, y),
            Joypad::PS5(joypad) => joypad.set_stick(stick_type, x, y),
            Joypad::XboxOne(joypad) => joypad.set_stick(stick_type, x, y),
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
        match self {
            Joypad::Switch(joypad) => joypad.set_on_rumble(on_rumble_fn),
            Joypad::PS5(joypad) => joypad.set_on_rumble(on_rumble_fn),
            Joypad::XboxOne(joypad) => joypad.set_on_rumble(on_rumble_fn),
        }
    }
}
