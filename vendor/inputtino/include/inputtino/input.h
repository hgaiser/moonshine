/**
 * C API for the inputtino library
 */
#ifndef __INPUTTINO_H__
#define __INPUTTINO_H__

#ifndef INPUTTINO_STATIC_DEFINE
#  include <inputtino/export_shared.h>
#else
#  include <inputtino/export_static.h>
#endif
#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct InputtinoDeviceDefinition {
  const char *name;
  unsigned short vendor_id;
  unsigned short product_id;
  unsigned short version;

  const char * device_phys;
  const char * device_uniq;
} InputtinoDeviceDefinition;

typedef void (*InputtinoErrorHandlerFn)(const char *error_message, void *user_data);

typedef struct InputtinoErrorHandler {
  InputtinoErrorHandlerFn eh;
  void *user_data;
} InputtinoErrorHandler;

/*
 * MOUSE
 */
struct InputtinoMouse;
typedef struct InputtinoMouse InputtinoMouse;

LIBINPUTTINO_EXPORT InputtinoMouse *inputtino_mouse_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_mouse_get_nodes(InputtinoMouse *mouse, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_mouse_move(InputtinoMouse *mouse, int delta_x, int delta_y);

LIBINPUTTINO_EXPORT void inputtino_mouse_move_absolute(InputtinoMouse *mouse, int x, int y, int screen_width, int screen_height);

enum INPUTTINO_MOUSE_BUTTON {
  LEFT,
  MIDDLE,
  RIGHT,
  SIDE,
  EXTRA
};

LIBINPUTTINO_EXPORT void inputtino_mouse_press_button(InputtinoMouse *mouse, enum INPUTTINO_MOUSE_BUTTON button);

LIBINPUTTINO_EXPORT void inputtino_mouse_release_button(InputtinoMouse *mouse, enum INPUTTINO_MOUSE_BUTTON button);

LIBINPUTTINO_EXPORT void inputtino_mouse_scroll_vertical(InputtinoMouse *mouse, int high_res_distance);

LIBINPUTTINO_EXPORT void inputtino_mouse_scroll_horizontal(InputtinoMouse *mouse, int high_res_distance);

LIBINPUTTINO_EXPORT void inputtino_mouse_destroy(InputtinoMouse *mouse);

/*
 * TRACKPAD
 */

struct InputtinoTrackpad;
typedef struct InputtinoTrackpad InputtinoTrackpad;

LIBINPUTTINO_EXPORT InputtinoTrackpad *inputtino_trackpad_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_trackpad_get_nodes(InputtinoTrackpad *trackpad, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_trackpad_place_finger(
    InputtinoTrackpad *trackpad, int finger_nr, float x, float y, float pressure, int orientation);

LIBINPUTTINO_EXPORT void inputtino_trackpad_release_finger(InputtinoTrackpad *trackpad, int finger_nr);

LIBINPUTTINO_EXPORT void inputtino_trackpad_set_left_btn(InputtinoTrackpad *trackpad, bool pressed);

LIBINPUTTINO_EXPORT void inputtino_trackpad_destroy(InputtinoTrackpad *trackpad);

/*
 * TOUCHSCREEN
 */

struct InputtinoTouchscreen;
typedef struct InputtinoTouchscreen InputtinoTouchscreen;

LIBINPUTTINO_EXPORT InputtinoTouchscreen *inputtino_touchscreen_create(const InputtinoDeviceDefinition *device,
                                                          const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_touchscreen_get_nodes(InputtinoTouchscreen *touchscreen, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_touchscreen_place_finger(
    InputtinoTouchscreen *touchscreen, int finger_nr, float x, float y, float pressure, int orientation);

LIBINPUTTINO_EXPORT void inputtino_touchscreen_release_finger(InputtinoTouchscreen *touchscreen, int finger_nr);

LIBINPUTTINO_EXPORT void inputtino_touchscreen_destroy(InputtinoTouchscreen *touchscreen);

/*
 * PENTABLET
 */

struct InputtinoPenTablet;
typedef struct InputtinoPenTablet InputtinoPenTablet;

LIBINPUTTINO_EXPORT InputtinoPenTablet *inputtino_pen_tablet_create(const InputtinoDeviceDefinition *device,
                                                       const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_pen_tablet_get_nodes(InputtinoPenTablet *pen_tablet, int *num_nodes);

enum INPUTTINO_PEN_TOOL_TYPE {
  PEN,
  ERASER,
  BRUSH,
  PENCIL,
  AIRBRUSH,
  TOUCH,
  SAME_AS_BEFORE /* Real devices don't need to report the tool type when it's still the same */
};

LIBINPUTTINO_EXPORT void inputtino_pen_tablet_place_tool(InputtinoPenTablet *pen_tablet,
                                            enum INPUTTINO_PEN_TOOL_TYPE tool_type,
                                            float x,
                                            float y,
                                            float pressure,
                                            float distance,
                                            float tilt_x,
                                            float tilt_y);

enum INPUTTINO_PEN_BTN_TYPE {
  PRIMARY,
  SECONDARY,
  TERTIARY
};

LIBINPUTTINO_EXPORT void
inputtino_pen_tablet_set_button(InputtinoPenTablet *pen_tablet, enum INPUTTINO_PEN_BTN_TYPE button, bool pressed);

LIBINPUTTINO_EXPORT void inputtino_pen_tablet_destroy(InputtinoPenTablet *pen_tablet);

/*
 * KEYBOARD
 */

struct InputtinoKeyboard;
typedef struct InputtinoKeyboard InputtinoKeyboard;

LIBINPUTTINO_EXPORT InputtinoKeyboard *inputtino_keyboard_create(const InputtinoDeviceDefinition *device, const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_keyboard_get_nodes(InputtinoKeyboard *keyboard, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_keyboard_press(InputtinoKeyboard *keyboard, short key_code);

LIBINPUTTINO_EXPORT void inputtino_keyboard_release(InputtinoKeyboard *keyboard, short key_code);

LIBINPUTTINO_EXPORT void inputtino_keyboard_destroy(InputtinoKeyboard *keyboard);

/*
 * Joypads
 */

enum INPUTTINO_JOYPAD_BTN : int {
  DPAD_UP = 0x0001,
  DPAD_DOWN = 0x0002,
  DPAD_LEFT = 0x0004,
  DPAD_RIGHT = 0x0008,

  START = 0x0010,
  BACK = 0x0020,
  HOME = 0x0400,

  LEFT_STICK = 0x0040,
  RIGHT_STICK = 0x0080,
  LEFT_BUTTON = 0x0100,
  RIGHT_BUTTON = 0x0200,

  SPECIAL_FLAG = 0x0400,
  PADDLE1_FLAG = 0x010000,
  PADDLE2_FLAG = 0x020000,
  PADDLE3_FLAG = 0x040000,
  PADDLE4_FLAG = 0x080000,
  TOUCHPAD_FLAG = 0x100000, // Touchpad buttons on Sony controllers
  MISC_FLAG = 0x200000,     // Share/Mic/Capture/Mute buttons on various controllers

  A = 0x1000,
  B = 0x2000,
  X = 0x4000,
  Y = 0x8000
};

enum INPUTTINO_JOYPAD_STICK_POSITION {
  RS,
  LS
};

typedef void (*InputtinoJoypadRumbleFn)(int low_freq, int high_freq, void *user_data);

/*
 * XOne Joypad
 */

struct InputtinoXOneJoypad;
typedef struct InputtinoXOneJoypad InputtinoXOneJoypad;

LIBINPUTTINO_EXPORT InputtinoXOneJoypad *inputtino_joypad_xone_create(const InputtinoDeviceDefinition *device,
                                                         const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_joypad_xone_get_nodes(InputtinoXOneJoypad *joypad, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_joypad_xone_set_pressed_buttons(InputtinoXOneJoypad *joypad, int newly_pressed);

LIBINPUTTINO_EXPORT void inputtino_joypad_xone_set_triggers(InputtinoXOneJoypad *joypad, short left_trigger, short right_trigger);

LIBINPUTTINO_EXPORT void inputtino_joypad_xone_set_stick(InputtinoXOneJoypad *joypad,
                                            enum INPUTTINO_JOYPAD_STICK_POSITION stick_type,
                                            short x,
                                            short y);

LIBINPUTTINO_EXPORT void
inputtino_joypad_xone_set_on_rumble(InputtinoXOneJoypad *joypad, InputtinoJoypadRumbleFn rumble_fn, void *user_data);

LIBINPUTTINO_EXPORT void inputtino_joypad_xone_destroy(InputtinoXOneJoypad *joypad);

/*
 * Nintendo Switch Joypad
 */

struct InputtinoSwitchJoypad;
typedef struct InputtinoSwitchJoypad InputtinoSwitchJoypad;

LIBINPUTTINO_EXPORT InputtinoSwitchJoypad *inputtino_joypad_switch_create(const InputtinoDeviceDefinition *device,
                                                             const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_joypad_switch_get_nodes(InputtinoSwitchJoypad *joypad, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_joypad_switch_set_pressed_buttons(InputtinoSwitchJoypad *joypad, int newly_pressed);

LIBINPUTTINO_EXPORT void
inputtino_joypad_switch_set_triggers(InputtinoSwitchJoypad *joypad, short left_trigger, short right_trigger);

LIBINPUTTINO_EXPORT void inputtino_joypad_switch_set_stick(InputtinoSwitchJoypad *joypad,
                                              enum INPUTTINO_JOYPAD_STICK_POSITION stick_type,
                                              short x,
                                              short y);

LIBINPUTTINO_EXPORT void inputtino_joypad_switch_set_on_rumble(InputtinoSwitchJoypad *joypad,
                                                  InputtinoJoypadRumbleFn rumble_fn,
                                                  void *user_data);

LIBINPUTTINO_EXPORT void inputtino_joypad_switch_destroy(InputtinoSwitchJoypad *joypad);

/*
 * PS5 Joypad
 */

struct InputtinoPS5Joypad;
typedef struct InputtinoPS5Joypad InputtinoPS5Joypad;

LIBINPUTTINO_EXPORT InputtinoPS5Joypad *inputtino_joypad_ps5_create(const InputtinoDeviceDefinition *device,
                                                       const InputtinoErrorHandler *eh);

LIBINPUTTINO_EXPORT char **inputtino_joypad_ps5_get_nodes(InputtinoPS5Joypad *joypad, int *num_nodes);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_set_pressed_buttons(InputtinoPS5Joypad *joypad, int newly_pressed);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_set_triggers(InputtinoPS5Joypad *joypad, short left_trigger, short right_trigger);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_set_stick(InputtinoPS5Joypad *joypad,
                                           enum INPUTTINO_JOYPAD_STICK_POSITION stick_type,
                                           short x,
                                           short y);

LIBINPUTTINO_EXPORT void
inputtino_joypad_ps5_set_on_rumble(InputtinoPS5Joypad *joypad, InputtinoJoypadRumbleFn rumble_fn, void *user_data);

LIBINPUTTINO_EXPORT void
inputtino_joypad_ps5_place_finger(InputtinoPS5Joypad *joypad, int finger_nr, unsigned short x, unsigned short y);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_release_finger(InputtinoPS5Joypad *joypad, int finger_nr);

enum INPUTTINO_JOYPAD_MOTION_TYPE : unsigned short {
  ACCELERATION = 0x01,
  GYROSCOPE = 0x02
};

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_set_motion(
    InputtinoPS5Joypad *joypad, enum INPUTTINO_JOYPAD_MOTION_TYPE motion_type, float x, float y, float z);

enum BATTERY_STATE : unsigned short {
  BATTERY_DISCHARGING = 0x0,
  BATTERY_CHARGHING = 0x1,
  BATTERY_FULL = 0x2,
  VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE = 0xA,
  TEMPERATURE_ERROR = 0xB,
  CHARGHING_ERROR = 0xF
};

LIBINPUTTINO_EXPORT void
inputtino_joypad_ps5_set_battery(InputtinoPS5Joypad *joypad, enum BATTERY_STATE battery_state, unsigned short level);

typedef void (*InputtinoJoypadLEDFn)(int r, int g, int b, void *user_data);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_set_on_led(InputtinoPS5Joypad *joypad, InputtinoJoypadLEDFn led_fn, void *user_data);

typedef void (*InputtinoJoypadTriggerFn)(uint8_t type_left,
                                         uint8_t event_flags, uint8_t type_right, const uint8_t *left, const uint8_t *right, void *user_data);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_set_on_trigger_effect(InputtinoPS5Joypad *joypad, InputtinoJoypadTriggerFn trigger_effect_fn, void *user_data);

LIBINPUTTINO_EXPORT void inputtino_joypad_ps5_destroy(InputtinoPS5Joypad *joypad);

#ifdef __cplusplus
} // extern "C"
#endif

#endif
