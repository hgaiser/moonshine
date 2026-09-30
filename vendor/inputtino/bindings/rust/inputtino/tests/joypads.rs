use inputtino::{DeviceDefinition, JoypadButton, JoypadStickPosition, SwitchJoypad};
use serial_test::serial;

#[test]
#[serial]
fn test_switch_joypad() {
    let device = DeviceDefinition::new(
        "Rusty Switch controller",
        // https://github.com/torvalds/linux/blob/276f98efb64a2c31c099465ace78d3054c662a0f/drivers/hid/hid-ids.h#L990
        0x057e,
        0x2009,
        0x8111,
        "00:11:22:33:44",
        "00:11:22:33:44",
    );
    let mut joypad = SwitchJoypad::new(&device).unwrap();

    let nodes = joypad.get_nodes().unwrap();
    {
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0]
            .to_str()
            .expect("valid utf-8")
            .starts_with("/dev/input/event"));
        assert!(nodes[1]
            .to_str()
            .expect("valid utf-8")
            .starts_with("/dev/input/js"));
    }

    // Sleep to let the system detect the anew device
    std::thread::sleep(std::time::Duration::from_millis(500));

    let sdl = sdl2::init().unwrap();
    let joystick_subsystem = sdl.game_controller().unwrap();

    assert_eq!(joystick_subsystem.num_joysticks().unwrap(), 1);

    let mut sdl_js = joystick_subsystem.open(0).unwrap();
    let mut event_pump = sdl.event_pump().unwrap();

    for event in event_pump.poll_iter() {
        match event {
            sdl2::event::Event::JoyDeviceAdded { which, .. } => {
                assert_eq!(which, 0);
            }
            sdl2::event::Event::ControllerDeviceAdded { which, .. } => {
                assert_eq!(which, 0);
            }
            _ => panic!("Unexpected event : {:?}", event),
        }
    }

    assert_eq!(sdl_js.name(), "Nintendo Switch Pro Controller");
    assert!(sdl_js.has_rumble());

    {
        joypad.set_pressed(JoypadButton::A as i32);
        for event in event_pump.wait_timeout_iter(50) {
            match event {
                sdl2::event::Event::ControllerButtonDown { button, .. } => {
                    assert_eq!(button, sdl2::controller::Button::A);
                }
                sdl2::event::Event::JoyButtonDown { button_idx, .. } => {
                    // Old joystick API uses the "xbox" layout for indexes
                    // A on a switch pad is on the same position as B on an xbox pad
                    assert_eq!(button_idx, sdl2::controller::Button::B as u8);
                    break;
                }
                _ => panic!("Unexpected event : {:?}", event),
            }
        }
    }

    {
        joypad.set_triggers(0, 0);
        for event in event_pump.wait_timeout_iter(50) {
            match event {
                sdl2::event::Event::ControllerAxisMotion { axis, value, .. } => {
                    assert_eq!(axis, sdl2::controller::Axis::TriggerLeft);
                    assert_eq!(value, 0);
                }
                sdl2::event::Event::JoyAxisMotion {
                    axis_idx, value, ..
                } => {
                    assert_eq!(axis_idx, sdl2::controller::Axis::TriggerLeft as u8);
                    assert_eq!(value, 0);
                    break;
                }
                sdl2::event::Event::Unknown { .. } => {}
                _ => panic!("Unexpected event : {:?}", event),
            }
        }
    }

    {
        joypad.set_stick(JoypadStickPosition::LS, 0, 0);
        for event in event_pump.wait_timeout_iter(50) {
            match event {
                sdl2::event::Event::ControllerAxisMotion { axis, value, .. } => {
                    assert_eq!(axis, sdl2::controller::Axis::LeftX);
                    assert_eq!(value, 0);
                }
                sdl2::event::Event::JoyAxisMotion {
                    axis_idx, value, ..
                } => {
                    assert_eq!(axis_idx, sdl2::controller::Axis::LeftX as u8);
                    assert_eq!(value, 0);
                    break;
                }
                _ => panic!("Unexpected event : {:?}", event),
            }
        }
    }

    {
        joypad.set_stick(JoypadStickPosition::RS, 0, 0);
        for event in event_pump.wait_timeout_iter(50) {
            match event {
                sdl2::event::Event::ControllerAxisMotion { axis, value, .. } => {
                    assert_eq!(axis, sdl2::controller::Axis::RightX);
                    assert_eq!(value, 0);
                }
                sdl2::event::Event::JoyAxisMotion {
                    axis_idx, value, ..
                } => {
                    assert_eq!(axis_idx, sdl2::controller::Axis::RightX as u8);
                    assert_eq!(value, 0);
                    break;
                }
                _ => panic!("Unexpected event : {:?}", event),
            }
        }
    }

    {
        joypad.set_on_rumble(move |left, right| {
            assert_eq!(left, 100);
            assert_eq!(right, 200);
        });
        let res = sdl_js.set_rumble(100, 200, 150);
        assert!(res.is_ok());
        std::thread::sleep(std::time::Duration::from_millis(25));
        joypad.set_on_rumble(move |left, right| {
            assert_eq!(left, 0);
            assert_eq!(right, 0);
        });
        std::thread::sleep(std::time::Duration::from_millis(125));
    }
}
