#include "helpers.hpp"
#include <inputtino/input.h>

InputtinoTouchscreen *inputtino_touchscreen_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh) {
  auto touchscreen_ = inputtino::TouchScreen::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (touchscreen_) {
    return reinterpret_cast<InputtinoTouchscreen *>(new inputtino::TouchScreen(std::move(*touchscreen_)));
  } else {
    eh->eh(touchscreen_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_touchscreen_get_nodes(InputtinoTouchscreen *touchscreen, int *num_nodes) {
  return c_get_nodes(touchscreen, num_nodes);
}

void inputtino_touchscreen_place_finger(
    InputtinoTouchscreen *touchscreen, int finger_nr, float x, float y, float pressure, int orientation) {
  if (touchscreen) {
    reinterpret_cast<inputtino::TouchScreen *>(touchscreen)->place_finger(finger_nr, x, y, pressure, orientation);
  }
}

void inputtino_touchscreen_release_finger(InputtinoTouchscreen *touchscreen, int finger_nr) {
  if (touchscreen) {
    reinterpret_cast<inputtino::TouchScreen *>(touchscreen)->release_finger(finger_nr);
  }
}

void inputtino_touchscreen_destroy(InputtinoTouchscreen *touchscreen) {
  if (touchscreen) {
    inputtino::TouchScreen *touch_ptr = reinterpret_cast<inputtino::TouchScreen *>(touchscreen);
    delete touch_ptr;
  }
}
