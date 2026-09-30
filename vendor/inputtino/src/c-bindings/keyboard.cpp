#include "helpers.hpp"
#include <inputtino/input.h>

InputtinoKeyboard *inputtino_keyboard_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh) {
  auto keyboard_ = inputtino::Keyboard::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (keyboard_) {
    return reinterpret_cast<InputtinoKeyboard *>(new inputtino::Keyboard(std::move(*keyboard_)));
  } else {
    eh->eh(keyboard_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_keyboard_get_nodes(InputtinoKeyboard *keyboard, int *num_nodes) {
  return c_get_nodes(keyboard, num_nodes);
}

void inputtino_keyboard_press(InputtinoKeyboard *keyboard, short key_code) {
  if (keyboard) {
    reinterpret_cast<inputtino::Keyboard *>(keyboard)->press(key_code);
  }
}

void inputtino_keyboard_release(InputtinoKeyboard *keyboard, short key_code) {
  if (keyboard) {
    reinterpret_cast<inputtino::Keyboard *>(keyboard)->release(key_code);
  }
}

void inputtino_keyboard_destroy(InputtinoKeyboard *keyboard) {
  if (keyboard) {
    auto ptr = reinterpret_cast<inputtino::Keyboard *>(keyboard);
    delete ptr;
  }
}
