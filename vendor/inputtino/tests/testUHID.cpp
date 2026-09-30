#include "catch2/catch_all.hpp"
#include <crc32.hpp>
#include <filesystem>
#include <fstream>
#include <inputtino/input.hpp>
#include <iostream>
#include <SDL.h>
#include <thread>
#include <uhid/ps5.hpp>

using Catch::Matchers::Contains;
using Catch::Matchers::ContainsSubstring;
using Catch::Matchers::Equals;
using Catch::Matchers::SizeIs;
using Catch::Matchers::WithinAbs;
using namespace inputtino;
using namespace std::chrono_literals;

static void flush_sdl_events() {
  SDL_JoystickUpdate();
  SDL_Event event;
  while (SDL_PollEvent(&event) != 0) {
    switch (event.type) {
    case SDL_CONTROLLERDEVICEADDED:
      std::cout << "SDL_CONTROLLERDEVICEADDED " << SDL_GameControllerNameForIndex(event.cdevice.which) << std::endl;
      break;
    case SDL_CONTROLLERDEVICEREMOVED:
      std::cout << "SDL_CONTROLLERDEVICEREMOVED " << event.cdevice.which << std::endl;
      break;
    case SDL_CONTROLLERDEVICEREMAPPED:
      std::cout << "SDL_CONTROLLERDEVICEREMAPPED " << SDL_GameControllerNameForIndex(event.cdevice.which) << std::endl;
      break;
    case SDL_CONTROLLERBUTTONDOWN:
    case SDL_CONTROLLERBUTTONUP:
    case SDL_JOYBUTTONDOWN:
    case SDL_JOYBUTTONUP:
      std::cout << "SDL button - " << (event.cbutton.state == SDL_PRESSED ? "pressed " : "released ")
                << (int)event.cbutton.button << std::endl;
      break;
    case SDL_JOYAXISMOTION:
    case SDL_CONTROLLERAXISMOTION:
      std::cout << "SDL axis - " << (int)event.jaxis.axis << " " << event.jaxis.value << std::endl;
      break;
    case SDL_JOYHATMOTION:
      std::cout << "SDL_JOYHATMOTION " << (int)event.jhat.value << std::endl;
      break;
    default:
      std::cout << "SDL event: " << event.type << "\n";
      break;
    }
  }
}

class SDLTestsFixture {
public:
  SDLTestsFixture() {
    if (SDL_Init(SDL_INIT_JOYSTICK | SDL_INIT_HAPTIC | SDL_INIT_GAMECONTROLLER | SDL_INIT_SENSOR | SDL_INIT_EVENTS) <
        0) {
      std::cerr << "SDL could not initialize! SDL_Error: " << SDL_GetError() << std::endl;
    }
    SDL_LogSetAllPriority(SDL_LOG_PRIORITY_VERBOSE);
    SDL_GameControllerEventState(SDL_ENABLE);
  }

  ~SDLTestsFixture() {
    SDL_Quit();
  }
};

#define SDL_TEST_BUTTON(JOYPAD_BTN, SDL_BTN)                                                                           \
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_BTN) == 0);                                                              \
  joypad.set_pressed_buttons(JOYPAD_BTN);                                                                              \
  flush_sdl_events();                                                                                                  \
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_BTN) == 1);

static void test_buttons(SDL_GameController *gc, Joypad &joypad) {
  SDL_TEST_BUTTON(Joypad::DPAD_UP, SDL_CONTROLLER_BUTTON_DPAD_UP)
  SDL_TEST_BUTTON(Joypad::DPAD_DOWN, SDL_CONTROLLER_BUTTON_DPAD_DOWN)
  SDL_TEST_BUTTON(Joypad::DPAD_LEFT, SDL_CONTROLLER_BUTTON_DPAD_LEFT)
  SDL_TEST_BUTTON(Joypad::DPAD_RIGHT, SDL_CONTROLLER_BUTTON_DPAD_RIGHT)

  SDL_TEST_BUTTON(Joypad::LEFT_STICK, SDL_CONTROLLER_BUTTON_LEFTSTICK)
  SDL_TEST_BUTTON(Joypad::RIGHT_STICK, SDL_CONTROLLER_BUTTON_RIGHTSTICK)
  SDL_TEST_BUTTON(Joypad::LEFT_BUTTON, SDL_CONTROLLER_BUTTON_LEFTSHOULDER)
  SDL_TEST_BUTTON(Joypad::RIGHT_BUTTON, SDL_CONTROLLER_BUTTON_RIGHTSHOULDER)

  SDL_TEST_BUTTON(Joypad::A, SDL_CONTROLLER_BUTTON_A)
  SDL_TEST_BUTTON(Joypad::B, SDL_CONTROLLER_BUTTON_B)
  SDL_TEST_BUTTON(Joypad::X, SDL_CONTROLLER_BUTTON_X)
  SDL_TEST_BUTTON(Joypad::Y, SDL_CONTROLLER_BUTTON_Y)

  SDL_TEST_BUTTON(Joypad::START, SDL_CONTROLLER_BUTTON_START)
  SDL_TEST_BUTTON(Joypad::BACK, SDL_CONTROLLER_BUTTON_BACK)
  SDL_TEST_BUTTON(Joypad::HOME, SDL_CONTROLLER_BUTTON_GUIDE)

  // Release all buttons
  joypad.set_pressed_buttons(0);
  flush_sdl_events();
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_A) == 0);
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_B) == 0);
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_X) == 0);
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_Y) == 0);

  // Press some of them together
  joypad.set_pressed_buttons(Joypad::A | Joypad::B | Joypad::X | Joypad::Y);
  flush_sdl_events();
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_A) == 1);
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_B) == 1);
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_X) == 1);
  REQUIRE(SDL_GameControllerGetButton(gc, SDL_CONTROLLER_BUTTON_Y) == 1);
}

/**
 * @param power_supply_path (ex:
 * "/sys/devices/virtual/misc/uhid/0003:054C:0CE6.0016/power_supply/ps-controller-battery-00:21:c1:75:88:38/")
 * @return a pair of <capacity, status> as read from the system
 */
std::pair<int, std::string> get_system_battery(const std::filesystem::path &power_supply_path) {
  // It's fairly simple, we have to read two files: capacity and status
  std::ifstream capacity_file(power_supply_path / "capacity");
  std::ifstream status_file(power_supply_path / "status");
  if (!capacity_file.is_open() || !status_file.is_open()) {
    return {0, "Unknown"};
  }
  int capacity = 0;
  std::string status;
  capacity_file >> capacity;
  status_file >> status;
  return {capacity, status};
}

TEST_CASE_METHOD(SDLTestsFixture, "PS Joypad", "[SDL],[PS]") {
  // Create the controller
  auto joypad = std::move(*PS5Joypad::create());

  std::this_thread::sleep_for(50ms);

  auto devices = joypad.get_nodes();
  REQUIRE_THAT(devices, SizeIs(5)); // 3 eventXX and 2 jsYY
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/event")));
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/js")));

  // TODO: seems that I can't force it to use HIDAPI, it's picking up sysjoystick which is lacking features
  SDL_SetHint(SDL_HINT_JOYSTICK_HIDAPI, "1");
  SDL_SetHint(SDL_HINT_JOYSTICK_HIDAPI_PS5, "1");
  SDL_SetHint(SDL_HINT_JOYSTICK_HIDAPI_PS5_RUMBLE, "1");
  SDL_SetHint(SDL_HINT_JOYSTICK_HIDAPI_PS5_PLAYER_LED, "1");
  // Initializing the controller
  flush_sdl_events();
  SDL_GameController *gc = SDL_GameControllerOpen(0);
  if (gc == nullptr) {
    WARN(SDL_GetError());
  }
  REQUIRE(gc);

  REQUIRE(SDL_GameControllerGetType(gc) == SDL_CONTROLLER_TYPE_PS5);
  { // Rumble
    // Checking for basic capability
    REQUIRE(SDL_GameControllerHasRumble(gc));

    auto rumble_data = std::make_shared<std::pair<int, int>>(0, 0);
    joypad.set_on_rumble([rumble_data](int low_freq, int high_freq) {
      if (rumble_data->first == 0)
        rumble_data->first = low_freq;
      if (rumble_data->second == 0)
        rumble_data->second = high_freq;
    });

    // When debugging this, bear in mind that SDL will send max duration here
    // https://github.com/libsdl-org/SDL/blob/da8fc70a83cf6b76d5ea75c39928a7961bd163d3/src/joystick/linux/SDL_sysjoystick.c#L1628
    SDL_GameControllerRumble(gc, 0xFF00, 0xF00F, 100);
    std::this_thread::sleep_for(30ms); // wait for the effect to be picked up
    REQUIRE(rumble_data->first == 0x7f7f);
    REQUIRE(rumble_data->second == 0x7878);
  }

  test_buttons(gc, joypad);
  { // Sticks
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_LEFTX));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_LEFTY));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_RIGHTY));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT));

    joypad.set_stick(Joypad::LS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTX) == 899);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTY) == -1928);

    joypad.set_stick(Joypad::RS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTX) == 899);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTY) == -1928);

    joypad.set_stick(Joypad::RS, -16384, -32768);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX) == -16320);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTY) == 32767);

    joypad.set_triggers(125, 255);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT) == 16062);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT) == 32767);

    joypad.set_triggers(0, 0);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT) == 0);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT) == 0);
  }
  { // test acceleration
    REQUIRE(SDL_GameControllerHasSensor(gc, SDL_SENSOR_ACCEL));
    if (SDL_GameControllerSetSensorEnabled(gc, SDL_SENSOR_ACCEL, SDL_TRUE) != 0) {
      WARN(SDL_GetError());
    }

    std::array<float, 3> acceleration_data = {9.8f, 0.0f, 20.0f};
    joypad.set_motion(inputtino::PS5Joypad::ACCELERATION,
                      acceleration_data[0],
                      acceleration_data[1],
                      acceleration_data[2]);
    SDL_GameControllerUpdate();
    SDL_SensorUpdate();
    SDL_Event event;
    while (SDL_PollEvent(&event) != 0) {
      if (event.type == SDL_CONTROLLERSENSORUPDATE) {
        break;
      }
    }
    REQUIRE(event.type == SDL_CONTROLLERSENSORUPDATE);
    REQUIRE(event.csensor.sensor == SDL_SENSOR_ACCEL);
    REQUIRE_THAT(event.csensor.data[0], WithinAbs(acceleration_data[0], 0.9f));
    REQUIRE_THAT(event.csensor.data[1], WithinAbs(acceleration_data[1], 0.9f));
    REQUIRE_THAT(event.csensor.data[2], WithinAbs(acceleration_data[2], 0.9f));
    flush_sdl_events();

    // Now lets test the negatives
    acceleration_data = {-9.8f, -0.0f, -20.0f};
    joypad.set_motion(inputtino::PS5Joypad::ACCELERATION,
                      acceleration_data[0],
                      acceleration_data[1],
                      acceleration_data[2]);
    SDL_GameControllerUpdate();
    SDL_SensorUpdate();
    while (SDL_PollEvent(&event) != 0) {
      if (event.type == SDL_CONTROLLERSENSORUPDATE) {
        break;
      }
    }
    REQUIRE(event.type == SDL_CONTROLLERSENSORUPDATE);
    REQUIRE(event.csensor.sensor == SDL_SENSOR_ACCEL);
    REQUIRE_THAT(event.csensor.data[0], WithinAbs(acceleration_data[0], 0.9f));
    REQUIRE_THAT(event.csensor.data[1], WithinAbs(acceleration_data[1], 0.9f));
    REQUIRE_THAT(event.csensor.data[2], WithinAbs(acceleration_data[2], 0.9f));
    flush_sdl_events();
  }
  { // test gyro
    REQUIRE(SDL_GameControllerHasSensor(gc, SDL_SENSOR_GYRO));
    if (SDL_GameControllerSetSensorEnabled(gc, SDL_SENSOR_GYRO, SDL_TRUE) != 0) {
      WARN(SDL_GetError());
    }

    std::array<float, 3> gyro_data = {0.0f,
                                      M_PI_2f, // half a turn (180 degrees)
                                      M_PIf};  // full turn (360 degrees)
    joypad.set_motion(inputtino::PS5Joypad::GYROSCOPE, gyro_data[0], gyro_data[1], gyro_data[2]);
    std::this_thread::sleep_for(10ms);

    SDL_GameControllerUpdate();
    SDL_SensorUpdate();
    SDL_Event event;
    while (SDL_PollEvent(&event) != 0) {
      if (event.type == SDL_CONTROLLERSENSORUPDATE) {
        break;
      }
    }
    REQUIRE(event.type == SDL_CONTROLLERSENSORUPDATE);
    REQUIRE(event.csensor.sensor == SDL_SENSOR_GYRO);
    REQUIRE_THAT(event.csensor.data[0], WithinAbs(gyro_data[0], 0.01f));
    REQUIRE_THAT(event.csensor.data[1], WithinAbs(gyro_data[1], 0.01f));
    REQUIRE_THAT(event.csensor.data[2], WithinAbs(gyro_data[2], 0.01f));
    flush_sdl_events();

    // Now let's try the negatives
    gyro_data = {
        -0.0f,
        -M_PI_2f, // half a turn (180 degrees)
        -M_PIf    // full turn (360 degrees)
    };
    joypad.set_motion(inputtino::PS5Joypad::GYROSCOPE, gyro_data[0], gyro_data[1], gyro_data[2]);
    std::this_thread::sleep_for(10ms);

    SDL_GameControllerUpdate();
    SDL_SensorUpdate();
    while (SDL_PollEvent(&event) != 0) {
      if (event.type == SDL_CONTROLLERSENSORUPDATE) {
        break;
      }
    }
    REQUIRE(event.type == SDL_CONTROLLERSENSORUPDATE);
    REQUIRE(event.csensor.sensor == SDL_SENSOR_GYRO);
    REQUIRE_THAT(event.csensor.data[0], WithinAbs(gyro_data[0], 0.01f));
    REQUIRE_THAT(event.csensor.data[1], WithinAbs(gyro_data[1], 0.01f));
    REQUIRE_THAT(event.csensor.data[2], WithinAbs(gyro_data[2], 0.01f));
    flush_sdl_events();

    // Try out problematic values from https://github.com/LizardByte/Sunshine/issues/3247
    gyro_data = {-32769.0f, 32769.0f, -0.0004124999977648258f};
    joypad.set_motion(inputtino::PS5Joypad::GYROSCOPE, gyro_data[0], gyro_data[1], gyro_data[2]);
    std::this_thread::sleep_for(10ms);

    SDL_GameControllerUpdate();
    SDL_SensorUpdate();
    while (SDL_PollEvent(&event) != 0) {
      if (event.type == SDL_CONTROLLERSENSORUPDATE) {
        break;
      }
    }
    REQUIRE(event.type == SDL_CONTROLLERSENSORUPDATE);
    REQUIRE(event.csensor.sensor == SDL_SENSOR_GYRO);
    REQUIRE_THAT(event.csensor.data[0], WithinAbs(-28.59546f, 0.01f));
    REQUIRE_THAT(event.csensor.data[1], WithinAbs(28.59546f, 0.01f));
    REQUIRE_THAT(event.csensor.data[2], WithinAbs(0.0f, 0.01f));
    flush_sdl_events();
  }

  { // LED TODO: seems that this only works after some gyro/acceleration data is sent
    REQUIRE(SDL_GameControllerHasLED(gc));
    struct LED {
      int r;
      int g;
      int b;
    };
    auto led_data = std::make_shared<LED>();
    joypad.set_on_led([led_data](int r, int g, int b) {
      led_data->r = r;
      led_data->g = g;
      led_data->b = b;
    });
    REQUIRE(SDL_GameControllerSetLED(gc, 50, 100, 150) == 0);
    std::this_thread::sleep_for(20ms); // wait for the effect to be picked up
    REQUIRE(led_data->r == 50);
    REQUIRE(led_data->g == 100);
    REQUIRE(led_data->b == 150);
  }

  { // Test touchpad
    REQUIRE(SDL_GameControllerGetNumTouchpads(gc) == 1);
    REQUIRE(SDL_GameControllerGetNumTouchpadFingers(gc, 0) == 2);

    // TODO: test these values with SDL
    joypad.place_finger(0, 1920, 1080);
    joypad.place_finger(1, 1920, 1080);
    joypad.release_finger(0);
    joypad.release_finger(1);
  }

  { // Test battery
    auto joy = SDL_GameControllerGetJoystick(gc);
    REQUIRE(SDL_JoystickCurrentPowerLevel(joy) == SDL_JOYSTICK_POWER_FULL);

    auto base_path =
        std::filesystem::path(
            joypad.get_sys_nodes()[0]) // "/sys/devices/virtual/misc/uhid/0003:054C:0CE6.0017/input/input123"
            .parent_path()             // "/sys/devices/virtual/misc/uhid/0003:054C:0CE6.0017/input/"
            .parent_path()             // "/sys/devices/virtual/misc/uhid/0003:054C:0CE6.0017/"
        / "power_supply"               // "/sys/devices/virtual/misc/uhid/0003:054C:0CE6.0017/power_supply"
        / ("ps-controller-battery-" + joypad.get_mac_address());
    REQUIRE(std::filesystem::exists(base_path));

    { // Defaults to full if nothing is set
      auto [capacity, status] = get_system_battery(base_path);
      REQUIRE(capacity == 100);
      REQUIRE(status == "Full");
    }

    {
      joypad.set_battery(inputtino::PS5Joypad::BATTERY_CHARGHING, 80);
      auto [capacity, status] = get_system_battery(base_path);
      REQUIRE(capacity == 85);
      REQUIRE(status == "Charging");
    }

    {
      joypad.set_battery(inputtino::PS5Joypad::BATTERY_CHARGHING, 10);
      auto [capacity, status] = get_system_battery(base_path);
      REQUIRE(capacity == 15);
      REQUIRE(status == "Charging");
    }

    {
      joypad.set_battery(inputtino::PS5Joypad::BATTERY_DISCHARGING, 75);
      auto [capacity, status] = get_system_battery(base_path);
      REQUIRE(capacity == 75);
      REQUIRE(status == "Discharging");
    }

    {
      joypad.set_battery(inputtino::PS5Joypad::BATTERY_FULL, 100);
      auto [capacity, status] = get_system_battery(base_path);
      REQUIRE(capacity == 100);
      REQUIRE(status == "Full");
    }
  }

  // Adaptive triggers aren't directly supported by SDL
  // see:https://github.com/libsdl-org/SDL/issues/5125#issuecomment-1204261666
  // see: HIDAPI_DriverPS5_RumbleJoystickTriggers()
  // but we can send custom data to the device, the following code is adapted from
  // https://github.com/libsdl-org/SDL/blob/d66483dfccfcdc4e03f719e318c7a76f963f22d9/test/testcontroller.c#L235-L255
  {
    auto trigger_event = std::make_shared<PS5Joypad::TriggerEffect>();
    joypad.set_on_trigger_effect([trigger_event](const PS5Joypad::TriggerEffect &effect) { *trigger_event = effect; });

    /* Resistance and vibration when trigger is pulled */
    uint8_t left_effect_type = 0x06;
    uint8_t left_effect[10] = {15, 63, 128, 0, 0, 0, 0, 0, 0, 0};
    /* Constant resistance across entire trigger pull */
    uint8_t right_effect_type = 0x01;
    uint8_t right_effect[10] = {0, 110, 0, 0, 0, 0, 0, 0, 0, 0};

    uhid::dualsense_output_report_common state = {};
    SDL_zero(state);
    state.valid_flag0 |= (uhid::RIGHT_TRIGGER_EFFECT);
    state.right_trigger_effect_type = right_effect_type;
    SDL_memcpy(state.right_trigger_effect, right_effect, sizeof(right_effect));
    state.left_trigger_effect_type = left_effect_type;
    SDL_memcpy(state.left_trigger_effect, left_effect, sizeof(left_effect));
    SDL_GameControllerSendEffect(gc, &state, sizeof(state));

    std::this_thread::sleep_for(15ms);
    flush_sdl_events();
    REQUIRE(trigger_event->event_flags == uhid::RIGHT_TRIGGER_EFFECT);
    REQUIRE(trigger_event->type_left == left_effect_type);
    REQUIRE(trigger_event->type_right == right_effect_type);
    REQUIRE(std::equal(std::begin(trigger_event->left), std::end(trigger_event->left), std::begin(left_effect)));
    REQUIRE(std::equal(std::begin(trigger_event->right), std::end(trigger_event->right), std::begin(right_effect)));
  }

  { // Test creating a second device
    REQUIRE(SDL_NumJoysticks() == 1);
    auto joypad2 = std::move(*PS5Joypad::create());
    std::this_thread::sleep_for(50ms);

    auto devices2 = joypad2.get_nodes();
    REQUIRE_THAT(devices2, SizeIs(5)); // 3 eventXX and 2 jsYY
    REQUIRE_THAT(devices2, Contains(ContainsSubstring("/dev/input/event")));
    REQUIRE_THAT(devices2, Contains(ContainsSubstring("/dev/input/js")));

    flush_sdl_events();
    REQUIRE(SDL_NumJoysticks() == 2);
    SDL_GameController *gc2 = SDL_GameControllerOpen(1);
    REQUIRE(SDL_GameControllerGetType(gc2) == SDL_CONTROLLER_TYPE_PS5);
    SDL_GameControllerClose(gc2);
  }

  SDL_GameControllerClose(gc);
}

TEST_CASE("Bluetooth CRC32", "[PS]") {
  std::string buffer = "123456789";
  auto crc = CRC32(reinterpret_cast<const unsigned char *>(buffer.data()), buffer.length());
  REQUIRE(crc == 0xcbf43926); // https://crccalc.com/?crc=123456789&method=CRC-32/ISO-HDLC&datatype=ascii&outtype=hex

  unsigned char PS_INPUT_CRC32_SEED = 0xA1;
  auto crc2 = CRC32(&PS_INPUT_CRC32_SEED, 1, 0xFFFFFFFF);
  crc2 = CRC32(reinterpret_cast<unsigned char *>(buffer.data()), buffer.length(), crc2);
  REQUIRE(crc2 == 0x9498b398);
}

TEST_CASE("Test MAC address", "[PS]") {
  DeviceDefinition def = {.name = "Wolf DualSense (virtual) pad",
                          .vendor_id = 0x054C,
                          .product_id = 0x0CE6,
                          .version = 0x8111,
                          .device_uniq = "AA:00:CC:11:EE:22"};

  auto joypad = std::move(*PS5Joypad::create(def));

  std::this_thread::sleep_for(50ms);
  REQUIRE_THAT(joypad.get_mac_address(), Equals("aa:00:cc:11:ee:22"));
}