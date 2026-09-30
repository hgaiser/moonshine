#include "inputtino/input.hpp"

#include <algorithm>
#include <cstring>
#include <inputtino/protected_types.hpp>
#include <inputtino/keyboard.hpp>
#include <thread>

namespace inputtino {

using namespace std::string_literals;

std::vector<std::string> Keyboard::get_nodes() const {
  std::vector<std::string> nodes;

  if (auto kb = _state->kb.get()) {
    nodes.emplace_back(libevdev_uinput_get_devnode(kb));
  }

  return nodes;
}

Result<libevdev_uinput_ptr> create_keyboard(const DeviceDefinition &device) {
  auto dev = libevdev_new();
  libevdev_uinput *uidev;

  libevdev_set_name(dev, device.name.c_str());
  libevdev_set_id_vendor(dev, device.vendor_id);
  libevdev_set_id_product(dev, device.product_id);
  libevdev_set_id_version(dev, device.version);
  libevdev_set_id_bustype(dev, BUS_USB);

  libevdev_enable_event_type(dev, EV_KEY);
  libevdev_enable_event_code(dev, EV_KEY, KEY_BACKSPACE, nullptr);

  for (auto ev : keyboard::key_mappings) {
    libevdev_enable_event_code(dev, EV_KEY, ev.second.linux_code, nullptr);
  }

  auto err = libevdev_uinput_create_from_device(dev, LIBEVDEV_UINPUT_OPEN_MANAGED, &uidev);
  libevdev_free(dev);
  if (err != 0) {
    return Error(strerror(-err));
  }

  return libevdev_uinput_ptr{uidev, ::libevdev_uinput_destroy};
}

static std::optional<keyboard::KEY_MAP> press_btn(libevdev_uinput *kb, short key_code) {
  auto search_key = keyboard::key_mappings.find(key_code);
  if (search_key != keyboard::key_mappings.end()) {
    auto mapped_key = search_key->second;

    libevdev_uinput_write_event(kb, EV_MSC, MSC_SCAN, mapped_key.scan_code);
    libevdev_uinput_write_event(kb, EV_KEY, mapped_key.linux_code, 1);
    libevdev_uinput_write_event(kb, EV_SYN, SYN_REPORT, 0);
    return mapped_key;
  }
  return {};
}

Keyboard::Keyboard() : _state(std::make_shared<KeyboardState>()) {}

Keyboard::~Keyboard() {
  if (_state) {
    _state->stop_repeat_thread = true;
    if (_state->repeat_press_t.joinable()) {
      _state->repeat_press_t.join();
    }
  }
}

Result<Keyboard> Keyboard::create(const DeviceDefinition &device, int millis_repress_key) {
  auto kb_el = create_keyboard(device);
  if (kb_el) {
    Keyboard kb;
    kb._state->kb = std::move(*kb_el);
    auto repeat_thread = std::thread([state = kb._state, millis_repress_key]() {
      while (!state->stop_repeat_thread) {
        std::this_thread::sleep_for(std::chrono::milliseconds(millis_repress_key));
        for (auto key : state->cur_press_keys) {
          if (auto keyboard = state->kb.get()) {
            press_btn(keyboard, key);
          }
        }
      }
    });
    kb._state->repeat_press_t = std::move(repeat_thread);
    kb._state->repeat_press_t.detach();
    return kb;
  } else {
    return Error(kb_el.getErrorMessage());
  }
}

void Keyboard::press(short key_code) {
  if (auto keyboard = _state->kb.get()) {
    if (auto key = press_btn(keyboard, key_code)) {
      _state->cur_press_keys.push_back(key_code);
    }
  }
}

void Keyboard::release(short key_code) {
  auto search_key = keyboard::key_mappings.find(key_code);
  if (search_key != keyboard::key_mappings.end()) {
    if (auto keyboard = _state->kb.get()) {
      auto mapped_key = search_key->second;
      this->_state->cur_press_keys.erase(
          std::remove(this->_state->cur_press_keys.begin(), this->_state->cur_press_keys.end(), key_code),
          this->_state->cur_press_keys.end());

      libevdev_uinput_write_event(keyboard, EV_MSC, MSC_SCAN, mapped_key.scan_code);
      libevdev_uinput_write_event(keyboard, EV_KEY, mapped_key.linux_code, 0);
      libevdev_uinput_write_event(keyboard, EV_SYN, SYN_REPORT, 0);
    }
  }
}

} // namespace inputtino