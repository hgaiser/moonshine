#include "catch2/catch_all.hpp"
#include <filesystem>
#include <fstream>
#include <inputtino/input.hpp>
#include <iostream>
#include <SDL.h>
#include <thread>

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

TEST_CASE_METHOD(SDLTestsFixture, "XBOX Joypad", "[SDL]") {
  // Create the controller
  auto joypad = std::move(*XboxOneJoypad::create());

  std::this_thread::sleep_for(150ms);

  auto devices = joypad.get_nodes();
  REQUIRE_THAT(devices, SizeIs(2)); // 1 eventXX and 1 jsYY
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/event")));
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/js")));

  // Initializing the controller
  flush_sdl_events();
  SDL_GameController *gc = SDL_GameControllerOpen(0);
  if (gc == nullptr) {
    WARN(SDL_GetError());
  }
  REQUIRE(gc);
  REQUIRE(SDL_GameControllerGetType(gc) == SDL_CONTROLLER_TYPE_XBOXONE);
  // Checking for basic joypad capabilities
  REQUIRE(SDL_GameControllerHasRumble(gc));

  test_buttons(gc, joypad);
  { // Rumble
    // Checking for basic capability
    REQUIRE(SDL_GameControllerHasRumble(gc));

    auto rumble_data = std::make_shared<std::pair<int, int>>();
    joypad.set_on_rumble([rumble_data](int low_freq, int high_freq) {
      rumble_data->first = low_freq;
      rumble_data->second = high_freq;
    });

    // When debugging this, bear in mind that SDL will send max duration here
    // https://github.com/libsdl-org/SDL/blob/da8fc70a83cf6b76d5ea75c39928a7961bd163d3/src/joystick/linux/SDL_sysjoystick.c#L1628
    SDL_GameControllerRumble(gc, 100, 200, 100);
    std::this_thread::sleep_for(30ms); // wait for the effect to be picked up
    REQUIRE(rumble_data->first == 100);
    REQUIRE(rumble_data->second == 200);
  }

  { // Sticks
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_LEFTX));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_LEFTY));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_RIGHTY));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT));

    joypad.set_stick(Joypad::LS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTX) == 1000);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTY) == -2000);

    joypad.set_stick(Joypad::RS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX) == 1000);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTY) == -2000);

    joypad.set_triggers(10, 20);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT) == 1284);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT) == 2569);

    joypad.set_triggers(0, 0);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT) == 0);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT) == 0);
  }

  SDL_GameControllerClose(gc);
}

TEST_CASE_METHOD(SDLTestsFixture, "Nintendo Joypad", "[SDL]") {
  // Create the controller
  auto joypad = std::move(*SwitchJoypad::create());

  std::this_thread::sleep_for(150ms);

  auto devices = joypad.get_nodes();
  REQUIRE_THAT(devices, SizeIs(2)); // 1 eventXX and 1 jsYY
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/event")));
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/js")));

  // Initializing the controller
  flush_sdl_events();
  SDL_GameController *gc = SDL_GameControllerOpen(0);
  if (gc == nullptr) {
    WARN(SDL_GetError());
  }
  REQUIRE(gc);
  REQUIRE(SDL_GameControllerGetType(gc) == SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_PRO);

  test_buttons(gc, joypad);
  { // Rumble
    // Checking for basic capability
    REQUIRE(SDL_GameControllerHasRumble(gc));

    auto rumble_data = std::make_shared<std::pair<int, int>>();
    joypad.set_on_rumble([rumble_data](int low_freq, int high_freq) {
      rumble_data->first = low_freq;
      rumble_data->second = high_freq;
    });

    // When debugging this, bear in mind that SDL will send max duration here
    // https://github.com/libsdl-org/SDL/blob/da8fc70a83cf6b76d5ea75c39928a7961bd163d3/src/joystick/linux/SDL_sysjoystick.c#L1628
    SDL_GameControllerRumble(gc, 100, 200, 100);
    std::this_thread::sleep_for(30ms); // wait for the effect to be picked up
    REQUIRE(rumble_data->first == 100);
    REQUIRE(rumble_data->second == 200);
  }

  SDL_TEST_BUTTON(Joypad::MISC_FLAG, SDL_CONTROLLER_BUTTON_MISC1);

  { // Sticks
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_LEFTX));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_LEFTY));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_RIGHTY));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT));
    REQUIRE(SDL_GameControllerHasAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT));

    joypad.set_stick(Joypad::LS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTX) == 1000);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTY) == -2000);

    joypad.set_stick(Joypad::RS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX) == 1000);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTY) == -2000);

    // Nintendo ONLY: triggers are buttons, so it can only be MAX or 0
    joypad.set_triggers(10, 20);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT) == 32767);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT) == 32767);

    joypad.set_triggers(0, 0);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERLEFT) == 0);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_TRIGGERRIGHT) == 0);
  }

  SDL_GameControllerClose(gc);
}

#ifndef USE_UHID
// This test is only valid when using the uinput backend
TEST_CASE_METHOD(SDLTestsFixture, "PS Joypad (basic)", "[SDL],[PS]") {
  // Create the controller
  auto joypad = std::move(*PS5Joypad::create());

  std::this_thread::sleep_for(50ms);

  auto devices = joypad.get_nodes();
  REQUIRE_THAT(devices, SizeIs(2));
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/event")));
  REQUIRE_THAT(devices, Contains(ContainsSubstring("/dev/input/js")));

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
    REQUIRE(rumble_data->first == 0xff00);
    REQUIRE(rumble_data->second == 0xF00F);
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
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTX) == 1000);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTY) == -2000);

    joypad.set_stick(Joypad::RS, 1000, 2000);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTX) == 1000);
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_LEFTY) == -2000);

    joypad.set_stick(Joypad::RS, -16384, -32768);
    flush_sdl_events();
    REQUIRE(SDL_GameControllerGetAxis(gc, SDL_CONTROLLER_AXIS_RIGHTX) == -16384);
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

  SDL_GameControllerClose(gc);
}
#endif // USE_UHID
