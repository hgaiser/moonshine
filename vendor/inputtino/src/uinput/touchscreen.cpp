#include "inputtino/input.hpp"
#include <algorithm>
#include <cmath>
#include <cstring>
#include <inputtino/protected_types.hpp>

namespace inputtino {

std::vector<std::string> TouchScreen::get_nodes() const {
  std::vector<std::string> nodes;

  if (auto kb = _state->touch_screen.get()) {
    nodes.emplace_back(libevdev_uinput_get_devnode(kb));
  }

  return nodes;
}

static constexpr int TOUCH_MAX_X = 19200;
static constexpr int TOUCH_MAX_Y = 10800;
static constexpr int NUM_FINGERS = 16;
static constexpr int PRESSURE_MAX = 253;

Result<libevdev_uinput_ptr> create_touch_screen(const DeviceDefinition &device) {
  libevdev *dev = libevdev_new();
  libevdev_uinput *uidev;

  libevdev_set_name(dev, device.name.c_str());
  libevdev_set_id_vendor(dev, device.vendor_id);
  libevdev_set_id_product(dev, device.product_id);
  libevdev_set_id_version(dev, device.version);
  libevdev_set_id_bustype(dev, BUS_USB);

  libevdev_enable_event_type(dev, EV_KEY);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOUCH, nullptr);

  libevdev_enable_event_type(dev, EV_ABS);
  input_absinfo mt_slot{0, 0, NUM_FINGERS - 1, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_SLOT, &mt_slot);

  input_absinfo abs_x{0, 0, TOUCH_MAX_X, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_X, &abs_x);
  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_POSITION_X, &abs_x);

  input_absinfo abs_y{0, 0, TOUCH_MAX_Y, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_Y, &abs_y);
  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_POSITION_Y, &abs_y);

  input_absinfo tracking{0, 0, 65535, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_TRACKING_ID, &tracking);

  input_absinfo abs_pressure{0, 0, PRESSURE_MAX, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_PRESSURE, &abs_pressure);
  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_PRESSURE, &abs_pressure);
  // TODO:
  //  input_absinfo touch{0, 0, TOUCH_MAX, 4, 0, 0};
  //  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_TOUCH_MAJOR, &touch);
  //  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_TOUCH_MINOR, &touch);
  input_absinfo orientation{0, -90, 90, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_MT_ORIENTATION, &orientation);

  // https://docs.kernel.org/input/event-codes.html#touchscreens
  libevdev_enable_property(dev, INPUT_PROP_DIRECT);

  auto err = libevdev_uinput_create_from_device(dev, LIBEVDEV_UINPUT_OPEN_MANAGED, &uidev);
  libevdev_free(dev);
  if (err != 0) {
    return Error(strerror(-err));
  }

  return libevdev_uinput_ptr{uidev, ::libevdev_uinput_destroy};
}

TouchScreen::TouchScreen() : _state(std::make_shared<TouchScreenState>()) {}

TouchScreen::~TouchScreen() {
  if (_state) {
    _state.reset();
  }
}

Result<TouchScreen> TouchScreen::create(const DeviceDefinition &device) {
  auto touch_screen = create_touch_screen(device);
  if (touch_screen) {
    TouchScreen ts;
    ts._state->touch_screen = std::move(*touch_screen);
    return ts;
  } else {
    return Error(touch_screen.getErrorMessage());
  }
}

void TouchScreen::place_finger(int finger_nr, float x, float y, float pressure, int orientation) {
  if (auto ts = this->_state->touch_screen.get()) {
    int scaled_x = (int)std::lround(TOUCH_MAX_X * x);
    int scaled_y = (int)std::lround(TOUCH_MAX_Y * y);
    int scaled_orientation = std::clamp(orientation, -90, 90);

    if (_state->fingers.find(finger_nr) == _state->fingers.end()) {
      // Wow, a wild finger appeared!
      // Pick the lowest free slot: fingers.size() + 1 would collide with a slot that's
      // still in use after another finger has been released (ex: two fingers down,
      // first one lifted and placed again)
      auto finger_slot = first_free_mt_slot(_state->fingers);
      auto tracking_id = _state->next_tracking_id;
      _state->next_tracking_id = (_state->next_tracking_id + 1) % 65536;
      _state->fingers[finger_nr] = finger_slot;
      libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_SLOT, finger_slot);
      _state->current_slot = finger_slot;
      libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_TRACKING_ID, tracking_id);
    } else {
      // I already know this finger, let's check the slot
      auto finger_slot = _state->fingers[finger_nr];
      if (_state->current_slot != finger_slot) {
        libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_SLOT, finger_slot);
        _state->current_slot = finger_slot;
      }
    }

    libevdev_uinput_write_event(ts, EV_ABS, ABS_X, scaled_x);
    libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_POSITION_X, scaled_x);
    libevdev_uinput_write_event(ts, EV_ABS, ABS_Y, scaled_y);
    libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_POSITION_Y, scaled_y);
    libevdev_uinput_write_event(ts, EV_ABS, ABS_PRESSURE, (int)std::lround(pressure * PRESSURE_MAX));
    libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_PRESSURE, (int)std::lround(pressure * PRESSURE_MAX));
    libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_ORIENTATION, scaled_orientation);

    libevdev_uinput_write_event(ts, EV_SYN, SYN_REPORT, 0);
  }
}

void TouchScreen::release_finger(int finger_nr) {
  if (auto ts = this->_state->touch_screen.get()) {
    auto finger = _state->fingers.find(finger_nr);
    if (finger == _state->fingers.end()) {
      // Unknown finger, releasing it would corrupt the state of another slot
      return;
    }
    auto finger_slot = finger->second;
    if (_state->current_slot != finger_slot) {
      libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_SLOT, finger_slot);
      _state->current_slot = finger_slot;
    }
    _state->fingers.erase(finger);
    libevdev_uinput_write_event(ts, EV_ABS, ABS_MT_TRACKING_ID, -1);

    libevdev_uinput_write_event(ts, EV_SYN, SYN_REPORT, 0);
  }
}

} // namespace inputtino