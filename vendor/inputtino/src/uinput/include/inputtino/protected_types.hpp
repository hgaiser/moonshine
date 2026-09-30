#pragma once

#include <cstring>
#include <inputtino/input.hpp>
#include <iostream>
#include <libevdev/libevdev-uinput.h>
#include <libevdev/libevdev.h>
#include <thread>
#include <unistd.h>

namespace inputtino {

using libevdev_uinput_ptr = std::shared_ptr<libevdev_uinput>;
using libevdev_event_ptr = std::shared_ptr<input_event>;

/**
 * Given a uinput fd will read all queued events available at this time up to max_events
 */
static std::vector<libevdev_event_ptr> fetch_events(int uinput_fd, int max_events = 50) {
  std::vector<libevdev_event_ptr> events = {};
  struct input_event ev {};
  int ret, read_events = 0;
  while (read_events < max_events && (ret = read(uinput_fd, &ev, sizeof(ev))) == sizeof(ev)) {
    events.push_back(std::make_shared<input_event>(ev));
    read_events++;
  }
  if (ret < 0 && errno != EAGAIN) {
    std::cerr << "Failed reading uinput fd; ret=" << strerror(errno);
  } else if (ret > 0) {
    std::cerr << "Uinput incorrect read size of " << ret;
  }

  return events;
}

struct PenTabletState {
  libevdev_uinput_ptr pen_tablet = nullptr;
  PenTablet::TOOL_TYPE last_tool = PenTablet::SAME_AS_BEFORE;
};

struct BaseJoypadState {
  libevdev_uinput_ptr joy = nullptr;
  int currently_pressed_btns = 0;

  bool stop_listening_events = false;
  std::thread events_thread;

  std::optional<std::function<void(int low_freq, int high_freq)>> on_rumble = std::nullopt;
};

struct XboxOneJoypadState : BaseJoypadState {};
struct SwitchJoypadState : BaseJoypadState {};

struct KeyboardState {
  std::thread repeat_press_t;
  bool stop_repeat_thread = false;
  libevdev_uinput_ptr kb = nullptr;
  std::vector<short> cur_press_keys = {};
};

struct MouseState {
  libevdev_uinput_ptr mouse_rel = nullptr;
  libevdev_uinput_ptr mouse_abs = nullptr;
  bool absolute = false;
};

struct TouchScreenState {
  libevdev_uinput_ptr touch_screen = nullptr;

  /**
   * Multi touch protocol type B is stateful; see: https://docs.kernel.org/input/multi-touch-protocol.html
   * Slots are numbered starting from 0 up to <number of currently connected fingers> (max: 4)
   *
   * The way it works:
   * - first time a new finger_id arrives we'll assign it the lowest free slot
   *   and call MT_TRACKING_ID = <a monotonically increasing contact id>
   * - we can keep updating ABS_X and ABS_Y as long as the finger_id stays the same
   * - if we want to update a different finger we'll have to call ABS_MT_SLOT = slot_number
   * - when a finger is released we'll call ABS_MT_SLOT = slot_number && MT_TRACKING_ID = -1
   *
   * The other thing that needs to be kept in sync is the EV_KEY.
   * EX: enabling BTN_TOOL_DOUBLETAP will result in scrolling instead of moving the mouse
   */
  /* The MT_SLOT we are currently updating */
  int current_slot = -1;
  /* The MT_TRACKING_ID to assign to the next new contact */
  int next_tracking_id = 0;
  /* A map of finger_id to MT_SLOT */
  std::map<int /* finger_id */, int /* MT_SLOT */> fingers;
};

struct TrackpadState {
  libevdev_uinput_ptr trackpad = nullptr;

  /**
   * Multi touch protocol type B is stateful; see: https://docs.kernel.org/input/multi-touch-protocol.html
   * Slots are numbered starting from 0 up to <number of currently connected fingers> (max: 4)
   *
   * The way it works:
   * - first time a new finger_id arrives we'll assign it the lowest free slot
   *   and call MT_TRACKING_ID = <a monotonically increasing contact id>
   * - we can keep updating ABS_X and ABS_Y as long as the finger_id stays the same
   * - if we want to update a different finger we'll have to call ABS_MT_SLOT = slot_number
   * - when a finger is released we'll call ABS_MT_SLOT = slot_number && MT_TRACKING_ID = -1
   *
   * The other thing that needs to be kept in sync is the EV_KEY.
   * EX: enabling BTN_TOOL_DOUBLETAP will result in scrolling instead of moving the mouse
   */
  /* The MT_SLOT we are currently updating */
  int current_slot = -1;
  /* The MT_TRACKING_ID to assign to the next new contact */
  int next_tracking_id = 0;
  /* A map of finger_id to MT_SLOT */
  std::map<int /* finger_id */, int /* MT_SLOT */> fingers;
};

/* Returns the lowest MT slot not currently assigned to any finger */
static inline int first_free_mt_slot(const std::map<int, int> &fingers) {
  int slot = 0;
  auto is_taken = [&fingers](int candidate) {
    for (const auto &finger : fingers) {
      if (finger.second == candidate) {
        return true;
      }
    }
    return false;
  };
  while (is_taken(slot)) {
    slot++;
  }
  return slot;
}

} // namespace inputtino
