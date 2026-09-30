use inputtino::{DeviceDefinition, Mouse, MouseButton};

fn main() {
    let definition = DeviceDefinition::new(
        "Rusty Mouse",
        0xAB,
        0xCD,
        0xEF,
        "Rusty Mouse Phys",
        "Rusty Mouse Uniq",
    );
    let mouse = Mouse::new(&definition).expect("failed to create fake mouse");

    // Move down-right by 100px and right click.
    mouse.move_rel(100, 100);
    mouse.press_button(MouseButton::RIGHT);
}
