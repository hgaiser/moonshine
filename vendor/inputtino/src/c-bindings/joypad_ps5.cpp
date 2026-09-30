#include "helpers.hpp"
#include <inputtino/input.h>

InputtinoPS5Joypad *inputtino_joypad_ps5_create(const InputtinoDeviceDefinition *device,
                                                const InputtinoErrorHandler *eh) {
  auto joypad_ = inputtino::PS5Joypad::create({
      .name = device->name ? device->name : "Inputtino virtual device",
      .vendor_id = device->vendor_id,
      .product_id = device->product_id,
      .version = device->version,
      .device_phys = device->device_phys ? device->device_phys : "00:11:22:33:44:55",
      .device_uniq = device->device_uniq ? device->device_uniq : "00:11:22:33:44:55",
  });
  if (joypad_) {
    return reinterpret_cast<InputtinoPS5Joypad *>(new inputtino::PS5Joypad(std::move(*joypad_)));
  } else {
    eh->eh(joypad_.getErrorMessage().c_str(), eh->user_data);
    return nullptr;
  }
}

char **inputtino_joypad_ps5_get_nodes(InputtinoPS5Joypad *joypad, int *num_nodes) {
  return c_get_nodes(joypad, num_nodes);
}

void inputtino_joypad_ps5_set_pressed_buttons(InputtinoPS5Joypad *joypad, int newly_pressed) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_pressed_buttons(newly_pressed);
  }
}

void inputtino_joypad_ps5_set_triggers(InputtinoPS5Joypad *joypad, short left_trigger, short right_trigger) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_triggers(left_trigger, right_trigger);
  }
}

void inputtino_joypad_ps5_set_stick(InputtinoPS5Joypad *joypad,
                                    enum INPUTTINO_JOYPAD_STICK_POSITION stick_type,
                                    short x,
                                    short y) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_stick(inputtino::Joypad::STICK_POSITION(stick_type), x, y);
  }
}

void inputtino_joypad_ps5_set_on_rumble(InputtinoPS5Joypad *joypad,
                                        InputtinoJoypadRumbleFn rumble_fn,
                                        void *user_data) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_on_rumble(
        [user_data, rumble_fn](short left, short right) { rumble_fn(left, right, user_data); });
  }
}

void inputtino_joypad_ps5_place_finger(InputtinoPS5Joypad *joypad, int finger_nr, unsigned short x, unsigned short y) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->place_finger(finger_nr, x, y);
  }
}

void inputtino_joypad_ps5_release_finger(InputtinoPS5Joypad *joypad, int finger_nr) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->release_finger(finger_nr);
  }
}

void inputtino_joypad_ps5_set_motion(
    InputtinoPS5Joypad *joypad, enum INPUTTINO_JOYPAD_MOTION_TYPE motion_type, float x, float y, float z) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_motion(inputtino::PS5Joypad::MOTION_TYPE(motion_type),
                                                                 x,
                                                                 y,
                                                                 z);
  }
}

void inputtino_joypad_ps5_set_battery(InputtinoPS5Joypad *joypad,
                                      enum BATTERY_STATE battery_state,
                                      unsigned short level) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_battery(inputtino::PS5Joypad::BATTERY_STATE(battery_state),
                                                                  level);
  }
}

void inputtino_joypad_ps5_set_on_led(InputtinoPS5Joypad *joypad, InputtinoJoypadLEDFn led_fn, void *user_data) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_on_led(
        [user_data, led_fn](unsigned char r, unsigned char g, unsigned char b) { led_fn(r, g, b, user_data); });
  }
}

void inputtino_joypad_ps5_set_on_trigger_effect(InputtinoPS5Joypad *joypad,
                                                InputtinoJoypadTriggerFn trigger_effect_fn,
                                                void *user_data) {
  if (joypad) {
    reinterpret_cast<inputtino::PS5Joypad *>(joypad)->set_on_trigger_effect(
        [user_data, trigger_effect_fn](const inputtino::PS5Joypad::TriggerEffect &effect) {
          trigger_effect_fn(effect.event_flags,
                            effect.type_left,
                            effect.type_right,
                            effect.left.data(),
                            effect.right.data(),
                            user_data);
        });
  }
}

void inputtino_joypad_ps5_destroy(InputtinoPS5Joypad *joypad) {
  if (joypad) {
    auto joypad_ptr = reinterpret_cast<inputtino::PS5Joypad *>(joypad);
    delete joypad_ptr;
  }
}
