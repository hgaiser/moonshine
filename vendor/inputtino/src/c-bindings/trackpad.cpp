#include "helpers.hpp"
#include <inputtino/input.h>

InputtinoTrackpad *inputtino_trackpad_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh) {
  auto trackpad_ = inputtino::Trackpad::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (trackpad_) {
    return reinterpret_cast<InputtinoTrackpad *>(new inputtino::Trackpad(std::move(*trackpad_)));
  } else {
    eh->eh(trackpad_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_trackpad_get_nodes(InputtinoTrackpad *trackpad, int *num_nodes) {
  return c_get_nodes(trackpad, num_nodes);
}

void inputtino_trackpad_place_finger(
    InputtinoTrackpad *trackpad, int finger_nr, float x, float y, float pressure, int orientation) {
  if (trackpad) {
    reinterpret_cast<inputtino::Trackpad *>(trackpad)->place_finger(finger_nr, x, y, pressure, orientation);
  }
}

void inputtino_trackpad_release_finger(InputtinoTrackpad *trackpad, int finger_nr) {
  if (trackpad) {
    reinterpret_cast<inputtino::Trackpad *>(trackpad)->release_finger(finger_nr);
  }
}

void inputtino_trackpad_set_left_btn(InputtinoTrackpad *trackpad, bool pressed) {
  if (trackpad) {
    reinterpret_cast<inputtino::Trackpad *>(trackpad)->set_left_btn(pressed);
  }
}

void inputtino_trackpad_destroy(InputtinoTrackpad *trackpad) {
  if (trackpad) {
    inputtino::Trackpad *trackpad_ptr = reinterpret_cast<inputtino::Trackpad *>(trackpad);
    delete trackpad_ptr;
  }
}
