#ifdef NDEBUG
#undef NDEBUG
#endif
#include <uhid/dualsense_edge.hpp>
#include <cassert>
#include <cstddef>

int main() {
  using inputtino::Joypad;
  static_assert(sizeof(uhid::dualsense_input_report) == 63);
  static_assert(offsetof(uhid::dualsense_input_report, buttons) == 7);
  static_assert(sizeof(uhid::dualsense_input_report_bt_header) == 2);
  static_assert(sizeof(uhid::dualsense_input_report_usb_header) == 1);
  assert(uhid::is_dualsense_edge(0x054c, 0x0df2));
  assert(!uhid::is_dualsense_edge(0x054c, 0x0ce6));
  assert(!uhid::is_dualsense_edge(0x045e, 0x0df2));
  const unsigned flags[] = {Joypad::PADDLE1_FLAG, Joypad::PADDLE2_FLAG,
                            Joypad::PADDLE3_FLAG, Joypad::PADDLE4_FLAG};
  const unsigned bits[] = {0x80, 0x40, 0x20, 0x10};
  // All combinations prove independence, including simultaneous press/release.
  for (unsigned combination = 0; combination < 16; ++combination) {
    unsigned pressed = 0, expected = 0;
    for (unsigned i = 0; i < 4; ++i) {
      if (combination & (1u << i)) { pressed |= flags[i]; expected |= bits[i]; }
    }
    assert(uhid::edge_buttons(pressed, true) == expected);
    assert(uhid::edge_buttons(pressed, false) == 0);
    assert(uhid::edge_buttons(pressed | Joypad::A | Joypad::TOUCHPAD_FLAG | Joypad::MISC_FLAG, true) == expected);
    assert(uhid::edge_buttons(0, true) == 0);
  }
}
