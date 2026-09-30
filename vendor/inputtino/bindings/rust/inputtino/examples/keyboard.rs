use inputtino::{DeviceDefinition, Keyboard};

fn main() {
    let definition = DeviceDefinition::new(
        "Rusty Keyboard",
        0xAB,
        0xCD,
        0xEF,
        "Rusty Keyboard Phys",
        "Rusty Keyboard Uniq",
    );
    let keyboard = Keyboard::new(&definition).expect("failed to create fake keyboard");

    // Type the word "inputtino".
    keyboard.press_key(0x49);
    keyboard.release_key(0x49);
    keyboard.press_key(0x4E);
    keyboard.release_key(0x4E);
    keyboard.press_key(0x50);
    keyboard.release_key(0x50);
    keyboard.press_key(0x55);
    keyboard.release_key(0x55);
    keyboard.press_key(0x54);
    keyboard.release_key(0x54);
    keyboard.press_key(0x54);
    keyboard.release_key(0x54);
    keyboard.press_key(0x49);
    keyboard.release_key(0x49);
    keyboard.press_key(0x4E);
    keyboard.release_key(0x4E);
    keyboard.press_key(0x4F);
    keyboard.release_key(0x4F);
}
