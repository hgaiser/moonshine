#include "helpers.hpp"
#include <inputtino/input.h>
#include <inputtino/input.hpp>

InputtinoMouse *inputtino_mouse_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh) {
  auto mouse_ = inputtino::Mouse::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (mouse_) {
    return reinterpret_cast<InputtinoMouse *>(new inputtino::Mouse(std::move(*mouse_)));
  } else {
    eh->eh(mouse_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_mouse_get_nodes(InputtinoMouse *mouse, int *num_nodes) {
  return c_get_nodes(mouse, num_nodes);
}

void inputtino_mouse_move(InputtinoMouse *mouse, int delta_x, int delta_y) {
  if (mouse) {
    reinterpret_cast<inputtino::Mouse *>(mouse)->move(delta_x, delta_y);
  }
}

void inputtino_mouse_move_absolute(InputtinoMouse *mouse, int x, int y, int screen_width, int screen_height) {
  if (mouse) {
    reinterpret_cast<inputtino::Mouse *>(mouse)->move_abs(x, y, screen_width, screen_height);
  }
}

void inputtino_mouse_press_button(InputtinoMouse *mouse, enum INPUTTINO_MOUSE_BUTTON button) {
  if (mouse) {
    reinterpret_cast<inputtino::Mouse *>(mouse)->press(inputtino::Mouse::MOUSE_BUTTON(button));
  }
}

void inputtino_mouse_release_button(InputtinoMouse *mouse, enum INPUTTINO_MOUSE_BUTTON button) {
  if (mouse) {
    reinterpret_cast<inputtino::Mouse *>(mouse)->release(inputtino::Mouse::MOUSE_BUTTON(button));
  }
}

void inputtino_mouse_scroll_vertical(InputtinoMouse *mouse, int high_res_distance) {
  if (mouse) {
    reinterpret_cast<inputtino::Mouse *>(mouse)->vertical_scroll(high_res_distance);
  }
}

void inputtino_mouse_scroll_horizontal(InputtinoMouse *mouse, int high_res_distance) {
  if (mouse) {
    reinterpret_cast<inputtino::Mouse *>(mouse)->horizontal_scroll(high_res_distance);
  }
}

void inputtino_mouse_destroy(InputtinoMouse *mouse) {
  if (mouse) {
    inputtino::Mouse *mouse_ptr = reinterpret_cast<inputtino::Mouse *>(mouse);
    delete mouse_ptr;
  }
}
