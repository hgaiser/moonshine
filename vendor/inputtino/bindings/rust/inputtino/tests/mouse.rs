#[macro_use]
extern crate approx;

use input::event::pointer::{Axis, ButtonState};
use input::event::{DeviceEvent, PointerEvent};
use input::{Event, Libinput};
use inputtino::{DeviceDefinition, Mouse, MouseButton};

mod common;
use crate::common::{NixInterface, SyncEvent};

#[test]
fn test_inputtino_mouse() {
    let device = DeviceDefinition::new(
        "Rusty Mouse",
        0xAB,
        0xCD,
        0xEF,
        "Rusty Mouse Phys",
        "Rusty Mouse Uniq",
    );
    let mouse = Mouse::new(&device).unwrap();
    let nodes = mouse.get_nodes().unwrap();
    {
        assert_eq!(nodes.len(), 2);

        // Check that the nodes start with /dev/input/event
        assert!(nodes[0].to_str().unwrap().starts_with("/dev/input/event"));
        assert!(nodes[1].to_str().unwrap().starts_with("/dev/input/event"));
    }

    let mut input = Libinput::new_from_path(NixInterface);
    let dev_rel = input
        .path_add_device(nodes[0].to_str().expect("valid utf-8"))
        .expect("to get the device");
    let dev_abs = input
        .path_add_device(nodes[1].to_str().expect("valid utf-8"))
        .expect("to get the device");

    {
        assert_eq!(dev_rel.name(), "Rusty Mouse");
        assert_eq!(dev_abs.name(), "Rusty Mouse (absolute)");
        assert_eq!(dev_rel.id_vendor(), 0xAB);
        assert_eq!(dev_abs.id_vendor(), 0xAB);
        assert_eq!(dev_rel.id_product(), 0xCD);
        assert_eq!(dev_abs.id_product(), 0xCD);
        for event in &mut input {
            assert!(matches!(event, Event::Device(DeviceEvent::Added(_))));
        }
    }

    {
        // Test mouse relative motion
        mouse.move_rel(10, 20);

        let ev = input.wait_next_event().unwrap();
        assert!(matches!(ev, Event::Pointer(PointerEvent::Motion(_))));
        match ev {
            Event::Pointer(PointerEvent::Motion(ev)) => {
                assert_eq!(ev.dx_unaccelerated(), 10.0);
                assert_eq!(ev.dy_unaccelerated(), 20.0);
            }
            _ => unreachable!(),
        }
    }

    {
        // Test mouse absolute motion
        mouse.move_abs(100, 200, 1920, 1080);

        let ev = input.wait_next_event().unwrap();
        assert!(matches!(
            ev,
            Event::Pointer(PointerEvent::MotionAbsolute(_))
        ));
        match ev {
            Event::Pointer(PointerEvent::MotionAbsolute(ev)) => {
                assert_relative_eq!(ev.absolute_x_transformed(1920), 100.0, max_relative = 0.1);
                assert_relative_eq!(ev.absolute_y_transformed(1080), 200.0, max_relative = 0.1);
            }
            _ => unreachable!(),
        }
    }

    {
        // Test mouse button press
        mouse.press_button(MouseButton::LEFT);

        let ev = input.wait_next_event().unwrap();
        assert!(matches!(ev, Event::Pointer(PointerEvent::Button(_))));
        match ev {
            Event::Pointer(PointerEvent::Button(ev)) => {
                assert_eq!(ev.button(), 272);
                assert_eq!(ev.button_state(), ButtonState::Pressed);
            }
            _ => unreachable!(),
        }
    }

    {
        mouse.release_button(MouseButton::LEFT);

        let ev = input.wait_next_event().unwrap();
        assert!(matches!(ev, Event::Pointer(PointerEvent::Button(_))));
        match ev {
            Event::Pointer(PointerEvent::Button(ev)) => {
                assert_eq!(ev.button(), 272);
                assert_eq!(ev.button_state(), ButtonState::Released);
            }
            _ => unreachable!(),
        }
    }

    {
        mouse.scroll_vertical(100);

        let ev = input.wait_next_event().unwrap();
        assert!(matches!(ev, Event::Pointer(PointerEvent::ScrollWheel(_))));
        match ev {
            Event::Pointer(PointerEvent::ScrollWheel(ev)) => {
                assert_eq!(ev.scroll_value_v120(Axis::Vertical), -100.0);
            }
            _ => unreachable!(),
        }
    }

    {
        mouse.scroll_horizontal(100);

        let ev = input.wait_next_event().unwrap();
        assert!(matches!(ev, Event::Pointer(PointerEvent::ScrollWheel(_))));
        match ev {
            Event::Pointer(PointerEvent::ScrollWheel(ev)) => {
                assert_eq!(ev.scroll_value_v120(Axis::Horizontal), 100.0);
            }
            _ => unreachable!(),
        }
    }
}
