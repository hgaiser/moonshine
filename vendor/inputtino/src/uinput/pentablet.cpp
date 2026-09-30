#include "inputtino/input.hpp"
#include <algorithm>
#include <cmath>
#include <cstring>
#include <inputtino/protected_types.hpp>

namespace inputtino {

static const std::map<int, int> tool_to_linux = {
    {PenTablet::PEN, BTN_TOOL_PEN},
    {PenTablet::ERASER, BTN_TOOL_RUBBER},
    {PenTablet::BRUSH, BTN_TOOL_BRUSH},
    {PenTablet::PENCIL, BTN_TOOL_PENCIL},
    {PenTablet::AIRBRUSH, BTN_TOOL_AIRBRUSH},
    {PenTablet::TOUCH, BTN_TOUCH},
};

static const std::map<int, int> btn_to_linux = {
    {PenTablet::PRIMARY, BTN_STYLUS},
    {PenTablet::SECONDARY, BTN_STYLUS2},
    {PenTablet::TERTIARY, BTN_STYLUS3},
};

static constexpr int MAX_X = 19200;
static constexpr int MAX_Y = 10800;
static constexpr int PRESSURE_MAX = 4096;
static constexpr int DISTANCE_MAX = 1024;
static constexpr int RESOLUTION = 28;

Result<libevdev_uinput_ptr> create_tablet(const DeviceDefinition &device) {
  libevdev *dev = libevdev_new();
  libevdev_uinput *uidev;

  libevdev_set_name(dev, device.name.c_str());
  libevdev_set_id_vendor(dev, device.vendor_id);
  libevdev_set_id_product(dev, device.product_id);
  libevdev_set_id_version(dev, device.version);
  libevdev_set_id_bustype(dev, BUS_USB);

  libevdev_enable_event_type(dev, EV_KEY);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOUCH, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_STYLUS, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_STYLUS2, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_STYLUS3, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOOL_PEN, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOOL_RUBBER, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOOL_BRUSH, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOOL_PENCIL, nullptr);
  libevdev_enable_event_code(dev, EV_KEY, BTN_TOOL_AIRBRUSH, nullptr);

  libevdev_enable_event_type(dev, EV_ABS);
  input_absinfo abs_x{0, 0, MAX_X, 1, 0, RESOLUTION};
  libevdev_enable_event_code(dev, EV_ABS, ABS_X, &abs_x);

  input_absinfo abs_y{0, 0, MAX_Y, 1, 0, RESOLUTION};
  libevdev_enable_event_code(dev, EV_ABS, ABS_Y, &abs_y);

  input_absinfo pressure{0, 0, PRESSURE_MAX, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_PRESSURE, &pressure);

  input_absinfo distance{0, 0, DISTANCE_MAX, 0, 0, 0};
  libevdev_enable_event_code(dev, EV_ABS, ABS_DISTANCE, &distance);
  input_absinfo abs_tilt{0, -90, 90, 0, 0, RESOLUTION}; // If resolution is nonzero, it's in units/radian.
  libevdev_enable_event_code(dev, EV_ABS, ABS_TILT_X, &abs_tilt);
  libevdev_enable_event_code(dev, EV_ABS, ABS_TILT_Y, &abs_tilt);

  // https://docs.kernel.org/input/event-codes.html#tablets
  libevdev_enable_property(dev, INPUT_PROP_POINTER);
  libevdev_enable_property(dev, INPUT_PROP_DIRECT);
  auto err = libevdev_uinput_create_from_device(dev, LIBEVDEV_UINPUT_OPEN_MANAGED, &uidev);
  libevdev_free(dev);
  if (err != 0) {
    return Error(strerror(-err));
  }

  return libevdev_uinput_ptr{uidev, ::libevdev_uinput_destroy};
}

PenTablet::PenTablet() : _state(std::make_shared<PenTabletState>()) {}

PenTablet::~PenTablet() {
  if (_state) {
    _state.reset();
  }
}

Result<PenTablet> PenTablet::create(const DeviceDefinition &device) {
  auto tablet = create_tablet(device);
  if (tablet) {
    PenTablet pt;
    pt._state->pen_tablet = std::move(*tablet);
    return std::move(pt);
  } else {
    return Error(tablet.getErrorMessage());
  }
}

std::vector<std::string> PenTablet::get_nodes() const {
  std::vector<std::string> nodes;

  if (auto kb = _state->pen_tablet.get()) {
    nodes.emplace_back(libevdev_uinput_get_devnode(kb));
  }

  return nodes;
}

static inline float deg2rad(float degree) {
  return M_PI * degree / 180.0;
}

void PenTablet::place_tool(
    PenTablet::TOOL_TYPE tool_type, float x, float y, float pressure, float distance, float tilt_x, float tilt_y) {
  if (auto tablet = _state->pen_tablet.get()) {
    if (tool_type != PenTablet::SAME_AS_BEFORE && tool_type != _state->last_tool) {
      libevdev_uinput_write_event(tablet, EV_KEY, tool_to_linux.at(tool_type), 1);

      if (_state->last_tool != PenTablet::SAME_AS_BEFORE)
        libevdev_uinput_write_event(tablet, EV_KEY, tool_to_linux.at(_state->last_tool), 0);

      _state->last_tool = tool_type;
    }

    int scaled_x = (int)std::lround(MAX_X * x);
    int scaled_y = (int)std::lround(MAX_Y * y);
    libevdev_uinput_write_event(tablet, EV_ABS, ABS_X, scaled_x);
    libevdev_uinput_write_event(tablet, EV_ABS, ABS_Y, scaled_y);

    if (pressure >= 0) {
      int scaled_pressure = (int)std::lround(pressure * PRESSURE_MAX);
      libevdev_uinput_write_event(tablet, EV_ABS, ABS_PRESSURE, scaled_pressure);
      // when there's pressure, the tool must be touching the tablet
      libevdev_uinput_write_event(tablet, EV_ABS, ABS_DISTANCE, 0);
    }

    if (distance >= 0) {
      int scaled_distance = (int)std::lround(distance * DISTANCE_MAX);
      libevdev_uinput_write_event(tablet, EV_ABS, ABS_DISTANCE, scaled_distance);
      // when there's distance, the tool can't be touching the tablet
      libevdev_uinput_write_event(tablet, EV_ABS, ABS_PRESSURE, 0);
    }

    auto scaled_tilt_x = std::clamp(tilt_x, -90.0f, 90.0f);
    scaled_tilt_x = deg2rad(scaled_tilt_x * RESOLUTION);
    libevdev_uinput_write_event(tablet, EV_ABS, ABS_TILT_X, (int)std::lround(scaled_tilt_x));

    auto scaled_tilt_y = std::clamp(tilt_y, -90.0f, 90.0f);
    scaled_tilt_y = deg2rad(scaled_tilt_y * RESOLUTION);
    libevdev_uinput_write_event(tablet, EV_ABS, ABS_TILT_Y, (int)std::lround(scaled_tilt_y));

    libevdev_uinput_write_event(tablet, EV_SYN, SYN_REPORT, 0);
  }
}

void PenTablet::set_btn(PenTablet::BTN_TYPE btn, bool pressed) {
  if (auto tablet = _state->pen_tablet.get()) {
    libevdev_uinput_write_event(tablet, EV_KEY, btn_to_linux.at(btn), pressed ? 1 : 0);
    libevdev_uinput_write_event(tablet, EV_SYN, SYN_REPORT, 0);
  }
}

} // namespace inputtino
