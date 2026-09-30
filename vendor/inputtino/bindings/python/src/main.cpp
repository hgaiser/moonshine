#include <pybind11/functional.h>
#include <pybind11/pybind11.h>
#include <pybind11/stl.h>

#include <inputtino/input.hpp>

namespace py = pybind11;

PYBIND11_MODULE(_core, m) {
  m.doc() = "pybind inputtino module!";

  // DeviceDefinition
  py::class_<inputtino::DeviceDefinition>(m, "DeviceDefinition")
      .def(py::init<>())
      .def_readwrite("name", &inputtino::DeviceDefinition::name)
      .def_readwrite("vendor_id", &inputtino::DeviceDefinition::vendor_id)
      .def_readwrite("product_id", &inputtino::DeviceDefinition::product_id)
      .def_readwrite("version", &inputtino::DeviceDefinition::version)
      .def_readwrite("device_phys", &inputtino::DeviceDefinition::device_phys)
      .def_readwrite("device_uniq", &inputtino::DeviceDefinition::device_uniq);

  // VirtualDevice
  py::class_<inputtino::VirtualDevice>(m, "VirtualDevice")
      .def("get_nodes", &inputtino::VirtualDevice::get_nodes);

  // Mouse Button Enum
  py::enum_<inputtino::Mouse::MOUSE_BUTTON>(m, "MouseButton")
      .value("LEFT", inputtino::Mouse::MOUSE_BUTTON::LEFT)
      .value("MIDDLE", inputtino::Mouse::MOUSE_BUTTON::MIDDLE)
      .value("RIGHT", inputtino::Mouse::MOUSE_BUTTON::RIGHT)
      .value("SIDE", inputtino::Mouse::MOUSE_BUTTON::SIDE)
      .value("EXTRA", inputtino::Mouse::MOUSE_BUTTON::EXTRA);

  // Mouse
  py::class_<inputtino::Mouse, inputtino::VirtualDevice>(m, "Mouse")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::Mouse::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("move", &inputtino::Mouse::move)
      .def("move_abs", &inputtino::Mouse::move_abs)
      .def("press", &inputtino::Mouse::press)
      .def("release", &inputtino::Mouse::release)
      .def("vertical_scroll", &inputtino::Mouse::vertical_scroll)
      .def("horizontal_scroll", &inputtino::Mouse::horizontal_scroll);

  // Keyboard
  py::class_<inputtino::Keyboard, inputtino::VirtualDevice>(m, "Keyboard")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev,
                     int millis_repress_key = 50) {
                    auto result =
                        inputtino::Keyboard::create(dev, millis_repress_key);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("press", &inputtino::Keyboard::press)
      .def("release", &inputtino::Keyboard::release);

  // Trackpad
  py::class_<inputtino::Trackpad, inputtino::VirtualDevice>(m, "Trackpad")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::Trackpad::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("place_finger", &inputtino::Trackpad::place_finger)
      .def("release_finger", &inputtino::Trackpad::release_finger)
      .def("set_left_btn", &inputtino::Trackpad::set_left_btn);

  // Joypad Enums
  py::enum_<inputtino::Joypad::CONTROLLER_BTN>(m, "ControllerButton")
      .value("DPAD_UP", inputtino::Joypad::CONTROLLER_BTN::DPAD_UP)
      .value("DPAD_DOWN", inputtino::Joypad::CONTROLLER_BTN::DPAD_DOWN)
      .value("DPAD_LEFT", inputtino::Joypad::CONTROLLER_BTN::DPAD_LEFT)
      .value("DPAD_RIGHT", inputtino::Joypad::CONTROLLER_BTN::DPAD_RIGHT)
      .value("START", inputtino::Joypad::CONTROLLER_BTN::START)
      .value("BACK", inputtino::Joypad::CONTROLLER_BTN::BACK)
      .value("HOME", inputtino::Joypad::CONTROLLER_BTN::HOME)
      .value("LEFT_STICK", inputtino::Joypad::CONTROLLER_BTN::LEFT_STICK)
      .value("RIGHT_STICK", inputtino::Joypad::CONTROLLER_BTN::RIGHT_STICK)
      .value("LEFT_BUTTON", inputtino::Joypad::CONTROLLER_BTN::LEFT_BUTTON)
      .value("RIGHT_BUTTON", inputtino::Joypad::CONTROLLER_BTN::RIGHT_BUTTON)
      .value("SPECIAL_FLAG", inputtino::Joypad::CONTROLLER_BTN::SPECIAL_FLAG)
      .value("PADDLE1_FLAG", inputtino::Joypad::CONTROLLER_BTN::PADDLE1_FLAG)
      .value("PADDLE2_FLAG", inputtino::Joypad::CONTROLLER_BTN::PADDLE2_FLAG)
      .value("PADDLE3_FLAG", inputtino::Joypad::CONTROLLER_BTN::PADDLE3_FLAG)
      .value("PADDLE4_FLAG", inputtino::Joypad::CONTROLLER_BTN::PADDLE4_FLAG)
      .value("TOUCHPAD_FLAG", inputtino::Joypad::CONTROLLER_BTN::TOUCHPAD_FLAG)
      .value("MISC_FLAG", inputtino::Joypad::CONTROLLER_BTN::MISC_FLAG)
      .value("A", inputtino::Joypad::CONTROLLER_BTN::A)
      .value("B", inputtino::Joypad::CONTROLLER_BTN::B)
      .value("X", inputtino::Joypad::CONTROLLER_BTN::X)
      .value("Y", inputtino::Joypad::CONTROLLER_BTN::Y);

  py::enum_<inputtino::Joypad::STICK_POSITION>(m, "StickPosition")
      .value("RS", inputtino::Joypad::STICK_POSITION::RS)
      .value("LS", inputtino::Joypad::STICK_POSITION::LS);

  // Joypad base class
  py::class_<inputtino::Joypad, inputtino::VirtualDevice>(m, "Joypad")
      .def("set_pressed_buttons", &inputtino::Joypad::set_pressed_buttons)
      .def("set_triggers", &inputtino::Joypad::set_triggers)
      .def("set_stick", &inputtino::Joypad::set_stick);

  // XboxOneJoypad
  py::class_<inputtino::XboxOneJoypad, inputtino::Joypad>(m, "XboxOneJoypad")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::XboxOneJoypad::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("set_on_rumble", &inputtino::XboxOneJoypad::set_on_rumble);

  // SwitchJoypad
  py::class_<inputtino::SwitchJoypad, inputtino::Joypad>(m, "SwitchJoypad")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::SwitchJoypad::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("set_on_rumble", &inputtino::SwitchJoypad::set_on_rumble);

  // PS5 Joypad Enums
  py::enum_<inputtino::PS5Joypad::MOTION_TYPE>(m, "PS5MotionType")
      .value("ACCELERATION", inputtino::PS5Joypad::MOTION_TYPE::ACCELERATION)
      .value("GYROSCOPE", inputtino::PS5Joypad::MOTION_TYPE::GYROSCOPE);

  py::enum_<inputtino::PS5Joypad::BATTERY_STATE>(m, "PS5BatteryState")
      .value("BATTERY_DISCHARGING",
             inputtino::PS5Joypad::BATTERY_STATE::BATTERY_DISCHARGING)
      .value("BATTERY_CHARGING",
             inputtino::PS5Joypad::BATTERY_STATE::BATTERY_CHARGHING)
      .value("BATTERY_FULL", inputtino::PS5Joypad::BATTERY_STATE::BATTERY_FULL)
      .value("VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE",
             inputtino::PS5Joypad::BATTERY_STATE::
                 VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE)
      .value("TEMPERATURE_ERROR",
             inputtino::PS5Joypad::BATTERY_STATE::TEMPERATURE_ERROR)
      .value("CHARGING_ERROR",
             inputtino::PS5Joypad::BATTERY_STATE::CHARGHING_ERROR);

  // PS5 Joypad
  py::class_<inputtino::PS5Joypad, inputtino::Joypad>(m, "PS5Joypad")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::PS5Joypad::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("get_mac_address", &inputtino::PS5Joypad::get_mac_address)
      .def("get_sys_nodes", &inputtino::PS5Joypad::get_sys_nodes)
      .def("place_finger", &inputtino::PS5Joypad::place_finger)
      .def("release_finger", &inputtino::PS5Joypad::release_finger)
      .def("set_motion", &inputtino::PS5Joypad::set_motion)
      .def("set_battery", &inputtino::PS5Joypad::set_battery)
      .def("set_on_rumble", &inputtino::PS5Joypad::set_on_rumble)
      .def("set_on_led", &inputtino::PS5Joypad::set_on_led);

  // TouchScreen
  py::class_<inputtino::TouchScreen, inputtino::VirtualDevice>(m, "TouchScreen")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::TouchScreen::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("place_finger", &inputtino::TouchScreen::place_finger)
      .def("release_finger", &inputtino::TouchScreen::release_finger);

  // PenTablet tool and button enums
  py::enum_<inputtino::PenTablet::TOOL_TYPE>(m, "PenToolType")
      .value("PEN", inputtino::PenTablet::TOOL_TYPE::PEN)
      .value("ERASER", inputtino::PenTablet::TOOL_TYPE::ERASER)
      .value("BRUSH", inputtino::PenTablet::TOOL_TYPE::BRUSH)
      .value("PENCIL", inputtino::PenTablet::TOOL_TYPE::PENCIL)
      .value("AIRBRUSH", inputtino::PenTablet::TOOL_TYPE::AIRBRUSH)
      .value("TOUCH", inputtino::PenTablet::TOOL_TYPE::TOUCH)
      .value("SAME_AS_BEFORE", inputtino::PenTablet::TOOL_TYPE::SAME_AS_BEFORE);

  py::enum_<inputtino::PenTablet::BTN_TYPE>(m, "PenButtonType")
      .value("PRIMARY", inputtino::PenTablet::BTN_TYPE::PRIMARY)
      .value("SECONDARY", inputtino::PenTablet::BTN_TYPE::SECONDARY)
      .value("TERTIARY", inputtino::PenTablet::BTN_TYPE::TERTIARY);

  // PenTablet
  py::class_<inputtino::PenTablet, inputtino::VirtualDevice>(m, "PenTablet")
      .def_static("create",
                  [](const inputtino::DeviceDefinition& dev) {
                    auto result = inputtino::PenTablet::create(dev);
                    if (!result) {
                      throw std::runtime_error(result.getErrorMessage());
                    }
                    return std::move(*result);
                  })
      .def("place_tool", &inputtino::PenTablet::place_tool)
      .def("set_btn", &inputtino::PenTablet::set_btn);
}
