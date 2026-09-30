#include "helpers.hpp"
#include <inputtino/input.h>

InputtinoPenTablet *inputtino_pen_tablet_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh) {
  auto pen_tablet_ = inputtino::PenTablet::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (pen_tablet_) {
    return reinterpret_cast<InputtinoPenTablet *>(new inputtino::PenTablet(std::move(*pen_tablet_)));
  } else {
    eh->eh(pen_tablet_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_pen_tablet_get_nodes(InputtinoPenTablet *pen_tablet, int *num_nodes) {
  return c_get_nodes(pen_tablet, num_nodes);
}

void inputtino_pen_tablet_place_tool(InputtinoPenTablet *pen_tablet,
                                     enum INPUTTINO_PEN_TOOL_TYPE tool_type,
                                     float x,
                                     float y,
                                     float pressure,
                                     float distance,
                                     float tilt_x,
                                     float tilt_y) {
  if (pen_tablet) {
    reinterpret_cast<inputtino::PenTablet *>(pen_tablet)
        ->place_tool(inputtino::PenTablet::TOOL_TYPE(tool_type), x, y, pressure, distance, tilt_x, tilt_y);
  }
}

void inputtino_pen_tablet_set_button(InputtinoPenTablet *pen_tablet, enum INPUTTINO_PEN_BTN_TYPE button, bool pressed) {
  if (pen_tablet) {
    reinterpret_cast<inputtino::PenTablet *>(pen_tablet)->set_btn(inputtino::PenTablet::BTN_TYPE(button), pressed);
  }
}

void inputtino_pen_tablet_destroy(InputtinoPenTablet *pen_tablet) {
  if(pen_tablet){
    inputtino::PenTablet *pen_tablet_ = reinterpret_cast<inputtino::PenTablet *>(pen_tablet);
    delete pen_tablet_;
  }
}
