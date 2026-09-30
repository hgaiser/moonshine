#pragma once

#include <cstdint>
#include <inputtino/input.hpp>
#include <uhid/ps5.hpp>

namespace uhid {
constexpr bool is_dualsense_edge(uint16_t vendor, uint16_t product) {
  return vendor == 0x054c && product == 0x0df2;
}

// SDL2 -> Moonlight/inputtino -> DualSense buttons[2] (USB byte 10, BT byte 11).
// Right rear -> PADDLE1 -> bit 7; left rear -> PADDLE2 -> bit 6;
// right Fn -> PADDLE3 -> bit 5; left Fn -> PADDLE4 -> bit 4.
// Source: SDL2 SDL_gamecontroller.c HIDAPI mapping and Linux hid-playstation.c.
constexpr uint8_t edge_buttons(unsigned int pressed, bool is_edge) {
  return is_edge ?
      ((pressed & inputtino::Joypad::PADDLE1_FLAG ? EDGE_RIGHT_PADDLE : 0) |
       (pressed & inputtino::Joypad::PADDLE2_FLAG ? EDGE_LEFT_PADDLE : 0) |
       (pressed & inputtino::Joypad::PADDLE3_FLAG ? EDGE_RIGHT_FN : 0) |
       (pressed & inputtino::Joypad::PADDLE4_FLAG ? EDGE_LEFT_FN : 0)) : 0;
}
}
