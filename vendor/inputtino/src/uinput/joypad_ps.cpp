#include "joypad_utils.hpp"
#include <cstring>
#include <fcntl.h>
#include <inputtino/input.hpp>
#include <inputtino/protected_ps5_types.hpp>
#include <linux/input.h>
#include <optional>
#include <thread>

namespace inputtino {

std::vector<std::string> PS5Joypad::get_nodes() const {
  std::vector<std::string> nodes;

  if (auto joy = _state->joy.get()) {
    auto additional_nodes = get_child_dev_nodes(joy);
    nodes.insert(nodes.end(), additional_nodes.begin(), additional_nodes.end());
  }

  return nodes;
}

Result<libevdev_uinput_ptr> create_ps_controller(const DeviceDefinition &device) {
  libevdev *dev = libevdev_new();
  libevdev_uinput *uidev;

  libevdev_set_name(dev, device.name.c_str());
  libevdev_set_id_vendor(dev, device.vendor_id);
  libevdev_set_id_product(dev, device.product_id);
  libevdev_set_id_version(dev, device.version);
  libevdev_set_id_bustype(dev, BUS_USB);

  libevdev_enable_event_type(dev, EV_KEY);
  libevdev_enable_event_code(dev, EV_KEY, BTN_WEST, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_EAST, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_NORTH, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_SOUTH, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_THUMBL, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_THUMBR, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TR, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TL, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TR2, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TL2, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_SELECT, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_MODE, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_START, nullptr);

  libevdev_enable_event_type(dev, EV_ABS);

  input_absinfo dpad{0, -1, 1, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_HAT0Y, &dpad);
  libevdev_enable_event_code(dev, EV_ABS, ABS_HAT0X, &dpad);

  // see: https://github.com/games-on-whales/wolf/issues/56
  input_absinfo stick{0, -32768, 32767, 16, 128, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_X, &stick);
  libevdev_enable_event_code(dev, EV_ABS, ABS_RX, &stick);
  libevdev_enable_event_code(dev, EV_ABS, ABS_Y, &stick);
  libevdev_enable_event_code(dev, EV_ABS, ABS_RY, &stick);

  input_absinfo trigger{0, 0, 255, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_Z, &trigger);
  libevdev_enable_event_code(dev, EV_ABS, ABS_RZ, &trigger);

  libevdev_enable_event_type(dev, EV_FF);
  libevdev_enable_event_code(dev, EV_FF, FF_RUMBLE, nullptr);
  libevdev_enable_event_code(dev, EV_FF, FF_CONSTANT, nullptr);
  libevdev_enable_event_code(dev, EV_FF, FF_PERIODIC, nullptr);
  libevdev_enable_event_code(dev, EV_FF, FF_SINE, nullptr);
  libevdev_enable_event_code(dev, EV_FF, FF_RAMP, nullptr);
  libevdev_enable_event_code(dev, EV_FF, FF_GAIN, nullptr);

  auto err = libevdev_uinput_create_from_device(dev, LIBEVDEV_UINPUT_OPEN_MANAGED, &uidev);
  libevdev_free(dev);
  if (err != 0) {
    return Error(strerror(-err));
  }

  return libevdev_uinput_ptr{uidev, ::libevdev_uinput_destroy};
}

PS5Joypad::PS5Joypad(uint16_t vendor_id, std::array<unsigned char, 6> mac_address) : _state(std::make_shared<PS5JoypadState>()) {
}

PS5Joypad::~PS5Joypad() {
  if (_state) {
    _state->stop_listening_events = true;
    if (_state->joy.get() != nullptr && _state->events_thread.joinable()) {
      _state->events_thread.join();
    }
  }
}

Result<PS5Joypad> PS5Joypad::create(const DeviceDefinition &device) {
  auto joy_el = create_ps_controller(device);
  if (!joy_el) {
    return Error(joy_el.getErrorMessage());
  }

  PS5Joypad joypad(0);
  joypad._state->joy = std::move(*joy_el);

  auto event_thread = std::thread(event_listener, joypad._state);
  joypad._state->events_thread = std::move(event_thread);
  joypad._state->events_thread.detach();

  return joypad;
}

void PS5Joypad::set_pressed_buttons(unsigned int newly_pressed) {
  // Button flags that have been changed between current and prev
  auto bf_changed = newly_pressed ^ this->_state->currently_pressed_btns;
  // Button flags that are only part of the new packet
  auto bf_new = newly_pressed;
  if (auto controller = this->_state->joy.get()) {

    if (bf_changed) {
      if ((DPAD_UP | DPAD_DOWN) & bf_changed) {
        int button_state = bf_new & DPAD_UP ? -1 : (bf_new & DPAD_DOWN ? 1 : 0);

        libevdev_uinput_write_event(controller, EV_ABS, ABS_HAT0Y, button_state);
      }

      if ((DPAD_LEFT | DPAD_RIGHT) & bf_changed) {
        int button_state = bf_new & DPAD_LEFT ? -1 : (bf_new & DPAD_RIGHT ? 1 : 0);

        libevdev_uinput_write_event(controller, EV_ABS, ABS_HAT0X, button_state);
      }

      if (START & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_START, bf_new & START ? 1 : 0);
      if (BACK & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_SELECT, bf_new & BACK ? 1 : 0);
      if (LEFT_STICK & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_THUMBL, bf_new & LEFT_STICK ? 1 : 0);
      if (RIGHT_STICK & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_THUMBR, bf_new & RIGHT_STICK ? 1 : 0);
      if (LEFT_BUTTON & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_TL, bf_new & LEFT_BUTTON ? 1 : 0);
      if (RIGHT_BUTTON & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_TR, bf_new & RIGHT_BUTTON ? 1 : 0);
      if (HOME & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_MODE, bf_new & HOME ? 1 : 0);
      if (A & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_SOUTH, bf_new & A ? 1 : 0);
      if (B & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_EAST, bf_new & B ? 1 : 0);
      if (X & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_WEST, bf_new & X ? 1 : 0);
      if (Y & bf_changed)
        libevdev_uinput_write_event(controller, EV_KEY, BTN_NORTH, bf_new & Y ? 1 : 0);
    }

    libevdev_uinput_write_event(controller, EV_SYN, SYN_REPORT, 0);
  }
  this->_state->currently_pressed_btns = bf_new;
}

void PS5Joypad::set_stick(Joypad::STICK_POSITION stick_type, short x, short y) {
  if (auto controller = this->_state->joy.get()) {
    if (stick_type == LS) {
      libevdev_uinput_write_event(controller, EV_ABS, ABS_X, x);
      libevdev_uinput_write_event(controller, EV_ABS, ABS_Y, -y);
    } else {
      libevdev_uinput_write_event(controller, EV_ABS, ABS_RX, x);
      libevdev_uinput_write_event(controller, EV_ABS, ABS_RY, -y);
    }

    libevdev_uinput_write_event(controller, EV_SYN, SYN_REPORT, 0);
  }
}

void PS5Joypad::set_triggers(int16_t left, int16_t right) {
  if (auto controller = this->_state->joy.get()) {
    if (left > 0) {
      libevdev_uinput_write_event(controller, EV_ABS, ABS_Z, left);
    } else {
      libevdev_uinput_write_event(controller, EV_ABS, ABS_Z, left);
    }

    if (right > 0) {
      libevdev_uinput_write_event(controller, EV_ABS, ABS_RZ, right);
    } else {
      libevdev_uinput_write_event(controller, EV_ABS, ABS_RZ, right);
    }

    libevdev_uinput_write_event(controller, EV_SYN, SYN_REPORT, 0);
  }
}

void PS5Joypad::set_on_rumble(const std::function<void(int, int)> &callback) {
  this->_state->on_rumble = callback;
}

// Followings aren't supported when not using the UHID implementation
void PS5Joypad::place_finger(int finger_nr, uint16_t x, uint16_t y) {}
void PS5Joypad::release_finger(int finger_nr) {}
void PS5Joypad::set_motion(MOTION_TYPE type, float x, float y, float z) {}
void PS5Joypad::set_battery(BATTERY_STATE state, int percentage) {}
void PS5Joypad::set_on_led(const std::function<void(int r, int g, int b)> &callback) {}
void PS5Joypad::set_on_trigger_effect(const std::function<void(const TriggerEffect &)> &callback) {}

} // namespace inputtino
