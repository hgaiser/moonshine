/**
 * Adapted from https://gist.github.com/timepp/1f678e200d9e0f2a043a9ec6b3690635
 * uses constexpr to generate a CRC32 table at compile time
 */
#pragma once

#include <array>
#include <cstdint>

static constexpr auto generate_table(std::uint32_t polynomial = 0xEDB88320) {
  std::array<std::uint32_t, 256> table{};
  for (std::uint32_t i = 0; i < 256; i++) {
    std::uint32_t c = i;
    for (std::size_t j = 0; j < 8; j++) {
      if (c & 1) {
        c = polynomial ^ (c >> 1);
      } else {
        c >>= 1;
      }
    }
    table[i] = c;
  }
  return table;
}

// Static lookup table generated at compile time
static constexpr auto lookup_table = generate_table();

/**
 * Calculate the CRC32 of a buffer
 */
static constexpr uint32_t CRC32(const unsigned char *buffer, uint32_t length, uint32_t seed = 0) {
  uint32_t c = seed ^ 0xFFFFFFFF;
  for (size_t i = 0; i < length; ++i) {
    c = lookup_table[(c ^ buffer[i]) & 0xFF] ^ (c >> 8);
  }
  return c ^ 0xFFFFFFFF;
}