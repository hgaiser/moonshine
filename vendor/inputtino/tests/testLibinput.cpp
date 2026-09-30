#include "catch2/catch_all.hpp"

#include "libinput.h"
#include <inputtino/input.hpp>
#include <libinput.h>
#include <linux/input-event-codes.h>
#include <thread>
#include <vector>

using namespace inputtino;
using Catch::Matchers::WithinAbs;
using Catch::Matchers::WithinRel;
using namespace std::chrono_literals;

/**
 * TESTS
 */

TEST_CASE("virtual keyboard", "[LIBINPUT]") {
    auto kb = std::move(*Keyboard::create());
    auto li = create_libinput_context(kb.get_nodes());
    auto event = get_event(li);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_KEYBOARD));

    short test_key = 0x41;
    auto linux_code = KEY_A;

    { // Test pressing a key
        kb.press(test_key);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_KEYBOARD_KEY);
        auto k_event = libinput_event_get_keyboard_event(event.get());
        REQUIRE(libinput_event_keyboard_get_key(k_event) == linux_code);
        REQUIRE(libinput_event_keyboard_get_key_state(k_event) == LIBINPUT_KEY_STATE_PRESSED);
    }

    { // Test releasing a key
        kb.release(test_key);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_KEYBOARD_KEY);
        auto k_event = libinput_event_get_keyboard_event(event.get());
        REQUIRE(libinput_event_keyboard_get_key(k_event) == linux_code);
        REQUIRE(libinput_event_keyboard_get_key_state(k_event) == LIBINPUT_KEY_STATE_RELEASED);
    }
}

TEST_CASE("virtual mouse relative", "[LIBINPUT]") {
    auto mouse = std::move(*Mouse::create());
    auto li = create_libinput_context({mouse.get_nodes()[0]});
    auto event = get_event(li);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_POINTER));

    {
        mouse.move(100, 100);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_MOTION);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_dx_unaccelerated(p_event) == 100);
        REQUIRE(libinput_event_pointer_get_dy_unaccelerated(p_event) == 100);
    }

    {
        mouse.press(Mouse::LEFT);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_BUTTON);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_button(p_event) == BTN_LEFT);
        REQUIRE(libinput_event_pointer_get_button_state(p_event) == LIBINPUT_BUTTON_STATE_PRESSED);
    }

    std::this_thread::sleep_for(50ms); // TODO: not sure why this is needed

    {
        mouse.release(Mouse::LEFT);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_BUTTON);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_button(p_event) == BTN_LEFT);
        REQUIRE(libinput_event_pointer_get_button_state(p_event) == LIBINPUT_BUTTON_STATE_RELEASED);
    }

    {
        mouse.vertical_scroll(121);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_scroll_value_v120(p_event, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) == -121);
        // The value is the angle the wheel moved in degrees. The default is 15 degrees per wheel click
        REQUIRE(libinput_event_pointer_get_scroll_value(p_event, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) == -15.125);
        event = get_event(li); // skipping LIBINPUT_EVENT_POINTER_AXIS
    }

    {
        mouse.vertical_scroll(-121);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_scroll_value_v120(p_event, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) == 121);
        // The value is the angle the wheel moved in degrees. The default is 15 degrees per wheel click
        REQUIRE(libinput_event_pointer_get_scroll_value(p_event, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) == 15.125);
        event = get_event(li); // skipping LIBINPUT_EVENT_POINTER_AXIS
    }

    {
        mouse.horizontal_scroll(121);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_scroll_value_v120(p_event, LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL) == 121);
        // The value is the angle the wheel moved in degrees. The default is 15 degrees per wheel click
        REQUIRE(libinput_event_pointer_get_scroll_value(p_event, LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL) == 15.125);
        event = get_event(li); // skipping LIBINPUT_EVENT_POINTER_AXIS
    }

    {
        mouse.horizontal_scroll(-121);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_scroll_value_v120(p_event, LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL) == -121);
        // The value is the angle the wheel moved in degrees. The default is 15 degrees per wheel click
        REQUIRE(libinput_event_pointer_get_scroll_value(p_event, LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL) == -15.125);
        event = get_event(li); // skipping LIBINPUT_EVENT_POINTER_AXIS
    }
}

TEST_CASE("virtual mouse absolue", "[LIBINPUT]") {
    auto mouse = std::move(*Mouse::create());
    auto li = create_libinput_context({mouse.get_nodes()[1]});
    auto event = get_event(li);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_POINTER));

    auto TARGET_WIDTH = 1920;
    auto TARGET_HEIGHT = 1080;
    {
        mouse.move_abs(100, 100, TARGET_WIDTH, TARGET_HEIGHT);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_MOTION_ABSOLUTE);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE_THAT(libinput_event_pointer_get_absolute_y_transformed(p_event, TARGET_HEIGHT), WithinRel(98.f, 0.5f));
        REQUIRE_THAT(libinput_event_pointer_get_absolute_x_transformed(p_event, TARGET_WIDTH), WithinRel(99.f, 0.5f));
    }

    { // Testing outside bounds
        mouse.move_abs(TARGET_WIDTH + 100, TARGET_HEIGHT + 100, TARGET_WIDTH, TARGET_HEIGHT);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_MOTION_ABSOLUTE);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE_THAT(libinput_event_pointer_get_absolute_y_transformed(p_event, TARGET_HEIGHT),
                     WithinRel(TARGET_HEIGHT, 0.5f));
        REQUIRE_THAT(libinput_event_pointer_get_absolute_x_transformed(p_event, TARGET_WIDTH),
                     WithinRel(TARGET_WIDTH, 0.5f));
    }

    {// Testing non 16:9 aspect ratio (PR-9)
      TARGET_WIDTH = 19200;
      TARGET_HEIGHT = 10800;
      mouse.move_abs(TARGET_WIDTH, TARGET_HEIGHT, TARGET_WIDTH, TARGET_HEIGHT);
      event = get_event(li);
      REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_MOTION_ABSOLUTE);
      auto p_event = libinput_event_get_pointer_event(event.get());
      REQUIRE_THAT(libinput_event_pointer_get_absolute_y_transformed(p_event, TARGET_HEIGHT),
                   WithinRel(TARGET_HEIGHT, 0.001f));
      REQUIRE_THAT(libinput_event_pointer_get_absolute_x_transformed(p_event, TARGET_WIDTH),
                   WithinRel(TARGET_WIDTH, 0.001f));
    }

    {
        mouse.press(Mouse::RIGHT);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_BUTTON);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_button(p_event) == BTN_RIGHT);
        REQUIRE(libinput_event_pointer_get_button_state(p_event) == LIBINPUT_BUTTON_STATE_PRESSED);
    }

    {
        mouse.release(Mouse::RIGHT);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_BUTTON);
        auto p_event = libinput_event_get_pointer_event(event.get());
        REQUIRE(libinput_event_pointer_get_button(p_event) == BTN_RIGHT);
        REQUIRE(libinput_event_pointer_get_button_state(p_event) == LIBINPUT_BUTTON_STATE_RELEASED);
    }

    {
        mouse.vertical_scroll(121);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL);
        auto p_event = libinput_event_get_pointer_event(event.get());
        // REL_WHEEL positive is up in evdev; libinput reports vertical scroll with the opposite sign.
        REQUIRE(libinput_event_pointer_get_scroll_value_v120(p_event, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) == -121);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_AXIS);
    }
}

TEST_CASE("virtual mouse switches back to relative device", "[LIBINPUT]") {
    auto mouse = std::move(*Mouse::create());
    auto rel = create_libinput_context({mouse.get_nodes()[0]});
    auto abs = create_libinput_context({mouse.get_nodes()[1]});

    auto event = get_event(rel);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    event = get_event(abs);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);

    mouse.move_abs(100, 100, 1920, 1080);
    event = get_event(abs);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_MOTION_ABSOLUTE);

    mouse.move(100, 100);
    event = get_event(rel);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_MOTION);
    auto p_event = libinput_event_get_pointer_event(event.get());
    REQUIRE(libinput_event_pointer_get_dx_unaccelerated(p_event) == 100);
    REQUIRE(libinput_event_pointer_get_dy_unaccelerated(p_event) == 100);

    mouse.vertical_scroll(121);
    event = get_event(rel);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL);
    p_event = libinput_event_get_pointer_event(event.get());
    REQUIRE(libinput_event_pointer_get_scroll_value_v120(p_event, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) == -121);
    event = get_event(rel);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_POINTER_AXIS);
}

TEST_CASE("virtual touch screen", "[LIBINPUT]") {
    auto touch = std::move(*TouchScreen::create());
    auto li = create_libinput_context(touch.get_nodes());
    auto event = get_event(li);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_TOUCH));

    auto TARGET_WIDTH = 1920;
    auto TARGET_HEIGHT = 1080;
    { // Put down one finger
        touch.place_finger(0, 0.1, 0.1, 0.3, 45);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_DOWN);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 0);
        REQUIRE_THAT(libinput_event_touch_get_x_transformed(t_event, TARGET_WIDTH),
                     WithinRel(TARGET_WIDTH * 0.1f, 0.5f));
        REQUIRE_THAT(libinput_event_touch_get_y_transformed(t_event, TARGET_HEIGHT),
                     WithinRel(TARGET_HEIGHT * 0.1f, 0.5f));
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }

    { // Add a second finger
        touch.place_finger(1, 0.2, 0.2, 0.3, -45);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_DOWN);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 1);
        REQUIRE_THAT(libinput_event_touch_get_x_transformed(t_event, TARGET_WIDTH),
                     WithinRel(TARGET_WIDTH * 0.2f, 0.5f));
        REQUIRE_THAT(libinput_event_touch_get_y_transformed(t_event, TARGET_HEIGHT),
                     WithinRel(TARGET_HEIGHT * 0.2f, 0.5f));
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }

    { // Lift first finger
        touch.release_finger(0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_UP);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }

    { // Place the first finger again while the second is still down:
      // it must get its own free slot back, not collide with the second finger's slot
        touch.place_finger(0, 0.3, 0.3, 0.3, 0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_DOWN);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 0);
        REQUIRE_THAT(libinput_event_touch_get_x_transformed(t_event, TARGET_WIDTH),
                     WithinRel(TARGET_WIDTH * 0.3f, 0.5f));
        REQUIRE_THAT(libinput_event_touch_get_y_transformed(t_event, TARGET_HEIGHT),
                     WithinRel(TARGET_HEIGHT * 0.3f, 0.5f));
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }

    { // Move the second finger: it must still be tracked in its own slot
        touch.place_finger(1, 0.25, 0.25, 0.3, 0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_MOTION);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 1);
        REQUIRE_THAT(libinput_event_touch_get_x_transformed(t_event, TARGET_WIDTH),
                     WithinRel(TARGET_WIDTH * 0.25f, 0.5f));
        REQUIRE_THAT(libinput_event_touch_get_y_transformed(t_event, TARGET_HEIGHT),
                     WithinRel(TARGET_HEIGHT * 0.25f, 0.5f));
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }

    { // Lift first finger again
        touch.release_finger(0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_UP);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }

    { // Lift second finger
        touch.release_finger(1);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_UP);
        auto t_event = libinput_event_get_touch_event(event.get());
        REQUIRE(libinput_event_touch_get_slot(t_event) == 1);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TOUCH_FRAME);
    }
}

TEST_CASE("virtual trackpad", "[LIBINPUT]") {
    auto trackpad = std::move(*Trackpad::create());
    auto li = create_libinput_context(trackpad.get_nodes());
    auto event = get_event(li);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_GESTURE));
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_POINTER));
    libinput_device_config_send_events_set_mode(libinput_event_get_device(event.get()),
                                                LIBINPUT_CONFIG_SEND_EVENTS_ENABLED);

    { // TODO: I can see things happening on the logs but for some fucking reason I can't get the events
        trackpad.place_finger(0, 0.1, 0.1, 0.3, 0);
        event = get_event(li);
        trackpad.place_finger(1, 0.2, 0.2, 0.3, 0);
        event = get_event(li);
        std::this_thread::sleep_for(10ms);

        trackpad.place_finger(0, 0.1, 0.11, 0.3, 0);
        event = get_event(li);
        trackpad.place_finger(1, 0.2, 0.21, 0.3, 0);
        event = get_event(li);
        std::this_thread::sleep_for(10ms);

        trackpad.place_finger(0, 0.1, 0.12, 0.3, 0);
        event = get_event(li);
        trackpad.place_finger(1, 0.2, 0.22, 0.3, 0);
        event = get_event(li);
        std::this_thread::sleep_for(10ms);

        trackpad.place_finger(0, 0.1, 0.13, 0.3, 0);
        event = get_event(li);
        trackpad.place_finger(1, 0.2, 0.23, 0.3, 0);
        event = get_event(li);
        std::this_thread::sleep_for(10ms);

        trackpad.release_finger(0);
        event = get_event(li);
        trackpad.release_finger(1);
        event = get_event(li);
    }
}

TEST_CASE("virtual pen tablet", "[LIBINPUT]") {
    auto tablet = std::move(*PenTablet::create());
    auto li = create_libinput_context(tablet.get_nodes());
    auto event = get_event(li);
    REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_DEVICE_ADDED);
    REQUIRE(libinput_device_has_capability(libinput_event_get_device(event.get()), LIBINPUT_DEVICE_CAP_TABLET_TOOL));

    float TARGET_W = 1920;
    float TARGET_H = 1080;

    { // Let's move the pen close but not in contact with the tablet
        tablet.place_tool(PenTablet::PEN, 0.1, 0.2, -1, 0.5, 45, 0);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TABLET_TOOL_PROXIMITY);
        auto t_event = libinput_event_get_tablet_tool_event(event.get());
        REQUIRE(libinput_event_tablet_tool_get_proximity_state(t_event) == LIBINPUT_TABLET_TOOL_PROXIMITY_STATE_IN);
        REQUIRE(libinput_tablet_tool_get_type(libinput_event_tablet_tool_get_tool(t_event)) ==
                LIBINPUT_TABLET_TOOL_TYPE_PEN);
        REQUIRE_THAT(libinput_event_tablet_tool_get_distance(t_event), WithinAbs(0.5, 0.01));
        REQUIRE_THAT(libinput_event_tablet_tool_get_pressure(t_event), WithinAbs(0.0, 0.01));
        REQUIRE_THAT(libinput_event_tablet_tool_get_x_transformed(t_event, TARGET_W), WithinRel(TARGET_W * 0.1f, 0.5f));
        REQUIRE_THAT(libinput_event_tablet_tool_get_y_transformed(t_event, TARGET_H), WithinRel(TARGET_H * 0.2f, 0.5f));
        REQUIRE_THAT(libinput_event_tablet_tool_get_tilt_x(t_event), WithinRel(45, 0.1f));
        REQUIRE(libinput_event_tablet_tool_get_tilt_y(t_event) == 0);
        REQUIRE(libinput_event_tablet_tool_get_tip_state(t_event) == LIBINPUT_TABLET_TOOL_TIP_UP);
    }

    { // Let's put the pen in contact with the tablet
        tablet.place_tool(PenTablet::PEN, 0.1, 0.2, 0.5, -1.0, 45, 25);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TABLET_TOOL_TIP);
        auto t_event = libinput_event_get_tablet_tool_event(event.get());
        REQUIRE(libinput_event_tablet_tool_get_proximity_state(t_event) == LIBINPUT_TABLET_TOOL_PROXIMITY_STATE_IN);
        REQUIRE(libinput_tablet_tool_get_type(libinput_event_tablet_tool_get_tool(t_event)) ==
                LIBINPUT_TABLET_TOOL_TYPE_PEN);
        REQUIRE(libinput_event_tablet_tool_get_distance(t_event) == 0.0);
        REQUIRE_THAT(libinput_event_tablet_tool_get_pressure(t_event), WithinRel(0.5f, 0.5f));
        REQUIRE_THAT(libinput_event_tablet_tool_get_x_transformed(t_event, TARGET_W), WithinRel(TARGET_W * 0.1f, 0.5f));
        REQUIRE_THAT(libinput_event_tablet_tool_get_y_transformed(t_event, TARGET_H), WithinRel(TARGET_H * 0.2f, 0.5f));
        REQUIRE_THAT(libinput_event_tablet_tool_get_tilt_x(t_event), WithinRel(45, 0.1f));
        REQUIRE_THAT(libinput_event_tablet_tool_get_tilt_y(t_event), WithinRel(25, 0.1f));
        REQUIRE(libinput_event_tablet_tool_get_tip_state(t_event) == LIBINPUT_TABLET_TOOL_TIP_DOWN);
    }

    { // Try removing the pen by setting the distance to 1.0
      tablet.place_tool(PenTablet::PEN, 0.1, 0.2, -1, 1.0, 45, 25);
      event = get_event(li);
      std::cout << libinput_event_get_type(event.get()) << std::endl;
      REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TABLET_TOOL_TIP);
      auto t_event = libinput_event_get_tablet_tool_event(event.get());
      REQUIRE(libinput_event_tablet_tool_get_proximity_state(t_event) == LIBINPUT_TABLET_TOOL_PROXIMITY_STATE_IN);
      REQUIRE(libinput_tablet_tool_get_type(libinput_event_tablet_tool_get_tool(t_event)) ==
              LIBINPUT_TABLET_TOOL_TYPE_PEN);
      REQUIRE_THAT(libinput_event_tablet_tool_get_distance(t_event), WithinAbs(1.0, 0.01));
      REQUIRE_THAT(libinput_event_tablet_tool_get_pressure(t_event), WithinAbs(0.0, 0.01));
      REQUIRE(libinput_event_tablet_tool_get_tip_state(t_event) == LIBINPUT_TABLET_TOOL_TIP_UP);
    }

    { // Test out pressing a button on the tool
        tablet.set_btn(PenTablet::PRIMARY, true);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TABLET_TOOL_BUTTON);
        auto t_event = libinput_event_get_tablet_tool_event(event.get());
        REQUIRE(libinput_event_tablet_tool_get_button(t_event) == BTN_STYLUS);
        REQUIRE(libinput_event_tablet_tool_get_button_state(t_event) == LIBINPUT_BUTTON_STATE_PRESSED);
    }

    { // Test out releasing a button on the tool
        tablet.set_btn(PenTablet::PRIMARY, false);
        event = get_event(li);
        REQUIRE(libinput_event_get_type(event.get()) == LIBINPUT_EVENT_TABLET_TOOL_BUTTON);
        auto t_event = libinput_event_get_tablet_tool_event(event.get());
        REQUIRE(libinput_event_tablet_tool_get_button(t_event) == BTN_STYLUS);
        REQUIRE(libinput_event_tablet_tool_get_button_state(t_event) == LIBINPUT_BUTTON_STATE_RELEASED);
    }
}
