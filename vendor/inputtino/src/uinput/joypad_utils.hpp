#pragma once
#include <algorithm>
#include <chrono>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <inputtino/input.hpp>
#include <inputtino/protected_types.hpp>
#include <iostream>
#include <linux/input.h>
#include <linux/uinput.h>
#include <optional>
#include <poll.h>
#include <thread>

namespace inputtino {

using namespace std::chrono_literals;

constexpr long MAX_GAIN = 0xFFFF;
constexpr int RUMBLE_POLL_TIMEOUT = 500; // ms

/**
 * Joypads will also have one `/dev/input/js*` device as child, we want to expose that as well
 */
static std::vector<std::string> get_child_dev_nodes(libevdev_uinput *device) {
  std::vector<std::string> result;

  auto dev_path = libevdev_uinput_get_devnode(device);
  if (dev_path) {
    result.push_back(dev_path);
  }

  auto sys_path = libevdev_uinput_get_syspath(device);
  if (sys_path) {
    for (const auto &entry : std::filesystem::directory_iterator(sys_path)) {
      if (entry.is_directory() && entry.path().filename().string().rfind("js", 0) == 0) { // starts with "js"?
        result.push_back("/dev/input/" + entry.path().filename().string());
      }
    }
  }

  return result;
}

struct ActiveRumbleEffect {
  std::chrono::steady_clock::time_point start_point;
  std::chrono::steady_clock::time_point end_point;
  std::chrono::milliseconds length;
  std::chrono::milliseconds delay;
  ff_envelope envelope;

  struct {
    long weak, strong;
  } start;

  struct {
    long weak, strong;
  } end;
};

static long rumble_magnitude(std::chrono::milliseconds time_left,
                             std::uint32_t start,
                             std::uint32_t end,
                             std::chrono::milliseconds length) {
  auto rel = end - start;
  return start + (rel * time_left.count() / length.count());
}

static std::pair<long, long> simulate_rumble(const ActiveRumbleEffect &effect,
                                             const std::chrono::steady_clock::time_point &now) {
  if (effect.end_point < now || now < effect.start_point) {
    return {0, 0};
  }

  auto time_left = std::chrono::duration_cast<std::chrono::milliseconds>(effect.end_point - now);
  auto t = effect.length - time_left;

  auto weak = rumble_magnitude(t, effect.start.weak, effect.end.weak, effect.length);
  auto strong = rumble_magnitude(t, effect.start.strong, effect.end.strong, effect.length);

  if (t.count() < effect.envelope.attack_length) {
    weak = (effect.envelope.attack_level * t.count() + weak * (effect.envelope.attack_length - t.count())) /
           effect.envelope.attack_length;
    strong = (effect.envelope.attack_level * t.count() + strong * (effect.envelope.attack_length - t.count())) /
             effect.envelope.attack_length;
  } else if (time_left.count() < effect.envelope.fade_length) {
    auto dt = (t - effect.length).count() + effect.envelope.fade_length;

    weak = (effect.envelope.fade_level * dt + weak * (effect.envelope.fade_length - dt)) / effect.envelope.fade_length;
    strong = (effect.envelope.fade_level * dt + strong * (effect.envelope.fade_length - dt)) /
             effect.envelope.fade_length;
  }

  return {weak, strong};
}

static ActiveRumbleEffect create_rumble_effect(const ff_effect &effect) {
  // All duration values are expressed in ms. Values above 32767 ms (0x7fff) should not be used
  ActiveRumbleEffect r_effect{
      .start_point = std::chrono::steady_clock::time_point::min(),
      .end_point = std::chrono::steady_clock::time_point::min(),
      .length = std::chrono::milliseconds{std::clamp(effect.replay.length, (std::uint16_t)0, (std::uint16_t)32767)},
      .delay = std::chrono::milliseconds{std::clamp(effect.replay.delay, (std::uint16_t)0, (std::uint16_t)32767)},
      .envelope = {}};
  switch (effect.type) {
  case FF_CONSTANT:
    r_effect.start.weak = effect.u.constant.level;
    r_effect.start.strong = effect.u.constant.level;
    r_effect.end.weak = effect.u.constant.level;
    r_effect.end.strong = effect.u.constant.level;
    r_effect.envelope = effect.u.constant.envelope;
    break;
  case FF_PERIODIC:
    r_effect.start.weak = effect.u.periodic.magnitude;
    r_effect.start.strong = effect.u.periodic.magnitude;
    r_effect.end.weak = effect.u.periodic.magnitude;
    r_effect.end.strong = effect.u.periodic.magnitude;
    r_effect.envelope = effect.u.periodic.envelope;
    break;
  case FF_RAMP:
    r_effect.start.weak = effect.u.ramp.start_level;
    r_effect.start.strong = effect.u.ramp.start_level;
    r_effect.end.weak = effect.u.ramp.end_level;
    r_effect.end.strong = effect.u.ramp.end_level;
    r_effect.envelope = effect.u.ramp.envelope;
    break;
  case FF_RUMBLE:
    r_effect.start.weak = effect.u.rumble.weak_magnitude;
    r_effect.start.strong = effect.u.rumble.strong_magnitude;
    r_effect.end.weak = effect.u.rumble.weak_magnitude;
    r_effect.end.strong = effect.u.rumble.strong_magnitude;
    break;
  }
  return r_effect;
}

/**
 * Here we listen for events from the device and call the corresponding callback functions
 *
 * Rumble:
 *   First of, this is called force feedback (FF) in linux,
 *   you can see some docs here: https://www.kernel.org/doc/html/latest/input/ff.html
 *   In uinput this works as a two step process:
 *    - you first upload the FF effect with a given request ID
 *    - later on when the rumble has been activated you'll receive an EV_FF in your /dev/input/event**
 *      where the value is the request ID
 *   You can test the virtual devices that we create by simply using the utility `fftest`
 */
static void event_listener(const std::shared_ptr<BaseJoypadState> &state) {
  std::this_thread::sleep_for(100ms); // We have to sleep in order to be able to read from the newly created device

  auto uinput_fd = libevdev_uinput_get_fd(state->joy.get());
  if (uinput_fd < 0) {
    std::cerr << "Unable to open uinput device, additional events will be disabled.";
    return;
  }

  // We have to add 0_NONBLOCK to the flags in order to be able to read the events
  int flags = fcntl(uinput_fd, F_GETFL, 0);
  fcntl(uinput_fd, F_SETFL, flags | O_NONBLOCK);

  /* Local copy of all the uploaded ff effects */
  std::map<int, ActiveRumbleEffect> ff_effects = {};
  std::pair<std::uint32_t, std::uint32_t> prev_rumble = {0, 0};

  /* This can only be set globally when receiving FF_GAIN */
  unsigned int current_gain = MAX_GAIN;

  std::array<pollfd, 1> pfds = {pollfd{.fd = uinput_fd, .events = POLLIN}};
  int poll_rs = 0;

  while (!state->stop_listening_events) {
    poll_rs = poll(pfds.data(), pfds.size(), RUMBLE_POLL_TIMEOUT);
    if (poll_rs < 0) {
      std::cerr << "Failed polling uinput fd; ret=" << strerror(errno);
      return;
    }

    auto events = fetch_events(uinput_fd);
    for (auto ev : events) {
      if (ev->type == EV_UINPUT && ev->code == UI_FF_UPLOAD) { // Upload a new FF effect
        uinput_ff_upload upload{};
        upload.request_id = ev->value;

        ioctl(uinput_fd, UI_BEGIN_FF_UPLOAD, &upload); // retrieve the effect

        auto new_effect = create_rumble_effect(upload.effect);
        if (ff_effects.find(upload.effect.id) != ff_effects.end()) {
          // We have to keep the original start and end points of the effect
          new_effect.start_point = ff_effects[upload.effect.id].start_point;
          new_effect.end_point = ff_effects[upload.effect.id].end_point;
        }
        ff_effects.insert_or_assign(upload.effect.id, create_rumble_effect(upload.effect));
        upload.retval = 0;

        ioctl(uinput_fd, UI_END_FF_UPLOAD, &upload);
      } else if (ev->type == EV_UINPUT && ev->code == UI_FF_ERASE) { // Remove an uploaded FF effect
        uinput_ff_erase erase{};
        erase.request_id = ev->value;

        ioctl(uinput_fd, UI_BEGIN_FF_ERASE, &erase); // retrieve ff_erase

        ff_effects.erase(erase.effect_id);
        erase.retval = 0;

        ioctl(uinput_fd, UI_END_FF_ERASE, &erase);
      } else if (ev->type == EV_FF && ev->code == FF_GAIN) { // Force feedback set gain
        current_gain = std::clamp((long)ev->value, 0l, MAX_GAIN);
      } else if (ev->type == EV_FF) { // Force feedback effect
        auto effect_id = ev->code;
        if (auto effect = ff_effects.find(effect_id); effect != ff_effects.end()) {
          if (ev->value) { // Activate
            auto now = std::chrono::steady_clock::now();
            effect->second.start_point = now + effect->second.delay;
            effect->second.end_point = now + effect->second.delay + effect->second.length;
          } else { // Deactivate
            effect->second.end_point = std::chrono::steady_clock::time_point::min();
          }
        }
      }
    }

    auto now = std::chrono::steady_clock::now();

    // Accumulate all rumble effects
    std::pair<long, long> current_rumble = {0, 0};
    for (auto &[_id, effect] : ff_effects) {
      auto [weak, strong] = simulate_rumble(effect, now);
      current_rumble.first += weak;
      current_rumble.second += strong;
    }
    // Avoid sending too many events
    if (prev_rumble.first != current_rumble.first || prev_rumble.second != current_rumble.second) {
      prev_rumble.first = current_rumble.first;
      prev_rumble.second = current_rumble.second;

      if (auto callback = state->on_rumble) {
        callback.value()(static_cast<int>((current_rumble.second * current_gain / MAX_GAIN)),
                         static_cast<int>((current_rumble.first * current_gain / MAX_GAIN)));
      }
    }
  }
}

} // namespace inputtino
