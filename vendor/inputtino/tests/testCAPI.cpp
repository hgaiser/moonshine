#include "catch2/catch_all.hpp"
#include <filesystem>
#include <inputtino/input.h>
#include <thread>

TEST_CASE("C Mouse API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto mouse = inputtino_mouse_create(&def, &error_handler);
  REQUIRE(mouse != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_mouse_get_nodes(mouse, &num_nodes);
  REQUIRE(num_nodes == 2);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));
  REQUIRE_THAT(std::string(nodes[1]), Catch::Matchers::StartsWith("/dev/input/event"));

  REQUIRE(std::filesystem::exists(nodes[0]));
  REQUIRE(std::filesystem::exists(nodes[1]));

  { // TODO: test that this actually work
    inputtino_mouse_move(mouse, 100, 100);
    inputtino_mouse_move_absolute(mouse, 100, 100, 1920, 1080);
    inputtino_mouse_press_button(mouse, INPUTTINO_MOUSE_BUTTON::MIDDLE);
    inputtino_mouse_release_button(mouse, INPUTTINO_MOUSE_BUTTON::MIDDLE);
    inputtino_mouse_scroll_vertical(mouse, 125);
    inputtino_mouse_scroll_horizontal(mouse, 125);
  }

  delete[] nodes;
  inputtino_mouse_destroy(mouse);
}

TEST_CASE("C Trackpad API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto trackpad = inputtino_trackpad_create(&def, &error_handler);
  REQUIRE(trackpad != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_trackpad_get_nodes(trackpad, &num_nodes);
  REQUIRE(num_nodes == 1);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));

  REQUIRE(std::filesystem::exists(nodes[0]));

  { // TODO: test that this actually work
    inputtino_trackpad_place_finger(trackpad, 0, 100, 200, 1.0, 1);
    inputtino_trackpad_release_finger(trackpad, 0);
    inputtino_trackpad_set_left_btn(trackpad, true);
  }

  delete[] nodes;
  inputtino_trackpad_destroy(trackpad);
}

TEST_CASE("C Touchscreen API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto touchscreen = inputtino_touchscreen_create(&def, &error_handler);
  REQUIRE(touchscreen != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_touchscreen_get_nodes(touchscreen, &num_nodes);
  REQUIRE(num_nodes == 1);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));

  REQUIRE(std::filesystem::exists(nodes[0]));

  { // TODO: test that this actually work
    inputtino_touchscreen_place_finger(touchscreen, 0, 100, 200, 1.0, 1);
    inputtino_touchscreen_release_finger(touchscreen, 0);
  }

  delete[] nodes;
  inputtino_touchscreen_destroy(touchscreen);
}

TEST_CASE("C PenTablet API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto pen_tablet = inputtino_pen_tablet_create(&def, &error_handler);
  REQUIRE(pen_tablet != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_pen_tablet_get_nodes(pen_tablet, &num_nodes);
  REQUIRE(num_nodes == 1);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));

  REQUIRE(std::filesystem::exists(nodes[0]));

  { // TODO: test that this actually work
    inputtino_pen_tablet_place_tool(pen_tablet, INPUTTINO_PEN_TOOL_TYPE::PEN, 100, 200, 1.0, 0.5, 0.5, 0.5);
    inputtino_pen_tablet_set_button(pen_tablet, INPUTTINO_PEN_BTN_TYPE::PRIMARY, true);
  }

  delete[] nodes;
  inputtino_pen_tablet_destroy(pen_tablet);
}

TEST_CASE("C Keyboard API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto keyboard = inputtino_keyboard_create(&def, &error_handler);
  REQUIRE(keyboard != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_keyboard_get_nodes(keyboard, &num_nodes);
  REQUIRE(num_nodes == 1);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));

  REQUIRE(std::filesystem::exists(nodes[0]));

  { // TODO: test that this actually work
    inputtino_keyboard_press(keyboard, 1);
    inputtino_keyboard_release(keyboard, 1);
  }

  delete[] nodes;
  inputtino_keyboard_destroy(keyboard);
}

TEST_CASE("C XOne API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto xone = inputtino_joypad_xone_create(&def, &error_handler);
  REQUIRE(xone != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_joypad_xone_get_nodes(xone, &num_nodes);
  REQUIRE(num_nodes == 2);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));
  REQUIRE_THAT(std::string(nodes[1]), Catch::Matchers::StartsWith("/dev/input/js"));

  REQUIRE(std::filesystem::exists(nodes[0]));
  REQUIRE(std::filesystem::exists(nodes[1]));

  { // TODO: test that this actually work
    inputtino_joypad_xone_set_pressed_buttons(xone, INPUTTINO_JOYPAD_BTN::A | INPUTTINO_JOYPAD_BTN::B);
    inputtino_joypad_xone_set_triggers(xone, 50, 50);
    inputtino_joypad_xone_set_stick(xone, INPUTTINO_JOYPAD_STICK_POSITION::RS, 50, 50);
    inputtino_joypad_xone_set_on_rumble(xone, [](int low_freq, int high_freq, void *user_data) {}, nullptr);
  }

  delete[] nodes;
  inputtino_joypad_xone_destroy(xone);
}

TEST_CASE("C Switch API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {};
  auto switch_ = inputtino_joypad_switch_create(&def, &error_handler);
  REQUIRE(switch_ != nullptr);

  int num_nodes = 0;
  auto nodes = inputtino_joypad_switch_get_nodes(switch_, &num_nodes);
  REQUIRE(num_nodes == 2);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/event"));
  REQUIRE_THAT(std::string(nodes[1]), Catch::Matchers::StartsWith("/dev/input/js"));

  REQUIRE(std::filesystem::exists(nodes[0]));
  REQUIRE(std::filesystem::exists(nodes[1]));

  { // TODO: test that this actually work
    inputtino_joypad_switch_set_pressed_buttons(switch_, INPUTTINO_JOYPAD_BTN::A | INPUTTINO_JOYPAD_BTN::B);
    inputtino_joypad_switch_set_triggers(switch_, 50, 50);
    inputtino_joypad_switch_set_stick(switch_, INPUTTINO_JOYPAD_STICK_POSITION::RS, 50, 50);
    inputtino_joypad_switch_set_on_rumble(switch_, [](int low_freq, int high_freq, void *user_data) {}, nullptr);
  }

  delete[] nodes;
  inputtino_joypad_switch_destroy(switch_);
}

TEST_CASE("C PS5 API", "[C-API]") {
  InputtinoErrorHandler error_handler = {.eh = [](const char *message, void *_data) { FAIL(message); },
                                         .user_data = nullptr};
  InputtinoDeviceDefinition def = {.name = "Wolf DualSense (virtual) pad",
                                   .vendor_id = 0x054C,
                                   .product_id = 0x0CE6,
                                   .version = 0x8111};
  auto ps_pad = inputtino_joypad_ps5_create(&def, &error_handler);
  REQUIRE(ps_pad != nullptr);
  std::this_thread::sleep_for(std::chrono::milliseconds(50));
  int num_nodes = 0;
  auto nodes = inputtino_joypad_ps5_get_nodes(ps_pad, &num_nodes);
  REQUIRE(num_nodes >= 2);
  REQUIRE_THAT(std::string(nodes[0]), Catch::Matchers::StartsWith("/dev/input/"));

  { // TODO: test that this actually work
    inputtino_joypad_ps5_set_pressed_buttons(ps_pad, INPUTTINO_JOYPAD_BTN::A | INPUTTINO_JOYPAD_BTN::B);
    inputtino_joypad_ps5_set_triggers(ps_pad, 50, 50);
    inputtino_joypad_ps5_set_stick(ps_pad, INPUTTINO_JOYPAD_STICK_POSITION::RS, 50, 50);
    inputtino_joypad_ps5_set_on_rumble(ps_pad, [](int low_freq, int high_freq, void *user_data) {}, nullptr);
    inputtino_joypad_ps5_place_finger(ps_pad, 1, 100, 200);
    inputtino_joypad_ps5_release_finger(ps_pad, 1);
    inputtino_joypad_ps5_set_motion(ps_pad, INPUTTINO_JOYPAD_MOTION_TYPE::ACCELERATION, 1, 1, 1);
    inputtino_joypad_ps5_set_battery(ps_pad, BATTERY_STATE::BATTERY_DISCHARGING, 90);
    inputtino_joypad_ps5_set_on_led(ps_pad, [](int r, int g, int b, void *user_data) {}, nullptr);
    inputtino_joypad_ps5_set_on_trigger_effect(
        ps_pad,
        [](uint8_t type_left,
           uint8_t event_flags,
           uint8_t type_right,
           const uint8_t *left,
           const uint8_t *right,
           void *user_data) {},
        nullptr);
  }

  delete[] nodes;
  inputtino_joypad_ps5_destroy(ps_pad);
}
