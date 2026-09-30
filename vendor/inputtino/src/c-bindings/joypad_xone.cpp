#include "helpers.hpp"
#include <inputtino/input.h>

InputtinoXOneJoypad *inputtino_joypad_xone_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh) {
  auto joypad_ = inputtino::XboxOneJoypad::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (joypad_) {
    return reinterpret_cast<InputtinoXOneJoypad *>(new inputtino::XboxOneJoypad(std::move(*joypad_)));
  } else {
    eh->eh(joypad_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_joypad_xone_get_nodes(InputtinoXOneJoypad *joypad, int *num_nodes) {
  return c_get_nodes(joypad, num_nodes);
}

void inputtino_joypad_xone_set_pressed_buttons(InputtinoXOneJoypad *joypad, int newly_pressed) {
  if (joypad) {
    reinterpret_cast<inputtino::XboxOneJoypad *>(joypad)->set_pressed_buttons(newly_pressed);
  }
}

void inputtino_joypad_xone_set_triggers(InputtinoXOneJoypad *joypad, short left_trigger, short right_trigger) {
  if (joypad) {
    reinterpret_cast<inputtino::XboxOneJoypad *>(joypad)->set_triggers(left_trigger, right_trigger);
  }
}

void inputtino_joypad_xone_set_stick(InputtinoXOneJoypad *joypad,
                                     enum INPUTTINO_JOYPAD_STICK_POSITION stick_type,
                                     short x,
                                     short y) {
  if (joypad) {
    reinterpret_cast<inputtino::XboxOneJoypad *>(joypad)->set_stick(inputtino::Joypad::STICK_POSITION(stick_type),
                                                                    x,
                                                                    y);
  }
}

void inputtino_joypad_xone_set_on_rumble(InputtinoXOneJoypad *joypad,
                                         InputtinoJoypadRumbleFn rumble_fn,
                                         void *user_data) {
  if (joypad) {
    reinterpret_cast<inputtino::XboxOneJoypad *>(joypad)->set_on_rumble(
        [user_data, rumble_fn](short left, short right) { rumble_fn(left, right, user_data); });
  }
}

void inputtino_joypad_xone_destroy(InputtinoXOneJoypad *joypad) {
  if (joypad) {
    auto joypad_ptr = reinterpret_cast<inputtino::XboxOneJoypad *>(joypad);
    delete joypad_ptr;
  }
}
